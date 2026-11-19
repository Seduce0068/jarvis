//! In-memory approval workflow.
//!
//! A request registers a pending entry and waits until a decision is made
//! (via the HTTP approve/deny endpoints) or the configured timeout elapses.
//! Decisions are held in memory; an opt-in PostgreSQL backend
//! ([`crate::persistence::PgApprovalPersistence`]) can durably CAS the decision
//! and write the audit row in one transaction when `pg_url` is configured.
//! On startup, [`ApprovalStore::reconcile_pending`] loads undecided durable
//! rows: live holds are re-registered without a waiter and expired holds are
//! settled with an audit record, so stale approvals never linger in the UI.
//! Recovery honours `max_pending`; the surplus is settled as `Expired` with an
//! audit row rather than silently exceeding the cap. Recovered holds have no
//! timer of their own: the lazy `prune` sweep is their timeout path, and it
//! commits [`ApprovalPersistence::mark_expired`] before evicting
//! an expired entry so no durable row is left undecided and unaudited.
//! User notification (HA/phone) will be layered on top later.
//!
//! Approval is intentionally HTTP-only: the gRPC surface is limited to
//! authorization and kill-switch operations so a long-running human decision
//! never occupies a gRPC stream.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::{RwLock, watch};
use uuid::Uuid;

use crate::policy::{ApprovalDecision, ApprovalRequest};

#[derive(Debug, thiserror::Error)]
pub enum ApprovalError {
    #[error("approval request not found: {0}")]
    NotFound(Uuid),

    #[error("approval request already decided: {0}")]
    AlreadyDecided(Uuid),

    #[error("kill switch is activated; approval decisions are blocked")]
    KillSwitchActivated,

    /// The in-memory queue is at `max_pending`. This is an infrastructure
    /// exhaustion condition, not a policy decision: callers may retry later
    /// instead of reporting a user/operator denial to the requester.
    #[error("approval queue is full; the request was not enqueued")]
    QueueFull,

    /// A durable persistence backend operation failed (e.g. a database error).
    ///
    /// Like [`Self::QueueFull`] this is a transient infrastructure condition
    /// that callers may retry, but it is a distinct variant so a client can tell
    /// queue exhaustion from a persistence outage. It carries no payload: the
    /// message is fixed and safe to log or render on the wire, with no URL,
    /// path, SQL text or upstream error string.
    #[error("approval persistence is unavailable; the request was not completed")]
    Persistence,
}

impl ApprovalError {
    /// Fixed-vocabulary classification for logs and metrics.
    ///
    /// The returned literal is derived only from the variant, never from its
    /// payload: no request id, database URL, filesystem path, SQL text or
    /// upstream error string can reach a log record through this method. The
    /// match is exhaustive so a future variant must be classified explicitly
    /// instead of silently inheriting a generic category.
    pub(crate) fn category(&self) -> &'static str {
        match self {
            ApprovalError::NotFound(_) => "NotFound",
            ApprovalError::AlreadyDecided(_) => "AlreadyDecided",
            ApprovalError::KillSwitchActivated => "KillSwitchActivated",
            ApprovalError::QueueFull => "QueueFull",
            ApprovalError::Persistence => "Persistence",
        }
    }
}

/// Reason attached to the legacy `Denied` decision returned by
/// [`ApprovalStore::request`] when the queue is exhausted.
///
/// New callers should match [`ApprovalError::QueueFull`] from
/// [`ApprovalStore::try_request`] instead of comparing this string.
pub const QUEUE_FULL_REASON: &str = "approval queue full";

/// Persistence seam for approval requests and decisions (I29-A / I30-A, 案A).
///
/// The in-memory store remains the source of truth for pending state. Two
/// implementations exist: [`NoopPersistence`] (the default, no I/O) and
/// [`crate::persistence::PgApprovalPersistence`] (opt-in via `pg_url`, which
/// commits a CAS plus an audit row in one transaction).
///
/// Implementations must be `Send + Sync` because [`ApprovalStore`] is cloned
/// across Tokio tasks. `record_request` and `mark_expired` errors are logged by
/// [`ApprovalStore`]; a `record_request` failure additionally rolls back the
/// in-memory registration and surfaces a retryable error so an orphan hold can
/// never later produce a zero-row CAS. A backend-reported persistence failure
/// keeps its [`ApprovalError::Persistence`] identity; other backend errors keep
/// the legacy [`ApprovalError::QueueFull`] classification. With
/// [`NoopPersistence`] none of these paths can fail, so the memory-only default
/// is unchanged. For `record_decision`, an error withholds the in-memory
/// decision: the watch channel must not announce a decision that failed to
/// commit.
#[async_trait::async_trait]
pub trait ApprovalPersistence: Send + Sync {
    /// Persist a newly registered pending request with its authoritative
    /// deadline (the same value the store enforces).
    async fn record_request(
        &self,
        request: &ApprovalRequest,
        deadline: DateTime<Utc>,
    ) -> Result<(), ApprovalError>;

    /// Persist the decision reached for `id`.
    ///
    /// For a transactional backend this must return `Ok` only after the commit
    /// succeeded: [`ApprovalStore::decide`] publishes the in-memory decision and
    /// wakes the waiter only after this method returns `Ok`. A failure withholds
    /// both, so a rolled-back transaction is never observable by a waiter.
    async fn record_decision(
        &self,
        id: Uuid,
        decision: &ApprovalDecision,
        decided_at: DateTime<Utc>,
    ) -> Result<(), ApprovalError>;

    /// Load durable rows that are still awaiting a decision.
    ///
    /// Both live holds (`expires_at > now`) and already-expired holds are
    /// returned: [`ApprovalStore::reconcile_pending`] re-registers the former in
    /// memory and settles the latter through [`Self::mark_expired`]. Filtering
    /// expired rows out here would make them invisible to that settlement path.
    /// `list_pending` still reads in-memory state, so its return type and
    /// behavior remain unchanged.
    async fn load_pending(&self) -> Result<Vec<ApprovalRequest>, ApprovalError>;

    /// Mark a pending request as expired at `expired_at`.
    ///
    /// Used by the runtime timeout path, the lazy `prune` sweep (including
    /// recovered holds whose waiter was never restored) and startup
    /// reconciliation. The transactional backend commits the `Expired`
    /// decision and its audit row in one transaction, so a caller may use a
    /// successful return as the signal to drop its in-memory entry.
    async fn mark_expired(&self, id: Uuid, expired_at: DateTime<Utc>) -> Result<(), ApprovalError>;
}

/// Default [`ApprovalPersistence`]: does nothing.
///
/// This is what [`ApprovalStore::new`] and [`ApprovalStore::with_max_pending`]
/// use, so their in-memory-only behavior is exactly unchanged.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopPersistence;

#[async_trait::async_trait]
impl ApprovalPersistence for NoopPersistence {
    async fn record_request(
        &self,
        _request: &ApprovalRequest,
        _deadline: DateTime<Utc>,
    ) -> Result<(), ApprovalError> {
        Ok(())
    }

    async fn record_decision(
        &self,
        _id: Uuid,
        _decision: &ApprovalDecision,
        _decided_at: DateTime<Utc>,
    ) -> Result<(), ApprovalError> {
        Ok(())
    }

    async fn load_pending(&self) -> Result<Vec<ApprovalRequest>, ApprovalError> {
        Ok(Vec::new())
    }

    async fn mark_expired(
        &self,
        _id: Uuid,
        _expired_at: DateTime<Utc>,
    ) -> Result<(), ApprovalError> {
        Ok(())
    }
}

struct Entry {
    request: ApprovalRequest,
    decision: Option<ApprovalDecision>,
    decided_at: Option<DateTime<Utc>>,
    /// The authoritative deadline computed at registration (the min of the
    /// configured timeout and the caller's `expires_at`). Stored so `prune`
    /// can reclaim pending entries whose waiter was abandoned before it ever
    /// reached the timeout branch.
    deadline: DateTime<Utc>,
    /// True while a `decide` call has claimed this entry and its durable
    /// commit is in flight. The write guard is deliberately released across
    /// the commit, so the timeout path checks this flag and yields to an
    /// in-flight decide instead of masking a successful commit with a
    /// memory-only `Expired` marking (I33-A (b)).
    deciding: bool,
    tx: watch::Sender<Option<ApprovalDecision>>,
}

struct Inner {
    timeout: Duration,
    max_pending: usize,
    entries: RwLock<HashMap<Uuid, Entry>>,
    /// Additive seam handle. In slice 1 this is always [`NoopPersistence`].
    persistence: Arc<dyn ApprovalPersistence>,
}

#[derive(Clone)]
pub struct ApprovalStore {
    inner: Arc<Inner>,
}

/// Default cap on simultaneously pending approval requests.
///
/// The HTTP approval endpoint is loopback-only but unauthenticated; without
/// a cap, repeated requests could grow the in-memory queue without bound.
pub const DEFAULT_MAX_PENDING: usize = 100;

impl ApprovalStore {
    pub fn new(timeout: Duration) -> Self {
        Self::with_max_pending(timeout, DEFAULT_MAX_PENDING)
    }

    /// Constructor with an explicit pending-request cap (used by tests and
    /// operators that need a different bound).
    pub fn with_max_pending(timeout: Duration, max_pending: usize) -> Self {
        Self::with_persistence(timeout, max_pending, Arc::new(NoopPersistence))
    }

    /// Additive constructor that installs an [`ApprovalPersistence`] backend.
    ///
    /// The in-memory workflow stays authoritative for pending state; the
    /// backend receives the trait hooks. Passing [`NoopPersistence`] reproduces
    /// the default store exactly with no I/O. [`Self::new`] and
    /// [`Self::with_max_pending`] keep using it, so memory-only deployments are
    /// unchanged. A configured `pg_url` injects the PostgreSQL backend instead.
    pub fn with_persistence(
        timeout: Duration,
        max_pending: usize,
        persistence: Arc<dyn ApprovalPersistence>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                timeout,
                max_pending,
                entries: RwLock::new(HashMap::new()),
                persistence,
            }),
        }
    }

    /// Configured decision window for pending approvals.
    pub fn timeout(&self) -> Duration {
        self.inner.timeout
    }

    /// Latest deadline this store honours for a request registered at `now`.
    ///
    /// This is the authoritative cap used by [`Self::try_request`] and exposed
    /// to transports so the HTTP endpoint can clamp a caller-supplied
    /// `expires_at` to exactly the value the store enforces. Purely additive:
    /// no existing `ApprovalRequest` caller or gRPC contract changes.
    pub fn max_deadline(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        let timeout = chrono::Duration::from_std(self.inner.timeout)
            .unwrap_or_else(|_| chrono::Duration::seconds(300));
        now + timeout
    }

    /// Register `request` and wait for a decision.
    ///
    /// Returns the recorded decision, or [`ApprovalDecision::Expired`] when no
    /// decision arrives within the configured timeout (or the request's own
    /// `expires_at`, whichever comes first).
    ///
    /// Queue exhaustion is mapped here to a legacy
    /// `Denied { reason: QUEUE_FULL_REASON }` decision so existing callers keep
    /// their `ApprovalDecision` return type. Callers that must distinguish
    /// infrastructure exhaustion from a policy deny should use
    /// [`ApprovalStore::try_request`] instead.
    pub async fn request(&self, request: ApprovalRequest) -> ApprovalDecision {
        match self.try_request(request).await {
            Ok(decision) => decision,
            Err(ApprovalError::QueueFull) => ApprovalDecision::Denied {
                reason: QUEUE_FULL_REASON.into(),
            },
            // Other variants predate the queue-full split and were always
            // reported as a denied decision by this legacy wrapper.
            Err(error) => ApprovalDecision::Denied {
                reason: error.to_string(),
            },
        }
    }

    /// Like [`ApprovalStore::request`], but reports queue exhaustion as
    /// [`ApprovalError::QueueFull`] rather than as a `Denied` decision.
    ///
    /// This lets transports such as HTTP answer with 503 (retry later) while
    /// leaving an ordinary policy `Denied` as a normal 200 decision.
    pub async fn try_request(
        &self,
        request: ApprovalRequest,
    ) -> Result<ApprovalDecision, ApprovalError> {
        let id = request.id;
        let now = Utc::now();
        let deadline = if request.expires_at > now {
            std::cmp::min(self.max_deadline(now), request.expires_at)
        } else {
            self.max_deadline(now)
        };

        let (tx, mut rx) = watch::channel(None);
        {
            let mut entries = self.inner.entries.write().await;
            Self::prune(
                &mut entries,
                &*self.inner.persistence,
                self.inner.timeout,
                now,
            )
            .await;
            // A caller-supplied id must not overwrite an existing pending
            // request: doing so would drop the original waiter's channel and
            // let a second caller hijack the operator's decision target.
            if entries.contains_key(&id) {
                return Ok(ApprovalDecision::Denied {
                    reason: "duplicate approval id".into(),
                });
            }
            // Fail closed when the queue is saturated: an attacker on
            // loopback must not be able to grow pending state without limit.
            // An explicit error distinguishes this infrastructure condition
            // from a policy deny (which remains a `Denied` decision).
            let pending = entries
                .values()
                .filter(|entry| entry.decision.is_none())
                .count();
            if pending >= self.inner.max_pending {
                return Err(ApprovalError::QueueFull);
            }
            entries.insert(
                id,
                Entry {
                    request: request.clone(),
                    decision: None,
                    decided_at: None,
                    deadline,
                    deciding: false,
                    tx,
                },
            );
        }

        // Fail-closed on a durable INSERT failure. `NoopPersistence` always
        // returns Ok, so the memory-only default keeps its exact behavior.
        //
        // Choice (documented per R4-PERSIST): on `record_request` error we do
        // BOTH undo the in-memory registration AND return a retryable error
        // (HTTP 503). Leaving a pending entry with no durable row would make a
        // later operator `decide` hit a zero-row CAS and be reported as
        // `AlreadyDecided` (HTTP 409) with an empty audit trail. Rolling back
        // plus a retryable error closes that gap: no orphan hold survives and
        // the requester is asked to retry. This runs before the wait, so no
        // `decide` CAS can race a committed row that does not exist.
        if let Err(error) = self
            .inner
            .persistence
            .record_request(&request, deadline)
            .await
        {
            tracing::warn!(
                error = %error,
                request_id = %id,
                "failed to persist approval request; dropping in-memory entry (fail-closed)"
            );
            // Undo the registration so no orphan hold survives. A concurrent
            // `decide` cannot have published a decision here: for the Pg backend
            // the CAS needs the very row whose INSERT just failed, so `decide`
            // would itself have failed with `AlreadyDecided` without publishing.
            let mut entries = self.inner.entries.write().await;
            entries.remove(&id);
            // Keep the dedicated persistence failure (HTTP 503 with the fixed
            // "persistence" body) so callers can distinguish a DB outage from
            // queue exhaustion. Any other backend error retains the legacy
            // retryable `QueueFull` classification; both stay fail-closed.
            if matches!(error, ApprovalError::Persistence) {
                return Err(error);
            }
            return Err(ApprovalError::QueueFull);
        }

        let remaining = (deadline - Utc::now()).to_std().unwrap_or(Duration::ZERO);
        // Use a cloned receiver so the timeout future does not keep `rx`
        // borrowed: the timeout-recovery loop below must poll `rx.changed()`
        // while waiting for an in-flight `decide` commit to publish.
        let mut wait_rx = rx.clone();
        let wait = async move {
            loop {
                let current = {
                    let guard = wait_rx.borrow_and_update();
                    (*guard).clone()
                };
                if let Some(decision) = current {
                    return decision;
                }
                if wait_rx.changed().await.is_err() {
                    // Sender dropped without a decision; treat as expired.
                    return ApprovalDecision::Expired;
                }
            }
        };

        let decision = if remaining > Duration::ZERO {
            tokio::time::timeout(remaining, wait).await.ok()
        } else {
            None
        };

        let decision = match decision {
            Some(decision) => decision,
            None => {
                // The wait elapsed. A `decide` may have committed durably but
                // not yet published its result; yield to that in-flight commit
                // instead of racing it with a memory-only `Expired` marking
                // (I33-A (b)).
                enum TimeoutOutcome {
                    Decided(ApprovalDecision),
                    Expire,
                    YieldToDecide,
                }
                // Bound the yields: a `decide` task cancelled mid-commit could
                // otherwise leave `deciding` set forever and hang this waiter.
                let mut yields = 0usize;
                loop {
                    let expired_at = Utc::now();
                    let outcome = {
                        let mut entries = self.inner.entries.write().await;
                        match entries.get_mut(&id) {
                            None => TimeoutOutcome::Decided(ApprovalDecision::Expired),
                            Some(entry) => {
                                if let Some(decision) = &entry.decision {
                                    TimeoutOutcome::Decided(decision.clone())
                                } else if entry.deciding && yields < 100 {
                                    TimeoutOutcome::YieldToDecide
                                } else {
                                    if entry.deciding {
                                        entry.deciding = false;
                                        tracing::warn!(
                                            request_id = %id,
                                            "stale decide claim cleared after repeated yields"
                                        );
                                    }
                                    entry.decision = Some(ApprovalDecision::Expired);
                                    entry.decided_at = Some(expired_at);
                                    TimeoutOutcome::Expire
                                }
                            }
                        }
                    };
                    match outcome {
                        TimeoutOutcome::Decided(decision) => return Ok(decision),
                        TimeoutOutcome::YieldToDecide => {
                            // `decide` publishes on the watch channel once its
                            // commit finishes; the notification wakes this loop
                            // so it returns the committed decision, not Expired.
                            yields += 1;
                            let grace = Duration::from_millis(100);
                            let _ = tokio::time::timeout(grace, rx.changed()).await;
                            continue;
                        }
                        TimeoutOutcome::Expire => {
                            // `NoopPersistence` never errors, so the default
                            // path is unchanged. A failed audit write is
                            // retried by the next `prune` (or startup
                            // reconciliation), so no decision/audit is lost.
                            let mark = self.inner.persistence.mark_expired(id, expired_at).await;
                            match mark {
                                // A concurrent settlement won the durable CAS.
                                // The row may already carry the real decision
                                // even though this waiter never observed the
                                // watch notification (lost-ack). Give the
                                // winner one bounded chance to publish before
                                // falling back to a memory-only `Expired`
                                // (I36-C (3)). A full fix also has to re-read
                                // the durable decision, which needs a new seam
                                // on `ApprovalPersistence` (I36-B).
                                Err(ApprovalError::AlreadyDecided(_)) => {
                                    let grace = Duration::from_millis(100);
                                    let changed =
                                        tokio::time::timeout(grace, rx.changed()).await.is_ok();
                                    let published = if changed {
                                        let guard = rx.borrow_and_update();
                                        (*guard).clone()
                                    } else {
                                        None
                                    };
                                    if let Some(decision) = published {
                                        // Reconcile the provisional `Expired`
                                        // written above with the decision the
                                        // CAS winner committed, so memory, the
                                        // waiter and the durable row agree.
                                        let mut entries = self.inner.entries.write().await;
                                        if let Some(entry) = entries.get_mut(&id)
                                            && matches!(
                                                entry.decision.as_ref(),
                                                Some(&ApprovalDecision::Expired)
                                            )
                                        {
                                            entry.decision = Some(decision.clone());
                                            entry.decided_at = Some(expired_at);
                                        }
                                        return Ok(decision);
                                    }
                                    tracing::warn!(
                                        request_id = %id,
                                        "expiry CAS lost; waiter has no durable re-read (I36-B)"
                                    );
                                    return Ok(ApprovalDecision::Expired);
                                }
                                Err(error) => {
                                    tracing::warn!(
                                        error_category = error.category(),
                                        request_id = %id,
                                        "failed to persist approval expiration; audit row may be missing"
                                    );
                                    return Ok(ApprovalDecision::Expired);
                                }
                                Ok(()) => return Ok(ApprovalDecision::Expired),
                            }
                        }
                    }
                }
            }
        };
        Ok(decision)
    }

    /// Record `decision` for a pending request and wake its waiter.
    ///
    /// Ordering (I30-A, tightened by I33-A (b)): the durable backend commits
    /// first and its result is the authority; the in-memory entry is then
    /// overwritten to match. A short write guard claims the entry (`deciding`)
    /// before the commit so the timeout path can yield to an in-flight commit
    /// instead of racing it with a memory-only `Expired` marking. The guard is
    /// deliberately *not* held across the commit: `status()` and
    /// `watch_does_not_fire_before_commit` keep observing `Pending` until the
    /// commit succeeds, and a rollback publishes nothing.
    ///
    /// A request whose deadline has already passed is explicitly rejected
    /// (expiry is a terminal decision, reported as `AlreadyDecided` with a
    /// warning) and never reaches the persistence backend. With
    /// [`NoopPersistence`] the commit is immediate, so memory-only behavior is
    /// unchanged.
    pub async fn decide(&self, id: Uuid, decision: ApprovalDecision) -> Result<(), ApprovalError> {
        let decided_at = Utc::now();

        // Precondition + claim in one short write critical section: no `await`
        // while held, and a second `decide` fails closed without touching the
        // persistence backend.
        {
            let mut entries = self.inner.entries.write().await;
            let entry = entries.get_mut(&id).ok_or(ApprovalError::NotFound(id))?;
            if decided_at > entry.deadline {
                tracing::warn!(
                    request_id = %id,
                    "decide rejected: the decision window has elapsed"
                );
                return Err(ApprovalError::AlreadyDecided(id));
            }
            if entry.decision.is_some() || entry.deciding {
                return Err(ApprovalError::AlreadyDecided(id));
            }
            entry.deciding = true;
        }

        // Persist (and, for the Pg backend, commit) BEFORE publishing. The
        // guard is released here on purpose; the `deciding` flag above is what
        // serializes this path against the timeout path.
        let commit = self
            .inner
            .persistence
            .record_decision(id, &decision, decided_at)
            .await;

        {
            let mut entries = self.inner.entries.write().await;
            let entry = entries.get_mut(&id).ok_or(ApprovalError::NotFound(id))?;
            entry.deciding = false;
            // A failed commit must leave the entry pending so no waiter can
            // observe a rolled-back decision.
            commit?;
            // The commit is the authority: overwrite any provisional state so
            // a commit that won is never reported back as a 409.
            entry.decision = Some(decision.clone());
            entry.decided_at = Some(decided_at);
            let _ = entry.tx.send(Some(decision));
        }
        Ok(())
    }

    /// Current decision for `id`; `Pending` while still awaiting one.
    pub async fn status(&self, id: Uuid) -> Result<ApprovalDecision, ApprovalError> {
        let entries = self.inner.entries.read().await;
        let entry = entries.get(&id).ok_or(ApprovalError::NotFound(id))?;
        Ok(entry.decision.clone().unwrap_or(ApprovalDecision::Pending))
    }

    /// Requests still awaiting a decision within their deadline.
    ///
    /// `prune()` reclaims abandoned pending entries only when a new request is
    /// registered, so a read in between can still see entries whose waiter was
    /// cancelled and whose `deadline` has passed. They can no longer produce a
    /// real decision, so hide them here as well. This is the list consumed by
    /// `PolicyEngine::pending_approvals()` and the `/metrics` pending gauge,
    /// which would otherwise over-report outstanding approvals.
    pub async fn list_pending(&self) -> Vec<ApprovalRequest> {
        let entries = self.inner.entries.read().await;
        let now = Utc::now();
        entries
            .values()
            .filter(|entry| entry.decision.is_none() && now <= entry.deadline)
            .map(|entry| entry.request.clone())
            .collect()
    }

    /// Restore durable, still-live pending holds at startup (I31-A).
    ///
    /// [`ApprovalPersistence::load_pending`] returns every row whose
    /// `decided_at` is NULL, including holds whose deadline has already passed
    /// (they must be reachable by the settlement path below). A row with
    /// `expires_at > now` is re-inserted as an [`Entry`] with a watch sender but
    /// **no receiver**: an operator can still decide it, but there is no
    /// original waiter to wake. The durable `expires_at` is reused verbatim as
    /// the deadline so recovery never extends a hold.
    ///
    /// A row whose deadline has passed is settled through
    /// [`ApprovalPersistence::mark_expired`] instead of being re-registered.
    /// For the PostgreSQL backend that commits an `Expired` decision plus its
    /// audit row atomically, so a stale hold cannot resurface in the UI (or the
    /// `/metrics` pending gauge) after a restart.
    ///
    /// Recovery honours `max_pending` (I33-A (c)): only as many live rows as
    /// there are free slots are re-registered. The chosen policy for the
    /// surplus is **fail-closed at the audit boundary** — each excess live row
    /// is settled via [`ApprovalPersistence::mark_expired`] (an `Expired`
    /// decision plus its audit row in one transaction) instead of being loaded
    /// past the cap. That keeps the queue bounded without aborting startup on a
    /// backlog: an operator cannot later approve a hold that never got a slot.
    /// Recovered, expired and over-limit counts are emitted in one `INFO` log
    /// line so a backlog is visible after every restart.
    ///
    /// Expired-row settlement is best-effort per row (I34-E): a failed
    /// [`ApprovalPersistence::mark_expired`] no longer short-circuits the loop,
    /// so every remaining expired row is still settled. `AlreadyDecided` counts
    /// as success (the row was already settled by a concurrent decision or
    /// expiry). Other failures are reported as a structured warning carrying
    /// only the request id and fixed message text — no URL, SQL or upstream
    /// error string — and the first failure is returned only after the loop.
    ///
    /// Startup policy: reconciliation still fails closed if any expired row
    /// could not be settled, so an undecided, unaudited durable row is never
    /// silently ignored by `PolicyEngine::new()`. Startup therefore fails only
    /// when unsettled rows actually remain, not merely because an earlier row
    /// failed; every other row is settled before the error reaches the caller.
    ///
    /// With [`NoopPersistence`] this is a true no-op: `load_pending` yields an
    /// empty vector, so nothing is registered and nothing is audited. No
    /// existing API's return type or behavior changes.
    pub async fn reconcile_pending(&self) -> Result<(), ApprovalError> {
        let pending = self.inner.persistence.load_pending().await?;
        let total = pending.len();
        let now = Utc::now();
        let mut expired = Vec::new();
        let mut recovered = 0usize;
        let mut over_limit = 0usize;
        let mut already_present = 0usize;
        {
            let mut entries = self.inner.entries.write().await;
            for request in pending {
                // A request that raced startup must not overwrite an existing
                // waiter's channel.
                if entries.contains_key(&request.id) {
                    already_present += 1;
                    continue;
                }
                let deadline = request.expires_at;
                let live = entries
                    .values()
                    .filter(|entry| entry.decision.is_none())
                    .count();
                let at_capacity = live >= self.inner.max_pending;
                if deadline > now && !at_capacity {
                    // The receiver is intentionally dropped: recovered holds
                    // have no original caller to notify. `decide` ignores the
                    // resulting `send` error while still recording the decision
                    // in memory and durably. Their timeout path is the lazy
                    // `prune` sweep, which settles them durably on eviction.
                    let (tx, _rx) = watch::channel(None);
                    entries.insert(
                        request.id,
                        Entry {
                            request,
                            decision: None,
                            decided_at: None,
                            deadline,
                            deciding: false,
                            tx,
                        },
                    );
                    recovered += 1;
                } else {
                    if deadline > now {
                        over_limit += 1;
                    }
                    expired.push(request.id);
                }
            }
        }
        // Per-row settlement (I34-E): one failed `mark_expired` must not strand
        // the remaining expired rows. `AlreadyDecided` means the row was already
        // settled by a concurrent decision/expiry and counts as success. Other
        // errors are reported with only the request id and a fixed-vocabulary
        // error category (no URL, SQL or upstream error text) and are collected
        // so the loop always visits every row. `settled_expired` counts actual
        // successes, not attempts: `expired.len()` would over-report the INFO
        // total whenever any single settlement failed (I36-C (a)).
        let mut settled_expired = 0usize;
        let mut settlement_failed = 0usize;
        let mut first_error: Option<ApprovalError> = None;
        for id in expired {
            match self.inner.persistence.mark_expired(id, now).await {
                Ok(()) | Err(ApprovalError::AlreadyDecided(_)) => {
                    settled_expired += 1;
                }
                Err(error) => {
                    settlement_failed += 1;
                    // Fixed vocabulary only: the category is derived from the
                    // variant, never from an error payload or upstream text.
                    let category = error.category();
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                    tracing::warn!(
                        request_id = %id,
                        error_category = category,
                        "failed to settle expired approval; continuing with remaining rows"
                    );
                }
            }
        }
        tracing::info!(
            loaded = total,
            recovered,
            settled_expired,
            settlement_failed,
            over_limit,
            already_present,
            max_pending = self.inner.max_pending,
            "approval startup reconciliation complete"
        );
        // Fail-closed: after attempting every row, fail startup only if an
        // expired row is still unsettled. Returning here keeps
        // `PolicyEngine::new()` from silently ignoring an undecided, unaudited
        // durable row, while the loop above has already settled every other
        // row before the error reaches the caller.
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Drop entries that can no longer produce a real decision, settling every
    /// expired one durably first.
    ///
    /// Adopted approach for I33-A (a): recovered holds re-registered by
    /// [`Self::reconcile_pending`] have no waiter and therefore no timer of
    /// their own. This lazy sweep at the head of [`Self::try_request`] *is*
    /// their timeout path. For a pending entry whose deadline has passed we
    /// call [`ApprovalPersistence::mark_expired`] and remove the entry from
    /// memory **only after the commit succeeds**; a failed commit keeps the
    /// entry so a later sweep (or startup reconciliation) retries instead of
    /// silently leaving an undecided durable row with no audit record. Entries
    /// already recorded as `Expired` were settled by the timeout path, so they
    /// are simply evicted. Entries with a `decide` commit in flight are skipped.
    ///
    /// Besides that, decided entries older than twice the timeout are dropped
    /// by the usual retention window. Called under the caller's write guard so
    /// no new decision can be published while an expiry is being committed.
    /// With [`NoopPersistence`] every commit is an immediate `Ok`, so the
    /// memory-only default keeps its exact behavior.
    async fn prune(
        entries: &mut HashMap<Uuid, Entry>,
        persistence: &dyn ApprovalPersistence,
        timeout: Duration,
        now: DateTime<Utc>,
    ) {
        let retention = timeout.as_secs().saturating_mul(2).min(i64::MAX as u64) as i64;
        let cutoff = now - chrono::Duration::seconds(retention);

        // Phase 1 (no I/O): classify under the caller's write guard.
        let mut to_expire = Vec::new();
        let mut to_drop = Vec::new();
        for (id, entry) in entries.iter_mut() {
            let is_expired = matches!(entry.decision.as_ref(), Some(ApprovalDecision::Expired));
            let deadline_passed = entry.decided_at.is_none() && now > entry.deadline;
            if deadline_passed && !entry.deciding {
                // Abandoned waiter or recovered hold: no timeout path will
                // settle it, so this sweep owns the durable expiry.
                to_expire.push(*id);
            } else if is_expired {
                // The timeout path already called `mark_expired` and stamped
                // `decided_at`; only the in-memory entry is reclaimed here.
                to_drop.push(*id);
            } else if entry
                .decided_at
                .is_some_and(|decided_at| decided_at <= cutoff)
            {
                to_drop.push(*id);
            }
        }

        // Phase 2: commit each expiry BEFORE evicting its in-memory entry.
        for id in to_expire {
            match persistence.mark_expired(id, now).await {
                Ok(()) => {
                    if let Some(entry) = entries.get_mut(&id) {
                        if entry.decided_at.is_none() {
                            entry.decision = Some(ApprovalDecision::Expired);
                            entry.decided_at = Some(now);
                        }
                        // Wake a still-waiting receiver with the same decision
                        // the ordinary timeout path produces. Ignore send
                        // errors: no receiver means the waiter already left.
                        let _ = entry.tx.send(Some(ApprovalDecision::Expired));
                    }
                    entries.remove(&id);
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        request_id = %id,
                        "failed to persist expiration during prune; keeping entry for retry"
                    );
                }
            }
        }
        for id in to_drop {
            entries.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn make_request() -> ApprovalRequest {
        let now = Utc::now();
        ApprovalRequest {
            id: Uuid::new_v4(),
            principal: "jarvis".into(),
            action: "tool:git.commit".into(),
            resource: "repo".into(),
            context: json!({}),
            requested_at: now,
            expires_at: now + chrono::Duration::seconds(60),
        }
    }

    fn approved() -> ApprovalDecision {
        ApprovalDecision::Approved {
            approved_by: "tester".into(),
            approved_at: Utc::now(),
        }
    }

    /// I36-C (a): the warning/error classification is a fixed vocabulary
    /// derived from the variant alone. Payload-bearing ids, DB URLs, SQL text
    /// and upstream messages must never reach a log through `category()`.
    #[test]
    fn approval_error_category_is_fixed_vocabulary() {
        let id = Uuid::new_v4();
        let id_text = id.to_string();
        let cases: Vec<(ApprovalError, &str)> = vec![
            (ApprovalError::NotFound(id), "NotFound"),
            (ApprovalError::AlreadyDecided(id), "AlreadyDecided"),
            (ApprovalError::KillSwitchActivated, "KillSwitchActivated"),
            (ApprovalError::QueueFull, "QueueFull"),
            (ApprovalError::Persistence, "Persistence"),
        ];
        for (error, expected) in cases {
            let category = error.category();
            assert_eq!(category, expected);
            assert!(
                category.chars().all(|c| c.is_ascii_alphanumeric()),
                "category must be a fixed identifier: {category}"
            );
            assert!(!category.contains(id_text.as_str()));
            assert!(!category.to_ascii_lowercase().contains("postgres"));
            assert!(!category.contains("SELECT"));
        }
    }

    /// Decide as soon as the waiter has registered the request.
    async fn decide_after_registration(
        store: &ApprovalStore,
        id: Uuid,
        decision: ApprovalDecision,
    ) {
        for _ in 0..200 {
            match store.decide(id, decision.clone()).await {
                Ok(()) => return,
                Err(ApprovalError::NotFound(_)) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => panic!("unexpected decide error: {error}"),
            }
        }
        panic!("waiter never registered the request");
    }

    #[tokio::test]
    async fn approve_completes_waiter() {
        let store = ApprovalStore::new(Duration::from_secs(5));
        let request = make_request();
        let id = request.id;

        let store_for_waiter = store.clone();
        let waiter = tokio::spawn(async move { store_for_waiter.request(request).await });
        decide_after_registration(&store, id, approved()).await;

        let decision = waiter.await.expect("waiter task");
        assert!(matches!(decision, ApprovalDecision::Approved { .. }));
    }

    #[tokio::test]
    async fn deny_completes_waiter() {
        let store = ApprovalStore::new(Duration::from_secs(5));
        let request = make_request();
        let id = request.id;

        let store_for_waiter = store.clone();
        let waiter = tokio::spawn(async move { store_for_waiter.request(request).await });
        decide_after_registration(
            &store,
            id,
            ApprovalDecision::Denied {
                reason: "not allowed".into(),
            },
        )
        .await;

        let decision = waiter.await.expect("waiter task");
        match decision {
            ApprovalDecision::Denied { reason } => assert_eq!(reason, "not allowed"),
            other => panic!("expected denial, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn request_expires_after_timeout() {
        let store = ApprovalStore::new(Duration::from_millis(100));
        let decision = store.request(make_request()).await;
        assert!(matches!(decision, ApprovalDecision::Expired));
    }

    #[tokio::test]
    async fn status_tracks_pending_and_decided() {
        let store = ApprovalStore::new(Duration::from_secs(5));
        let request = make_request();
        let id = request.id;

        let store_for_waiter = store.clone();
        let waiter = tokio::spawn(async move { store_for_waiter.request(request).await });

        // Wait for registration, then inspect status.
        for _ in 0..200 {
            if !store.list_pending().await.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(matches!(
            store.status(id).await.expect("status"),
            ApprovalDecision::Pending
        ));

        store.decide(id, approved()).await.expect("decide");
        assert!(matches!(
            store.status(id).await.expect("status"),
            ApprovalDecision::Approved { .. }
        ));

        let _ = waiter.await.expect("waiter task");
    }

    #[tokio::test]
    async fn double_decision_and_unknown_id_error() {
        let store = ApprovalStore::new(Duration::from_secs(5));
        let request = make_request();
        let id = request.id;

        let store_for_waiter = store.clone();
        let waiter = tokio::spawn(async move { store_for_waiter.request(request).await });
        decide_after_registration(&store, id, approved()).await;

        assert!(matches!(
            store.decide(id, approved()).await,
            Err(ApprovalError::AlreadyDecided(_))
        ));
        assert!(matches!(
            store.status(Uuid::new_v4()).await,
            Err(ApprovalError::NotFound(_))
        ));

        let _ = waiter.await.expect("waiter task");
    }

    #[tokio::test]
    async fn saturated_queue_denies_new_requests() {
        // One slot only: the first request waits, the second must fail closed
        // instead of growing the in-memory queue without bound.
        // 期待値を更新: キュー枯渇はポリシー拒否 (Denied) ではなく基盤エラー
        // (ApprovalError::QueueFull) として返す。旧 request() の Denied 互換は
        // queue_full_is_reported_as_denied_by_legacy_request で確認する。
        let store = ApprovalStore::with_max_pending(Duration::from_secs(5), 1);
        let first = make_request();
        let first_id = first.id;
        let store_for_waiter = store.clone();
        let waiter = tokio::spawn(async move { store_for_waiter.request(first).await });
        // Wait until the first request is registered as pending. `status`
        // returns NotFound until the spawned task inserts the entry.
        loop {
            if matches!(store.status(first_id).await, Ok(ApprovalDecision::Pending)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let second = store.try_request(make_request()).await;
        assert!(
            matches!(second, Err(ApprovalError::QueueFull)),
            "unexpected: {second:?}"
        );

        store.decide(first_id, approved()).await.expect("decide");
        let _ = waiter.await.expect("waiter task");
    }

    #[tokio::test]
    async fn queue_full_is_reported_as_denied_by_legacy_request() {
        // request() の公開シグネチャと既存クライアント互換のため、枯渇時は
        // 従来どおり Denied { reason: QUEUE_FULL_REASON } を返す。
        let store = ApprovalStore::with_max_pending(Duration::from_secs(5), 1);
        let first = make_request();
        let first_id = first.id;
        let store_for_waiter = store.clone();
        let waiter = tokio::spawn(async move { store_for_waiter.request(first).await });
        loop {
            if matches!(store.status(first_id).await, Ok(ApprovalDecision::Pending)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let second = store.request(make_request()).await;
        assert!(
            matches!(&second, ApprovalDecision::Denied { reason } if reason == QUEUE_FULL_REASON),
            "unexpected: {second:?}"
        );

        store.decide(first_id, approved()).await.expect("decide");
        let _ = waiter.await.expect("waiter task");
    }

    #[tokio::test]
    async fn expired_pending_entries_do_not_exhaust_the_queue() {
        // Regression: an abandoned waiter leaves a pending entry behind. Once
        // its deadline passes, prune() must reclaim the slot so a new request
        // is not rejected with QueueFull.
        let store = ApprovalStore::with_max_pending(Duration::from_millis(50), 1);
        let first = make_request();
        let first_id = first.id;
        let store_for_waiter = store.clone();
        let waiter = tokio::spawn(async move { store_for_waiter.request(first).await });

        loop {
            if matches!(store.status(first_id).await, Ok(ApprovalDecision::Pending)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Simulate the abandoned waiter: drop the task before its timeout
        // fires, leaving the entry pending with no live decision path.
        waiter.abort();
        let _ = waiter.await;

        // Let the stale entry's deadline pass. No periodic task exists, so it
        // is still pending until the next request triggers prune().
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(matches!(
            store.status(first_id).await,
            Ok(ApprovalDecision::Pending)
        ));

        // The new request must be enqueued (and then follow the normal expiry
        // path), not rejected as QueueFull.
        let second = store.try_request(make_request()).await;
        assert!(
            matches!(second, Ok(ApprovalDecision::Expired)),
            "stale pending entry must not occupy the only slot: {second:?}"
        );
        // The abandoned entry was actually removed by prune(), not just
        // bypassed by an off-by-one in the capacity check.
        assert!(matches!(
            store.status(first_id).await,
            Err(ApprovalError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn duplicate_id_does_not_hijack_a_pending_request() {
        let store = ApprovalStore::new(Duration::from_secs(5));
        let first = make_request();
        let id = first.id;
        let store_for_waiter = store.clone();
        let waiter = tokio::spawn(async move { store_for_waiter.request(first).await });

        loop {
            if matches!(store.status(id).await, Ok(ApprovalDecision::Pending)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // A second caller reusing the id must not overwrite the pending entry.
        let mut duplicate = make_request();
        duplicate.id = id;
        let decision = store.request(duplicate).await;
        assert!(
            matches!(&decision, ApprovalDecision::Denied { reason } if reason == "duplicate approval id"),
            "unexpected: {decision:?}"
        );
        // The original request is still pending and the operator can decide it.
        assert!(matches!(
            store.status(id).await.expect("status"),
            ApprovalDecision::Pending
        ));
        store.decide(id, approved()).await.expect("decide");
        let _ = waiter.await.expect("waiter task");
    }

    #[test]
    fn timeout_accessor_and_max_deadline_use_the_configured_window() {
        let store = ApprovalStore::new(Duration::from_secs(7));
        assert_eq!(store.timeout(), Duration::from_secs(7));

        let now = Utc::now();
        assert_eq!(store.max_deadline(now), now + chrono::Duration::seconds(7));
    }

    /// A caller-supplied `expires_at` far in the future must never extend the
    /// wait past the configured timeout. The HTTP layer now clamps the value
    /// before storing it; this guards the store-level cap that still applies
    /// to every other transport (in-process callers included).
    #[tokio::test]
    async fn caller_expires_at_cannot_extend_the_configured_timeout() {
        let store = ApprovalStore::new(Duration::from_millis(100));
        let now = Utc::now();
        let mut request = make_request();
        request.expires_at = now + chrono::Duration::seconds(3600);
        let id = request.id;

        let store_for_waiter = store.clone();
        let waiter = tokio::spawn(async move { store_for_waiter.request(request).await });

        loop {
            if store.status(id).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(store.max_deadline(now) < now + chrono::Duration::seconds(3600));

        let started = std::time::Instant::now();
        assert!(matches!(
            waiter.await.expect("waiter task"),
            ApprovalDecision::Expired
        ));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "deadline must follow the configured timeout, not the caller hint"
        );
    }

    /// I23-E: an abandoned pending entry whose deadline has passed must not be
    /// reported by `list_pending()` (and therefore must not inflate the
    /// `/metrics` pending gauge). The entry is still present in the store until
    /// the next `try_request()` calls `prune()`, which is why `status()` still
    /// sees `Pending` here.
    #[tokio::test]
    async fn list_pending_hides_entries_past_their_deadline() {
        let store = ApprovalStore::with_max_pending(Duration::from_secs(60), 10);
        let active = make_request();
        let stale = make_request();
        let (active_tx, _active_rx) = watch::channel(None);
        let (stale_tx, _stale_rx) = watch::channel(None);
        let now = Utc::now();

        {
            let mut entries = store.inner.entries.write().await;
            entries.insert(
                active.id,
                Entry {
                    request: active.clone(),
                    decision: None,
                    decided_at: None,
                    deadline: now + chrono::Duration::seconds(30),
                    deciding: false,
                    tx: active_tx,
                },
            );
            entries.insert(
                stale.id,
                Entry {
                    request: stale.clone(),
                    decision: None,
                    decided_at: None,
                    deadline: now - chrono::Duration::seconds(1),
                    deciding: false,
                    tx: stale_tx,
                },
            );
        }

        let pending = store.list_pending().await;
        assert_eq!(pending.len(), 1, "only the live request is outstanding");
        assert_eq!(pending[0].id, active.id);
        assert!(matches!(
            store
                .status(stale.id)
                .await
                .expect("stale entry is still stored"),
            ApprovalDecision::Pending
        ));
    }

    // ---- I29-A persistence seam regression tests -------------------------

    #[tokio::test]
    async fn noop_persistence_is_a_no_op() {
        let noop = NoopPersistence;
        let request = make_request();
        noop.record_request(&request, Utc::now())
            .await
            .expect("noop record_request succeeds");
        noop.record_decision(request.id, &approved(), Utc::now())
            .await
            .expect("noop record_decision succeeds");
        let pending = noop.load_pending().await.expect("noop load_pending");
        assert!(
            pending.is_empty(),
            "noop backend reports no durable pending requests"
        );
        noop.mark_expired(request.id, Utc::now())
            .await
            .expect("noop mark_expired succeeds");
    }

    /// `with_persistence(NoopPersistence)` must be behaviorally identical to
    /// the legacy constructors: same registration, decision and waiter result.
    #[tokio::test]
    async fn noop_persistence_matches_default_behavior() {
        let timeout = Duration::from_secs(5);
        let default_store = ApprovalStore::new(timeout);
        let noop_store = ApprovalStore::with_persistence(
            timeout,
            DEFAULT_MAX_PENDING,
            Arc::new(NoopPersistence),
        );

        let request = make_request();
        let id = request.id;
        let noop_request = request.clone();
        let default_waiter = {
            let store = default_store.clone();
            tokio::spawn(async move { store.request(request).await })
        };
        let noop_waiter = {
            let store = noop_store.clone();
            tokio::spawn(async move { store.request(noop_request).await })
        };

        for _ in 0..200 {
            if default_store.status(id).await.is_ok() && noop_store.status(id).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(matches!(
            default_store.status(id).await,
            Ok(ApprovalDecision::Pending)
        ));
        assert!(matches!(
            noop_store.status(id).await,
            Ok(ApprovalDecision::Pending)
        ));

        noop_store
            .decide(id, approved())
            .await
            .expect("noop decide");
        default_store
            .decide(id, approved())
            .await
            .expect("default decide");

        assert!(matches!(
            default_waiter.await.expect("default waiter"),
            ApprovalDecision::Approved { .. }
        ));
        assert!(matches!(
            noop_waiter.await.expect("noop waiter"),
            ApprovalDecision::Approved { .. }
        ));
    }

    /// Test double proving the seam hooks actually fire without any I/O.
    #[derive(Debug, Default)]
    struct RecordingPersistence {
        requests: tokio::sync::Mutex<Vec<Uuid>>,
        decisions: tokio::sync::Mutex<Vec<Uuid>>,
        expired: tokio::sync::Mutex<Vec<Uuid>>,
    }

    #[async_trait::async_trait]
    impl ApprovalPersistence for RecordingPersistence {
        async fn record_request(
            &self,
            request: &ApprovalRequest,
            _deadline: DateTime<Utc>,
        ) -> Result<(), ApprovalError> {
            self.requests.lock().await.push(request.id);
            Ok(())
        }

        async fn record_decision(
            &self,
            id: Uuid,
            _decision: &ApprovalDecision,
            _decided_at: DateTime<Utc>,
        ) -> Result<(), ApprovalError> {
            self.decisions.lock().await.push(id);
            Ok(())
        }

        async fn load_pending(&self) -> Result<Vec<ApprovalRequest>, ApprovalError> {
            Ok(Vec::new())
        }

        async fn mark_expired(
            &self,
            id: Uuid,
            _expired_at: DateTime<Utc>,
        ) -> Result<(), ApprovalError> {
            self.expired.lock().await.push(id);
            Ok(())
        }
    }

    #[tokio::test]
    async fn persistence_hooks_record_request_and_decision() {
        let recorder = Arc::new(RecordingPersistence::default());
        let store = ApprovalStore::with_persistence(
            Duration::from_secs(5),
            DEFAULT_MAX_PENDING,
            recorder.clone(),
        );

        let request = make_request();
        let id = request.id;
        let store_for_waiter = store.clone();
        let waiter = tokio::spawn(async move { store_for_waiter.request(request).await });
        decide_after_registration(&store, id, approved()).await;
        let decision = waiter.await.expect("waiter");
        assert!(matches!(decision, ApprovalDecision::Approved { .. }));

        assert_eq!(*recorder.requests.lock().await, vec![id]);
        assert_eq!(*recorder.decisions.lock().await, vec![id]);
        assert!(recorder.expired.lock().await.is_empty());
    }

    #[tokio::test]
    async fn persistence_hook_records_timeout_as_expired() {
        let recorder = Arc::new(RecordingPersistence::default());
        let store = ApprovalStore::with_persistence(
            Duration::from_millis(50),
            DEFAULT_MAX_PENDING,
            recorder.clone(),
        );

        let request = make_request();
        let id = request.id;
        assert!(matches!(
            store.request(request).await,
            ApprovalDecision::Expired
        ));
        assert_eq!(*recorder.expired.lock().await, vec![id]);
        assert!(recorder.decisions.lock().await.is_empty());
    }

    // ---- I30-A commit-before-watch ordering tests (mock, no real DB) --------

    async fn wait_for_registration(store: &ApprovalStore, id: Uuid) {
        for _ in 0..200 {
            if store.status(id).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("waiter never registered the request");
    }

    /// Mock backend whose `record_decision` can be held open behind a gate and
    /// can simulate a rolled-back transaction. No database is contacted.
    struct GatedPersistence {
        entered: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
        gate: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
        rollback: bool,
    }

    #[async_trait::async_trait]
    impl ApprovalPersistence for GatedPersistence {
        async fn record_request(
            &self,
            _request: &ApprovalRequest,
            _deadline: DateTime<Utc>,
        ) -> Result<(), ApprovalError> {
            Ok(())
        }

        async fn record_decision(
            &self,
            _id: Uuid,
            _decision: &ApprovalDecision,
            _decided_at: DateTime<Utc>,
        ) -> Result<(), ApprovalError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.store(true, Ordering::SeqCst);
            if let Some(rx) = self.gate.lock().await.take() {
                // A real backend commits here. Until the gate opens, returning
                // from this method (and therefore any watch send) is impossible.
                let _ = rx.await;
            }
            if self.rollback {
                Err(ApprovalError::QueueFull)
            } else {
                Ok(())
            }
        }

        async fn load_pending(&self) -> Result<Vec<ApprovalRequest>, ApprovalError> {
            Ok(Vec::new())
        }

        async fn mark_expired(
            &self,
            _id: Uuid,
            _expired_at: DateTime<Utc>,
        ) -> Result<(), ApprovalError> {
            Ok(())
        }
    }

    /// (a) A decision must not be published or sent while the durable commit is
    /// still in flight.
    #[tokio::test]
    async fn watch_does_not_fire_before_commit() {
        let (commit_tx, commit_rx) = tokio::sync::oneshot::channel::<()>();
        let entered = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let persistence = Arc::new(GatedPersistence {
            entered: entered.clone(),
            calls: calls.clone(),
            gate: tokio::sync::Mutex::new(Some(commit_rx)),
            rollback: false,
        });

        let store = ApprovalStore::with_persistence(
            Duration::from_secs(5),
            DEFAULT_MAX_PENDING,
            persistence,
        );
        let request = make_request();
        let id = request.id;
        let waiter_done = Arc::new(AtomicBool::new(false));

        let waiter = {
            let store = store.clone();
            let done = waiter_done.clone();
            tokio::spawn(async move {
                let decision = store.request(request).await;
                done.store(true, Ordering::SeqCst);
                decision
            })
        };
        wait_for_registration(&store, id).await;

        let decider = {
            let store = store.clone();
            tokio::spawn(async move { store.decide(id, approved()).await })
        };
        for _ in 0..200 {
            if entered.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            entered.load(Ordering::SeqCst),
            "record_decision was entered"
        );

        // Gate still closed: neither the in-memory view nor the waiter may see
        // the decision yet.
        assert!(matches!(
            store.status(id).await,
            Ok(ApprovalDecision::Pending)
        ));
        assert!(
            !waiter_done.load(Ordering::SeqCst),
            "waiter must stay blocked until the commit succeeds"
        );

        commit_tx.send(()).expect("release commit gate");
        decider
            .await
            .expect("decide task")
            .expect("decide succeeds");

        let decision = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter completes after commit")
            .expect("waiter task");
        assert!(matches!(decision, ApprovalDecision::Approved { .. }));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// (b) A rolled-back commit must be invisible to the waiter: no in-memory
    /// decision, no watch send, and an error returned to the decider.
    #[tokio::test]
    async fn rollback_hides_decision_from_waiters() {
        let (commit_tx, commit_rx) = tokio::sync::oneshot::channel::<()>();
        let entered = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let persistence = Arc::new(GatedPersistence {
            entered: entered.clone(),
            calls: calls.clone(),
            gate: tokio::sync::Mutex::new(Some(commit_rx)),
            rollback: true,
        });

        let store = ApprovalStore::with_persistence(
            Duration::from_secs(5),
            DEFAULT_MAX_PENDING,
            persistence,
        );
        let request = make_request();
        let id = request.id;
        let waiter_done = Arc::new(AtomicBool::new(false));

        let waiter = {
            let store = store.clone();
            let done = waiter_done.clone();
            tokio::spawn(async move {
                let decision = store.request(request).await;
                done.store(true, Ordering::SeqCst);
                decision
            })
        };
        wait_for_registration(&store, id).await;

        let decider = {
            let store = store.clone();
            tokio::spawn(async move { store.decide(id, approved()).await })
        };
        for _ in 0..200 {
            if entered.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            entered.load(Ordering::SeqCst),
            "record_decision was entered"
        );
        commit_tx.send(()).expect("release rollback gate");

        assert!(
            decider.await.expect("decide task").is_err(),
            "a rolled-back commit must not be reported as success"
        );
        assert!(matches!(
            store.status(id).await,
            Ok(ApprovalDecision::Pending)
        ));
        // Give any erroneous watch send a chance to arrive before asserting.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            !waiter_done.load(Ordering::SeqCst),
            "waiter must never observe a rolled-back decision"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        waiter.abort();
        let _ = waiter.await;
    }

    /// (c) With the mock persistence installed, a second decide still fails
    /// closed with `AlreadyDecided` and does not hit the backend again.
    #[tokio::test]
    async fn double_decide_is_already_decided_with_mock_persistence() {
        let persistence = Arc::new(RecordingPersistence::default());
        let store = ApprovalStore::with_persistence(
            Duration::from_secs(5),
            DEFAULT_MAX_PENDING,
            persistence.clone(),
        );
        let request = make_request();
        let id = request.id;
        let waiter = {
            let store = store.clone();
            tokio::spawn(async move { store.request(request).await })
        };
        wait_for_registration(&store, id).await;

        store.decide(id, approved()).await.expect("first decide");
        assert!(matches!(
            store.decide(id, approved()).await,
            Err(ApprovalError::AlreadyDecided(_))
        ));
        assert_eq!(
            persistence.decisions.lock().await.len(),
            1,
            "the losing decide must not reach persistence"
        );

        let decision = waiter.await.expect("waiter task");
        assert!(matches!(decision, ApprovalDecision::Approved { .. }));
    }

    // ---- I31-A startup reconciliation tests (mock, no real DB) -------------

    /// Mock durable backend that replays a fixed set of undecided rows and
    /// mirrors the Pg backend's audit write inside `mark_expired` (one audit row
    /// per settled hold). No database is contacted. The optional id fields
    /// inject settlement outcomes for I34-E without any I/O.
    #[derive(Debug, Default)]
    struct ReconcilePersistence {
        rows: tokio::sync::Mutex<Vec<ApprovalRequest>>,
        expired: tokio::sync::Mutex<Vec<Uuid>>,
        audit: tokio::sync::Mutex<Vec<Uuid>>,
        decisions: tokio::sync::Mutex<Vec<Uuid>>,
        /// `mark_expired` for this id fails with [`ApprovalError::Persistence`]
        /// and writes no audit row, simulating a failed durable settlement.
        fail_mark_expired_for: Option<Uuid>,
        /// `mark_expired` for this id reports [`ApprovalError::AlreadyDecided`],
        /// simulating a row a concurrent decision/expiry already settled.
        already_decided_for: Option<Uuid>,
    }

    #[async_trait::async_trait]
    impl ApprovalPersistence for ReconcilePersistence {
        async fn record_request(
            &self,
            _request: &ApprovalRequest,
            _deadline: DateTime<Utc>,
        ) -> Result<(), ApprovalError> {
            Ok(())
        }

        async fn record_decision(
            &self,
            id: Uuid,
            _decision: &ApprovalDecision,
            _decided_at: DateTime<Utc>,
        ) -> Result<(), ApprovalError> {
            self.decisions.lock().await.push(id);
            Ok(())
        }

        async fn load_pending(&self) -> Result<Vec<ApprovalRequest>, ApprovalError> {
            Ok(self.rows.lock().await.clone())
        }

        async fn mark_expired(
            &self,
            id: Uuid,
            _expired_at: DateTime<Utc>,
        ) -> Result<(), ApprovalError> {
            if self.already_decided_for == Some(id) {
                return Err(ApprovalError::AlreadyDecided(id));
            }
            if self.fail_mark_expired_for == Some(id) {
                return Err(ApprovalError::Persistence);
            }
            self.expired.lock().await.push(id);
            // `PgApprovalPersistence::mark_expired` commits an Expired decision
            // plus one audit row; the mock mirrors that observable effect.
            self.audit.lock().await.push(id);
            Ok(())
        }
    }

    /// (a) A live durable row is re-registered as a pending entry with no
    /// waiter, so `list_pending()` (and `PolicyEngine::pending_approvals()` and
    /// the `/metrics` gauge) exposes it to the operator UI.
    #[tokio::test]
    async fn reconcile_reregisters_live_row_without_waiter() {
        let mock = Arc::new(ReconcilePersistence::default());
        let live = make_request();
        mock.rows.lock().await.push(live.clone());

        let store = ApprovalStore::with_persistence(
            Duration::from_secs(5),
            DEFAULT_MAX_PENDING,
            mock.clone(),
        );
        store.reconcile_pending().await.expect("reconcile");

        let pending = store.list_pending().await;
        assert_eq!(pending.len(), 1, "the recovered hold is visible");
        assert_eq!(pending[0].id, live.id);
        assert!(matches!(
            store.status(live.id).await,
            Ok(ApprovalDecision::Pending)
        ));
        assert!(mock.expired.lock().await.is_empty());
        assert!(mock.audit.lock().await.is_empty());
    }

    /// A recovered entry has no receiver; deciding it must still record the
    /// decision durably and expose it through `status()`.
    #[tokio::test]
    async fn recovered_entry_can_be_decided_without_a_receiver() {
        let mock = Arc::new(ReconcilePersistence::default());
        let live = make_request();
        mock.rows.lock().await.push(live.clone());

        let store = ApprovalStore::with_persistence(
            Duration::from_secs(5),
            DEFAULT_MAX_PENDING,
            mock.clone(),
        );
        store.reconcile_pending().await.expect("reconcile");
        store
            .decide(live.id, approved())
            .await
            .expect("decide recovered hold");

        assert!(matches!(
            store.status(live.id).await,
            Ok(ApprovalDecision::Approved { .. })
        ));
        assert!(store.list_pending().await.is_empty());
        assert_eq!(*mock.decisions.lock().await, vec![live.id]);
    }

    /// (b) An already-expired durable row is settled via `mark_expired`, which
    /// writes one audit row, and is never re-registered in memory.
    #[tokio::test]
    async fn reconcile_expires_stale_row_and_writes_one_audit_row() {
        let mock = Arc::new(ReconcilePersistence::default());
        let mut stale = make_request();
        stale.expires_at = Utc::now() - chrono::Duration::seconds(1);
        mock.rows.lock().await.push(stale.clone());

        let store = ApprovalStore::with_persistence(
            Duration::from_secs(5),
            DEFAULT_MAX_PENDING,
            mock.clone(),
        );
        store.reconcile_pending().await.expect("reconcile");

        assert_eq!(*mock.expired.lock().await, vec![stale.id]);
        assert_eq!(
            mock.audit.lock().await.len(),
            1,
            "mark_expired commits the Expired decision and its audit row"
        );
        assert!(
            store.list_pending().await.is_empty(),
            "a stale hold must not reappear in the UI after restart"
        );
        assert!(
            store.status(stale.id).await.is_err(),
            "no in-memory entry is created for an expired hold"
        );
    }

    /// (c) Without `pg_url` the store uses `NoopPersistence`; reconciliation
    /// must register nothing and audit nothing.
    #[tokio::test]
    async fn reconcile_is_a_noop_without_persistence() {
        let store = ApprovalStore::new(Duration::from_secs(5));
        store.reconcile_pending().await.expect("noop reconcile");
        assert!(store.list_pending().await.is_empty());

        let explicit_noop = ApprovalStore::with_persistence(
            Duration::from_secs(5),
            DEFAULT_MAX_PENDING,
            Arc::new(NoopPersistence),
        );
        explicit_noop
            .reconcile_pending()
            .await
            .expect("explicit noop reconcile");
        assert!(explicit_noop.list_pending().await.is_empty());
    }

    // ---- R4-PERSIST durable-write failure injection (mock, no real DB) -----

    /// Failure-injecting persistence double. No database is contacted: the
    /// injected `ApprovalError::QueueFull` is exactly what the Pg backend's
    /// `db_failure` returns on a failed INSERT / audit write.
    #[derive(Debug, Default)]
    struct FailingPersistence {
        fail_record_request: AtomicBool,
        fail_mark_expired: AtomicBool,
        record_calls: AtomicUsize,
        expired: tokio::sync::Mutex<Vec<Uuid>>,
    }

    #[async_trait::async_trait]
    impl ApprovalPersistence for FailingPersistence {
        async fn record_request(
            &self,
            _request: &ApprovalRequest,
            _deadline: DateTime<Utc>,
        ) -> Result<(), ApprovalError> {
            self.record_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_record_request.load(Ordering::SeqCst) {
                Err(ApprovalError::QueueFull)
            } else {
                Ok(())
            }
        }

        async fn record_decision(
            &self,
            _id: Uuid,
            _decision: &ApprovalDecision,
            _decided_at: DateTime<Utc>,
        ) -> Result<(), ApprovalError> {
            Ok(())
        }

        async fn load_pending(&self) -> Result<Vec<ApprovalRequest>, ApprovalError> {
            Ok(Vec::new())
        }

        async fn mark_expired(
            &self,
            id: Uuid,
            _expired_at: DateTime<Utc>,
        ) -> Result<(), ApprovalError> {
            self.expired.lock().await.push(id);
            if self.fail_mark_expired.load(Ordering::SeqCst) {
                Err(ApprovalError::QueueFull)
            } else {
                Ok(())
            }
        }
    }

    /// A failed durable INSERT must not leave an in-memory pending hold that a
    /// later operator `decide` cannot settle (zero-row CAS -> `AlreadyDecided`
    /// / HTTP 409 with an empty audit trail). The store must roll back the
    /// entry and answer with the retryable `QueueFull` (HTTP 503).
    #[tokio::test]
    async fn record_request_failure_fails_closed() {
        let persistence = Arc::new(FailingPersistence {
            fail_record_request: AtomicBool::new(true),
            ..Default::default()
        });
        let store = ApprovalStore::with_persistence(
            Duration::from_secs(5),
            DEFAULT_MAX_PENDING,
            persistence.clone(),
        );
        let request = make_request();
        let id = request.id;

        let result = store.try_request(request).await;
        assert!(
            matches!(result, Err(ApprovalError::QueueFull)),
            "INSERT failure must surface as QueueFull (503, retry): {result:?}"
        );
        assert_eq!(persistence.record_calls.load(Ordering::SeqCst), 1);
        assert!(
            matches!(store.status(id).await, Err(ApprovalError::NotFound(_))),
            "no orphan in-memory entry may survive a failed durable INSERT"
        );
        assert!(
            store.list_pending().await.is_empty(),
            "the rolled-back hold must not be visible to operators"
        );
    }

    /// Backend double that reports the dedicated persistence failure on the
    /// durable INSERT. No database is contacted; this mirrors what
    /// `PgApprovalPersistence::db_failure` now returns.
    #[derive(Debug, Default)]
    struct PersistenceFailingPersistence {
        record_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ApprovalPersistence for PersistenceFailingPersistence {
        async fn record_request(
            &self,
            _request: &ApprovalRequest,
            _deadline: DateTime<Utc>,
        ) -> Result<(), ApprovalError> {
            self.record_calls.fetch_add(1, Ordering::SeqCst);
            Err(ApprovalError::Persistence)
        }

        async fn record_decision(
            &self,
            _id: Uuid,
            _decision: &ApprovalDecision,
            _decided_at: DateTime<Utc>,
        ) -> Result<(), ApprovalError> {
            Ok(())
        }

        async fn load_pending(&self) -> Result<Vec<ApprovalRequest>, ApprovalError> {
            Ok(Vec::new())
        }

        async fn mark_expired(
            &self,
            _id: Uuid,
            _expired_at: DateTime<Utc>,
        ) -> Result<(), ApprovalError> {
            Ok(())
        }
    }

    /// I32-D: a backend persistence failure must reach the caller as
    /// `ApprovalError::Persistence` (not be collapsed into `QueueFull`) while
    /// still rolling back the in-memory hold so no orphan survives.
    #[tokio::test]
    async fn record_request_persistence_failure_stays_distinct() {
        let persistence = Arc::new(PersistenceFailingPersistence::default());
        let store = ApprovalStore::with_persistence(
            Duration::from_secs(5),
            DEFAULT_MAX_PENDING,
            persistence.clone(),
        );
        let request = make_request();
        let id = request.id;

        let result = store.try_request(request).await;
        assert!(
            matches!(result, Err(ApprovalError::Persistence)),
            "DB failure must stay distinct from queue exhaustion: {result:?}"
        );
        assert_eq!(persistence.record_calls.load(Ordering::SeqCst), 1);
        assert!(
            matches!(store.status(id).await, Err(ApprovalError::NotFound(_))),
            "no orphan in-memory entry may survive a failed durable INSERT"
        );
        assert!(store.list_pending().await.is_empty());
    }

    /// An injected `mark_expired` failure must be non-fatal for the timing-out
    /// waiter, must still reach the persistence seam (where the warning is
    /// emitted), and must leave the durable row undecided so startup
    /// reconciliation can retry the audit write.
    #[tokio::test]
    async fn mark_expired_failure_is_non_fatal() {
        let persistence = Arc::new(FailingPersistence {
            fail_mark_expired: AtomicBool::new(true),
            ..Default::default()
        });
        let store = ApprovalStore::with_persistence(
            Duration::from_millis(50),
            DEFAULT_MAX_PENDING,
            persistence.clone(),
        );
        let request = make_request();
        let id = request.id;

        assert!(
            matches!(store.request(request).await, ApprovalDecision::Expired),
            "the waiter still times out normally when the audit write fails"
        );
        assert_eq!(*persistence.expired.lock().await, vec![id]);
    }

    // ---- I33-A regression tests (mock backends, no real DB) ---------------

    /// (a) A recovered hold has no waiter, so `prune` is its only timeout
    /// path. It must commit the expiry (audit row included) before evicting
    /// the entry; otherwise a restart leaves an undecided, unaudited row.
    #[tokio::test]
    async fn prune_commits_expiry_for_recovered_hold_before_eviction() {
        let mock = Arc::new(ReconcilePersistence::default());
        let mut recovered = make_request();
        recovered.expires_at = Utc::now() + chrono::Duration::milliseconds(300);
        mock.rows.lock().await.push(recovered.clone());

        let store = ApprovalStore::with_persistence(
            Duration::from_millis(30),
            DEFAULT_MAX_PENDING,
            mock.clone(),
        );
        store.reconcile_pending().await.expect("reconcile");
        assert_eq!(store.list_pending().await.len(), 1, "hold is recovered");

        // Let the recovered deadline pass, then trigger `prune` with a fresh
        // request that itself times out quickly.
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert!(matches!(
            store.request(make_request()).await,
            ApprovalDecision::Expired
        ));

        assert!(
            mock.expired.lock().await.contains(&recovered.id),
            "prune must call mark_expired before evicting the recovered hold"
        );
        assert!(
            mock.audit.lock().await.contains(&recovered.id),
            "the expired recovered hold must have an audit row"
        );
        assert!(
            store.status(recovered.id).await.is_err(),
            "entry is removed only after the commit succeeded"
        );
    }

    /// (b) When the wait times out while a `decide` commit is still in flight,
    /// the timeout must not publish `Expired`. The successful commit is the
    /// authority: the operator gets `Ok` and the waiter observes `Approved`.
    #[tokio::test]
    async fn timeout_yields_to_in_flight_decide_commit() {
        let (commit_tx, commit_rx) = tokio::sync::oneshot::channel::<()>();
        let entered = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let persistence = Arc::new(GatedPersistence {
            entered: entered.clone(),
            calls: calls.clone(),
            gate: tokio::sync::Mutex::new(Some(commit_rx)),
            rollback: false,
        });
        let store = ApprovalStore::with_persistence(
            Duration::from_millis(400),
            DEFAULT_MAX_PENDING,
            persistence,
        );
        let request = make_request();
        let id = request.id;

        let waiter = {
            let store = store.clone();
            tokio::spawn(async move { store.request(request).await })
        };
        wait_for_registration(&store, id).await;

        let decider = {
            let store = store.clone();
            tokio::spawn(async move { store.decide(id, approved()).await })
        };
        for _ in 0..200 {
            if entered.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            entered.load(Ordering::SeqCst),
            "the decide commit must be in flight"
        );

        // The timeout fires while the commit is gated (400 ms window + slack).
        tokio::time::sleep(Duration::from_millis(450)).await;
        assert!(
            matches!(store.status(id).await, Ok(ApprovalDecision::Pending)),
            "timeout must not publish Expired over an in-flight decide"
        );

        commit_tx.send(()).expect("release commit gate");
        decider
            .await
            .expect("decide task")
            .expect("decide succeeds");

        let decision = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("waiter completes after the commit")
            .expect("waiter task");
        assert!(
            matches!(&decision, ApprovalDecision::Approved { .. }),
            "the committed decision is authoritative: {decision:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// (c) Reconciliation must not load more live rows than `max_pending`.
    /// The surplus is settled as `Expired` with one audit row each instead.
    #[tokio::test]
    async fn reconcile_enforces_max_pending_and_audits_the_surplus() {
        let mock = Arc::new(ReconcilePersistence::default());
        for _ in 0..3 {
            mock.rows.lock().await.push(make_request());
        }
        let store = ApprovalStore::with_persistence(Duration::from_secs(5), 2, mock.clone());
        store.reconcile_pending().await.expect("reconcile");

        assert_eq!(
            store.list_pending().await.len(),
            2,
            "only max_pending live rows may be recovered"
        );
        assert_eq!(
            mock.expired.lock().await.len(),
            1,
            "the surplus live row is settled as Expired"
        );
        assert_eq!(
            mock.audit.lock().await.len(),
            1,
            "the surplus settlement writes exactly one audit row"
        );
    }

    /// I34-E regression: a single failed `mark_expired` must not strand the
    /// remaining expired rows. Every row after the failure is still settled and
    /// audited; because one row remains unsettled, reconciliation still fails
    /// closed (the old `?` short-circuit stranded the later rows).
    #[tokio::test]
    async fn reconcile_continues_settling_after_one_mark_expired_failure() {
        let mut failing = make_request();
        failing.expires_at = Utc::now() - chrono::Duration::seconds(1);
        let mut good_a = make_request();
        good_a.expires_at = Utc::now() - chrono::Duration::seconds(1);
        let mut good_b = make_request();
        good_b.expires_at = Utc::now() - chrono::Duration::seconds(1);

        // The failing row is first: the old short-circuit never reached the
        // two rows behind it.
        let mock = Arc::new(ReconcilePersistence {
            fail_mark_expired_for: Some(failing.id),
            ..Default::default()
        });
        {
            let mut rows = mock.rows.lock().await;
            rows.push(failing.clone());
            rows.push(good_a.clone());
            rows.push(good_b.clone());
        }

        let store = ApprovalStore::with_persistence(
            Duration::from_secs(5),
            DEFAULT_MAX_PENDING,
            mock.clone(),
        );
        assert!(
            store.reconcile_pending().await.is_err(),
            "an unsettled row must keep startup fail-closed"
        );

        let settled = mock.expired.lock().await;
        assert!(
            settled.contains(&good_a.id) && settled.contains(&good_b.id),
            "a single failure must not strand later rows: {settled:?}"
        );
        assert!(!settled.contains(&failing.id));
        let audit = mock.audit.lock().await;
        assert_eq!(audit.len(), 2, "exactly the two good rows are audited");
        assert!(audit.contains(&good_a.id) && audit.contains(&good_b.id));
    }

    /// I34-E: `AlreadyDecided` is a success outcome for `mark_expired` (the row
    /// was settled by a concurrent decision/expiry), so reconciliation succeeds
    /// and the remaining expired rows are still settled.
    #[tokio::test]
    async fn reconcile_treats_already_decided_as_success() {
        let mut already = make_request();
        already.expires_at = Utc::now() - chrono::Duration::seconds(1);
        let mut good = make_request();
        good.expires_at = Utc::now() - chrono::Duration::seconds(1);

        let mock = Arc::new(ReconcilePersistence {
            already_decided_for: Some(already.id),
            ..Default::default()
        });
        {
            let mut rows = mock.rows.lock().await;
            rows.push(already.clone());
            rows.push(good.clone());
        }

        let store = ApprovalStore::with_persistence(
            Duration::from_secs(5),
            DEFAULT_MAX_PENDING,
            mock.clone(),
        );
        store.reconcile_pending().await.expect("reconcile");

        let settled = mock.expired.lock().await;
        assert_eq!(*settled, vec![good.id]);
        assert_eq!(*mock.audit.lock().await, vec![good.id]);
    }
}
