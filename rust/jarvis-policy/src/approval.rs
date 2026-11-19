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
//! an expired entry so no durable row is left undecided and unaudited. A
//! failed commit leaves the entry Pending so a later sweep can retry the
//! audit write instead of silently losing it. R5-C: the sweep classifies in a
//! short locked section, commits with the store lock released (so a hanging
//! backend cannot block `status`/`decide`/`list_pending`), then stamps and
//! evicts in a second short section.
//!
//! R4-C: the `deciding` claim is owned by an RAII [`DecidingGuard`] backed by a
//! shared atomic. Its `Drop` releases the claim synchronously, so a `decide`
//! (or timeout) future cancelled mid-commit always leaves the entry `Pending`
//! instead of stranding a flag that blocks both `prune` and later `decide`
//! calls until restart. A time-based stale-claim heuristic was considered and
//! rejected: a grace window that expires while a legitimate durable commit is
//! still in flight could split brain (durable `Approved` vs memory `Expired`).
//! User notification (HA/phone) will be layered on top later.
//!
//! Approval is intentionally HTTP-only: the gRPC surface is limited to
//! authorization and kill-switch operations so a long-running human decision
//! never occupies a gRPC stream.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// A committed decision together with its authoritative durable commit time.
///
/// Returned by [`ApprovalPersistence::load_decision_with_time`] so the timeout
/// path can stamp the in-memory entry with the database's `decided_at` instead
/// of overwriting it with the local timeout instant (I40-A (c)). `ApprovalDecision`
/// has no `Eq` impl, so this mirrors its `PartialEq` derive only.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedDecision {
    pub decision: ApprovalDecision,
    pub decided_at: DateTime<Utc>,
}

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

    /// Re-read the authoritative committed decision for `id`, if any (I38-A).
    ///
    /// Used by the timeout path after a lost expiry CAS: when `mark_expired`
    /// reports [`ApprovalError::AlreadyDecided`] but no watch notification
    /// arrives within the grace window, the durable row is the only source of
    /// truth. `Ok(None)` means no committed decision is visible (the row is
    /// still undecided or unknown); `Ok(Some(_))` is authoritative and wins
    /// over the provisional memory-only `Expired`. Implementations must never
    /// log the id, SQL text or decision body.
    ///
    /// The default returns [`None`], which keeps every existing backend (and
    /// the memory-only default) source-compatible: without a durable re-read
    /// capability the timeout path keeps its legacy `Expired` fallback. The Pg
    /// backend overrides it with the authoritative committed row.
    async fn load_decision(&self, _id: Uuid) -> Result<Option<ApprovalDecision>, ApprovalError> {
        Ok(None)
    }

    /// Like [`Self::load_decision`], but also returns the authoritative commit
    /// time (I40-A (c)).
    ///
    /// The default adapts the legacy [`Self::load_decision`] seam so every
    /// existing backend (and the memory-only default) stays source-compatible:
    /// when a backend has no explicit commit time, the observation time is
    /// used. A transactional backend overrides this with the durable
    /// `decided_at`, which the store copies into the entry so retention and
    /// prune use the real decision time instead of the timeout instant. As
    /// with `load_decision`, no id, SQL or decision body is logged.
    async fn load_decision_with_time(
        &self,
        id: Uuid,
    ) -> Result<Option<LoadedDecision>, ApprovalError> {
        Ok(self
            .load_decision(id)
            .await?
            .map(|decision| LoadedDecision {
                decision,
                decided_at: Utc::now(),
            }))
    }

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

    async fn load_decision(&self, _id: Uuid) -> Result<Option<ApprovalDecision>, ApprovalError> {
        Ok(None)
    }

    async fn mark_expired(
        &self,
        _id: Uuid,
        _expired_at: DateTime<Utc>,
    ) -> Result<(), ApprovalError> {
        Ok(())
    }
}

/// Shared claim flag for one [`Entry`] (R4-C).
///
/// The store's `RwLock` is deliberately released across a durable commit so
/// `status()` keeps observing `Pending` until that commit succeeds. The flag
/// therefore lives behind an `Arc`: a [`DecidingGuard`] can clear it in `Drop`
/// without re-acquiring the async lock (which is impossible in `Drop`), so
/// cancelling the owning future mid-commit always releases the claim.
#[derive(Debug, Default)]
struct DecidingClaim {
    claimed: AtomicBool,
}

impl DecidingClaim {
    fn is_claimed(&self) -> bool {
        self.claimed.load(Ordering::Acquire)
    }

    fn set(&self, value: bool) {
        self.claimed.store(value, Ordering::Release);
    }
}

/// RAII owner of an entry's `deciding` claim (R4-C).
///
/// Created under the store's write guard by [`Entry::claim`] and held across
/// `record_decision` / `mark_expired`. `Drop` clears the claim unconditionally
/// — on success, on error, and on cancellation of the owning future. A
/// cancelled `decide` therefore leaves the entry `Pending`, so `prune` (or
/// startup reconciliation) can still settle it and a later operator retry is
/// not answered with a spurious `AlreadyDecided` (HTTP 409).
struct DecidingGuard(Arc<DecidingClaim>);

impl Drop for DecidingGuard {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

fn new_deciding() -> Arc<DecidingClaim> {
    Arc::new(DecidingClaim::default())
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
    /// Held by a [`DecidingGuard`] while a `decide`/expiry commit is in
    /// flight. Shared through an `Arc` so the guard's `Drop` releases it
    /// synchronously even when the owning future is cancelled across the
    /// commit (R4-C). The write guard is deliberately released across that
    /// commit, so the timeout path checks this flag and yields to an
    /// in-flight decide instead of masking a successful commit with a
    /// memory-only `Expired` marking (I33-A (b)).
    deciding: Arc<DecidingClaim>,
    tx: watch::Sender<Option<ApprovalDecision>>,
}

impl Entry {
    /// True while a decide/expiry commit owns this entry.
    fn is_deciding(&self) -> bool {
        self.deciding.is_claimed()
    }

    /// Start a claim and return its RAII owner. Called under the store's
    /// write guard; the returned guard may outlive that lock and is released
    /// on `Drop`, including when the owning future is cancelled (R4-C).
    fn claim(&self) -> DecidingGuard {
        self.deciding.set(true);
        DecidingGuard(Arc::clone(&self.deciding))
    }

    /// Clear a claim early. The guard's `Drop` clears it again on release;
    /// keeping both makes the in-memory state explicit at each commit point.
    fn release(&self) {
        self.deciding.set(false);
    }
}

struct Inner {
    timeout: Duration,
    max_pending: usize,
    entries: RwLock<HashMap<Uuid, Entry>>,
    /// Additive seam handle. In slice 1 this is always [`NoopPersistence`].
    persistence: Arc<dyn ApprovalPersistence>,
    /// Synchronous authoritative gate re-read at every decision commit
    /// boundary (R5-D). `None` — used by every pre-existing constructor —
    /// keeps the historical behavior unchanged. When present, `decide`
    /// consults it immediately before invoking the backend and again before
    /// publishing the in-memory decision, so an emergency stop that lands
    /// after a caller's pre-check cannot commit a decision. The closure is
    /// called with the store's write guard held: it must not await or
    /// re-enter the store, only re-read the authority.
    commit_gate: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

/// Classification produced under the store's write guard before any durable
/// I/O (R5-C). Each expiry carries an RAII [`DecidingGuard`] so a concurrent
/// `decide` cannot commit over it while the lock is released for the commit.
struct PrunePlan {
    to_expire: Vec<(Uuid, DecidingGuard)>,
    to_drop: Vec<Uuid>,
}

/// How one expired entry was settled durably. The distinction matters to the
/// apply phase: a successful commit keeps an existing stamp, while a lost-ack
/// fallback always stamps `Expired` locally (the pre-split behavior).
enum ExpiryCommit {
    Committed,
    LostAck,
    Authoritative(LoadedDecision),
}

/// A committed expiry still waiting for its short locked stamp/evict section.
struct ExpiredCommit {
    id: Uuid,
    result: ExpiryCommit,
    _claim: DecidingGuard,
}

/// Output of the lock-free commit phase. `to_drop` entries need no I/O: the
/// timeout path already settled them, or they aged out of the retention window.
struct PruneSweep {
    to_settle: Vec<ExpiredCommit>,
    to_drop: Vec<Uuid>,
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

/// Single grace window for the timeout recovery path (I40-A (b)): the time
/// allowed for an in-flight decision notification or the parallel durable
/// re-read to resolve after the deadline elapsed. Retains the legacy 100 ms
/// value so existing timing assumptions stay unchanged; exposed as a constant
/// so the budget is visible and assertable. It is also the hard cap for the
/// final bounded `reread.await` (I42-A (b)): once the grace budget is spent,
/// a still-pending re-read gets at most one more `RECOVERY_GRACE` before the
/// waiter falls back to `Expired`, so DB RTT can never make the wait unbounded.
const RECOVERY_GRACE: Duration = Duration::from_millis(100);

/// Upper bound on yields to an in-flight `decide` commit before the timeout
/// path takes over. Retains the legacy value of 100; exposed so the re-check
/// budget is visible without changing behavior (I40-A (b)).
const MAX_DECIDE_YIELDS: usize = 100;

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
        Self::with_commit_gate(timeout, max_pending, persistence, None)
    }

    /// Additive constructor that also installs a synchronous commit gate.
    ///
    /// `is_blocked` is re-read (never cached) at both commit boundaries of
    /// [`Self::decide`]: before the durable write and before the in-memory
    /// publish. `PolicyEngine` injects a gate backed by the authoritative
    /// kill-switch flag; passing `None` reproduces
    /// [`Self::with_persistence`] exactly. The closure runs with the store's
    /// write guard held, so it must not await or re-enter the store.
    pub fn with_commit_gate(
        timeout: Duration,
        max_pending: usize,
        persistence: Arc<dyn ApprovalPersistence>,
        is_blocked: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                timeout,
                max_pending,
                entries: RwLock::new(HashMap::new()),
                persistence,
                commit_gate: is_blocked,
            }),
        }
    }

    /// Re-read the authoritative emergency-stop gate. With no gate installed
    /// (every legacy constructor) decisions are never blocked here.
    fn commit_blocked(&self) -> bool {
        self.inner.commit_gate.as_ref().is_some_and(|gate| gate())
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
        // The lazy sweep is split into three phases (R5-C): a short locked
        // classification, lock-free durable commits, then a short locked
        // apply. Holding `entries.write()` across `mark_expired` would freeze
        // `status`/`decide`/`list_pending` behind a hanging database.
        let plan = {
            let mut entries = self.inner.entries.write().await;
            Self::classify_prune(&mut entries, self.inner.timeout, now)
        };
        let sweep = Self::commit_prunes(plan, &*self.inner.persistence, now).await;
        {
            let mut entries = self.inner.entries.write().await;
            Self::apply_prune(&mut entries, sweep, now);
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
                    deciding: new_deciding(),
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
                    /// Carries the RAII claim across the durable commit so a
                    /// cancelled waiter releases it instead of leaking it.
                    Expire(DecidingGuard),
                    YieldToDecide,
                }
                // Bound the yields: a genuinely stuck `decide` commit cannot
                // hold this waiter forever. A cancelled commit is different —
                // its guard's `Drop` clears the claim immediately (R4-C).
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
                                } else if entry.is_deciding() && yields < MAX_DECIDE_YIELDS {
                                    TimeoutOutcome::YieldToDecide
                                } else {
                                    if entry.is_deciding() {
                                        entry.release();
                                        tracing::warn!(
                                            request_id = %id,
                                            "stale decide claim cleared after repeated yields"
                                        );
                                    }
                                    // I43-B (a): claim the entry for the expiry
                                    // commit, but keep it Pending until that
                                    // commit succeeds. Stamping `decision`/
                                    // `decided_at` here would make `prune` drop
                                    // the entry as already-expired even when the
                                    // durable audit write failed, losing the
                                    // retry. The `deciding` claim also keeps a
                                    // concurrent prune from racing this commit;
                                    // the guard lives across `mark_expired` so a
                                    // cancelled waiter releases it too (R4-C).
                                    TimeoutOutcome::Expire(entry.claim())
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
                            let _ = tokio::time::timeout(RECOVERY_GRACE, rx.changed()).await;
                            continue;
                        }
                        TimeoutOutcome::Expire(_guard) => {
                            // `_guard` is held until this arm ends (or the
                            // future is dropped), then releases the claim.
                            // `NoopPersistence` never errors, so the default
                            // path is unchanged. A failed `mark_expired` leaves
                            // the entry Pending (no decision/decided_at stamp),
                            // so the next `prune` (or startup reconciliation)
                            // retries the audit write instead of losing it.
                            let mark = self.inner.persistence.mark_expired(id, expired_at).await;
                            match mark {
                                // A concurrent settlement won the durable CAS.
                                // The row may already carry the real decision
                                // even though this waiter never observed the
                                // watch notification (lost-ack). Start the
                                // authoritative re-read NOW, in parallel with
                                // the single grace window, and adopt whichever
                                // produces a decision first. The old sequential
                                // path added grace + DB-RTT×3 after the deadline
                                // (I40-A (b)); DB=decided / waiter=Expired can no
                                // longer split brain.
                                Err(ApprovalError::AlreadyDecided(_)) => {
                                    let mut reread =
                                        self.inner.persistence.load_decision_with_time(id);
                                    // A decision already on the watch channel is
                                    // an immediate fast path (I36-C (3)).
                                    let published = {
                                        let guard = rx.borrow_and_update();
                                        (*guard).clone()
                                    };

                                    enum Race {
                                        Watch,
                                        Durable(Result<Option<LoadedDecision>, ApprovalError>),
                                        Grace,
                                    }

                                    // Resolve a durable re-read into the
                                    // `(decision, authoritative decided_at)` pair.
                                    // `Ok(Some(_))` is the only authoritative
                                    // outcome; `Ok(None)`/`Err` stay fail-closed.
                                    // Fixed vocabulary only: never log an id,
                                    // SQL text or decision body here.
                                    fn resolve_durable(
                                        id: Uuid,
                                        result: Result<Option<LoadedDecision>, ApprovalError>,
                                    ) -> Option<(ApprovalDecision, Option<DateTime<Utc>>)>
                                    {
                                        match result {
                                            Ok(Some(loaded)) => {
                                                Some((loaded.decision, Some(loaded.decided_at)))
                                            }
                                            Ok(None) => None,
                                            Err(error) => {
                                                tracing::warn!(
                                                    error_category = error.category(),
                                                    request_id = %id,
                                                    "durable decision re-read failed after lost expiry CAS"
                                                );
                                                None
                                            }
                                        }
                                    }

                                    // `(decision, authoritative decided_at)`; a
                                    // `None` timestamp means the backend has no
                                    // time seam and the observation time is used.
                                    let recovered = if let Some(decision) = published {
                                        Some((decision, None))
                                    } else {
                                        // One visible budget shared by the watch
                                        // re-check and the parallel re-read. A
                                        // non-authoritative durable result
                                        // (`Ok(None)`/`Err`) is recorded but never
                                        // ends the wait early: a late watch
                                        // notification may still carry the
                                        // committed decision, so resolving
                                        // `Expired` before the grace deadline
                                        // would reintroduce the DB=decided /
                                        // waiter=Expired split brain (I42-A (a)).
                                        let deadline = tokio::time::Instant::now() + RECOVERY_GRACE;
                                        let mut durable_fallback: Option<
                                            Result<Option<LoadedDecision>, ApprovalError>,
                                        > = None;

                                        loop {
                                            let remaining = deadline.saturating_duration_since(
                                                tokio::time::Instant::now(),
                                            );
                                            // I45-C (a): with `biased;` the watch
                                            // branch is polled first, so a durable
                                            // decision published in the same poll as
                                            // the grace sleep wins instead of a random
                                            // branch expiring the waiter (DB=Approved /
                                            // waiter=Expired split brain). The Grace
                                            // arm also re-checks the watch before every
                                            // break, covering a send that lands after
                                            // the poll already chose Grace.
                                            let winner = tokio::select! {
                                                biased;
                                                changed = rx.changed() => {
                                                    if changed.is_ok() { Race::Watch }
                                                    else { Race::Grace }
                                                },
                                                // Disabled once the re-read has
                                                // resolved non-authoritatively: the
                                                // watch still gets the full budget.
                                                result = &mut reread, if durable_fallback.is_none() => Race::Durable(result),
                                                _ = tokio::time::sleep(remaining) => Race::Grace,
                                            };
                                            match winner {
                                                Race::Watch => {
                                                    let guard = rx.borrow_and_update();
                                                    if let Some(decision) = (*guard).clone() {
                                                        break Some((decision, None));
                                                    }
                                                }
                                                Race::Durable(result) => match result {
                                                    Ok(Some(loaded)) => {
                                                        break Some((
                                                            loaded.decision,
                                                            Some(loaded.decided_at),
                                                        ));
                                                    }
                                                    other => durable_fallback = Some(other),
                                                },
                                                Race::Grace => {
                                                    // I45-C (a): the grace sleep and a
                                                    // concurrent commit's watch send can
                                                    // become ready in the same instant.
                                                    // `biased;` prefers Watch when both are
                                                    // ready at poll time; this final read
                                                    // also adopts a send that landed just
                                                    // after the poll chose Grace, so a
                                                    // published decision always beats
                                                    // `Expired`. `borrow_and_update` takes
                                                    // `&mut`, hence the mutable binding.
                                                    // Fixed vocabulary only: no id, SQL
                                                    // text or decision body is logged.
                                                    let mut watch_now = || {
                                                        let guard = rx.borrow_and_update();
                                                        (*guard).clone()
                                                    };
                                                    if let Some(decision) = watch_now() {
                                                        break Some((decision, None));
                                                    }
                                                    if let Some(result) = durable_fallback.take() {
                                                        break match watch_now() {
                                                            Some(decision) => {
                                                                Some((decision, None))
                                                            }
                                                            None => resolve_durable(id, result),
                                                        };
                                                    }
                                                    // The re-read is still in
                                                    // flight at budget expiry. Give
                                                    // it one final BOUNDED wait: a
                                                    // slow/stalled DB RTT must not
                                                    // hold the waiter indefinitely
                                                    // (I42-A (b)). On timeout, a
                                                    // fixed-vocabulary warn with the
                                                    // request id only, then the
                                                    // Expired fallback below.
                                                    match tokio::time::timeout(
                                                        RECOVERY_GRACE,
                                                        &mut reread,
                                                    )
                                                    .await
                                                    {
                                                        Ok(result) => {
                                                            break match watch_now() {
                                                                Some(decision) => {
                                                                    Some((decision, None))
                                                                }
                                                                None => resolve_durable(id, result),
                                                            };
                                                        }
                                                        Err(_) => {
                                                            if let Some(decision) = watch_now() {
                                                                break Some((decision, None));
                                                            }
                                                            tracing::warn!(
                                                                request_id = %id,
                                                                "durable decision re-read timed out after lost expiry CAS; waiter expires"
                                                            );
                                                            break None;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    };

                                    if let Some((decision, authoritative_at)) = recovered {
                                        // Reconcile the still-Pending entry with
                                        // the authoritative decision so memory,
                                        // watch, waiter and the durable row agree.
                                        // The DB commit time wins over the local
                                        // timeout instant (I40-A (c)). Since
                                        // I43-B (a) no longer stamps a
                                        // provisional `Expired` before the CAS,
                                        // Pending is the expected state here;
                                        // the old `Expired`-only check is kept
                                        // for backends/third parties that still
                                        // publish one.
                                        let mut entries = self.inner.entries.write().await;
                                        if let Some(entry) = entries.get_mut(&id) {
                                            entry.release();
                                            if entry.decision.is_none()
                                                || matches!(
                                                    entry.decision.as_ref(),
                                                    Some(&ApprovalDecision::Expired)
                                                )
                                            {
                                                entry.decision = Some(decision.clone());
                                                entry.decided_at =
                                                    Some(authoritative_at.unwrap_or_else(Utc::now));
                                                let _ = entry.tx.send(Some(decision.clone()));
                                            }
                                        }
                                        return Ok(decision);
                                    }
                                    // No authoritative decision was visible.
                                    // Release the expiry claim and leave the
                                    // entry Pending so prune / startup
                                    // reconciliation can still settle it.
                                    {
                                        let mut entries = self.inner.entries.write().await;
                                        if let Some(entry) = entries.get_mut(&id) {
                                            entry.release();
                                        }
                                    }
                                    tracing::warn!(
                                        request_id = %id,
                                        "expiry CAS lost and durable re-read found no decision; waiter expires"
                                    );
                                    return Ok(ApprovalDecision::Expired);
                                }
                                Err(error) => {
                                    // The entry stays Pending and the claim is
                                    // released, so the next prune (or startup
                                    // reconciliation) can retry the audit
                                    // write. The caller still gets `Expired`.
                                    {
                                        let mut entries = self.inner.entries.write().await;
                                        if let Some(entry) = entries.get_mut(&id) {
                                            entry.release();
                                        }
                                    }
                                    tracing::warn!(
                                        error_category = error.category(),
                                        request_id = %id,
                                        "failed to persist approval expiration; audit row may be missing"
                                    );
                                    return Ok(ApprovalDecision::Expired);
                                }
                                Ok(()) => {
                                    // Only a committed expiry may make the
                                    // in-memory entry decided (I43-B (a)).
                                    let mut entries = self.inner.entries.write().await;
                                    if let Some(entry) = entries.get_mut(&id) {
                                        entry.release();
                                        if entry.decision.is_none() {
                                            entry.decision = Some(ApprovalDecision::Expired);
                                            entry.decided_at = Some(expired_at);
                                        }
                                    }
                                    return Ok(ApprovalDecision::Expired);
                                }
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
    /// overwritten to match. A short write critical section claims the entry
    /// (`deciding`) before the commit so the timeout path can yield to an
    /// in-flight commit instead of racing it with a memory-only `Expired`
    /// marking. The claim is handed to an RAII [`DecidingGuard`] that is held
    /// across the commit and released on `Drop` (R4-C): cancelling this future
    /// mid-commit can no longer strand a claim that blocks `prune` and later
    /// decide attempts. The store's write lock is deliberately *not* held
    /// across the commit: `status()` and `watch_does_not_fire_before_commit`
    /// keep observing `Pending` until the commit succeeds, and a rollback
    /// publishes nothing.
    ///
    /// A request whose deadline has already passed is explicitly rejected
    /// (expiry is a terminal decision, reported as `AlreadyDecided` with a
    /// warning) and never reaches the persistence backend. With
    /// [`NoopPersistence`] the commit is immediate, so memory-only behavior is
    /// unchanged.
    ///
    /// R5-D: an optional synchronous commit gate ([`Self::with_commit_gate`])
    /// is re-read at both commit boundaries — before invoking the backend and
    /// before publishing the in-memory decision. A kill switch activation that
    /// lands after the transport's pre-check therefore cannot commit a
    /// decision. This is additive: every pre-existing constructor installs no
    /// gate, so the timeout, lost-ack, RAII-claim and prune paths are untouched.
    pub async fn decide(&self, id: Uuid, decision: ApprovalDecision) -> Result<(), ApprovalError> {
        let decided_at = Utc::now();

        // Precondition + claim in one short write critical section: no `await`
        // while held, and a second `decide` fails closed without touching the
        // persistence backend. The guard outlives the lock and is released on
        // `Drop` even if this future is cancelled mid-commit (R4-C).
        let _claim = {
            let mut entries = self.inner.entries.write().await;
            let entry = entries.get_mut(&id).ok_or(ApprovalError::NotFound(id))?;
            if decided_at > entry.deadline {
                tracing::warn!(
                    request_id = %id,
                    "decide rejected: the decision window has elapsed"
                );
                return Err(ApprovalError::AlreadyDecided(id));
            }
            if entry.decision.is_some() || entry.is_deciding() {
                return Err(ApprovalError::AlreadyDecided(id));
            }
            // R5-D: re-read the authoritative emergency-stop state at the
            // commit boundary, not only in the transport pre-check. This runs
            // with the entry claimed and no `await` in the critical section,
            // so an activation that landed after the caller's check is
            // rejected before the persistence backend is ever touched.
            if self.commit_blocked() {
                tracing::warn!("decision commit blocked: kill switch activated after pre-check");
                return Err(ApprovalError::KillSwitchActivated);
            }
            entry.claim()
        };

        // Persist (and, for the Pg backend, commit) BEFORE publishing. The
        // store write lock is released here on purpose; the RAII claim above
        // is what serializes this path against the timeout path, and its
        // `Drop` releases the flag even when the commit is cancelled.
        let commit = self
            .inner
            .persistence
            .record_decision(id, &decision, decided_at)
            .await;

        {
            let mut entries = self.inner.entries.write().await;
            let entry = entries.get_mut(&id).ok_or(ApprovalError::NotFound(id))?;
            entry.release();
            // A failed commit must leave the entry pending so no waiter can
            // observe a rolled-back decision.
            commit?;
            // R5-D: re-read the authority once more before finalizing. An
            // activation that landed while the durable commit was in flight
            // must not publish a decision to the waiter or the UI. The entry
            // stays Pending (claim already released) so the operator can retry
            // after deactivation; the error vocabulary is fixed and carries no
            // id, path or backend detail.
            if self.commit_blocked() {
                tracing::warn!("decision publish blocked: kill switch activated during commit");
                return Err(ApprovalError::KillSwitchActivated);
            }
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
                            deciding: new_deciding(),
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

    /// Classify expired entries under the caller's write guard (R5-C phase 1).
    ///
    /// Recovered holds re-registered by [`Self::reconcile_pending`] have no
    /// waiter and thus no timer: this lazy sweep at the head of
    /// [`Self::try_request`] is their timeout path. An unclaimed entry past its
    /// deadline is claimed for the durable expiry; a live `decide`/expiry
    /// claim is skipped, while a cancelled one is already released by its RAII
    /// [`DecidingGuard`] (R4-C). Entries already stamped `Expired` by the
    /// timeout path and decided entries past the retention window only need
    /// in-memory reclamation. No I/O happens here.
    fn classify_prune(
        entries: &mut HashMap<Uuid, Entry>,
        timeout: Duration,
        now: DateTime<Utc>,
    ) -> PrunePlan {
        let retention = timeout.as_secs().saturating_mul(2).min(i64::MAX as u64) as i64;
        let cutoff = now - chrono::Duration::seconds(retention);

        let mut to_expire = Vec::new();
        let mut to_drop = Vec::new();
        for (id, entry) in entries.iter_mut() {
            let is_expired = matches!(entry.decision.as_ref(), Some(ApprovalDecision::Expired));
            let deadline_passed = entry.decided_at.is_none() && now > entry.deadline;
            if deadline_passed && !entry.is_deciding() {
                // Abandoned waiter or recovered hold: no timeout path will
                // settle it, so this sweep owns the durable expiry. The claim
                // is carried into the lock-free commit phase so a concurrent
                // `decide` cannot commit over the pending expiry.
                to_expire.push((*id, entry.claim()));
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
        PrunePlan { to_expire, to_drop }
    }

    /// Commit every classified expiry with the store lock released (R5-C
    /// phase 2). Each `mark_expired` may reconnect and block on a hanging
    /// database, so no `status`, `decide`, `list_pending` or other
    /// `try_request` may queue behind these awaits. With [`NoopPersistence`]
    /// every commit is an immediate `Ok`, so the memory-only default keeps its
    /// exact behavior. A failed commit drops its claim and leaves the entry
    /// Pending for the next sweep; only fixed error categories are logged.
    async fn commit_prunes(
        plan: PrunePlan,
        persistence: &dyn ApprovalPersistence,
        now: DateTime<Utc>,
    ) -> PruneSweep {
        let mut to_settle = Vec::new();
        for (id, claim) in plan.to_expire {
            match persistence.mark_expired(id, now).await {
                Ok(()) => to_settle.push(ExpiredCommit {
                    id,
                    result: ExpiryCommit::Committed,
                    _claim: claim,
                }),
                // I43-B (b): a concurrent settlement already committed the
                // durable decision, so this ghost entry must NOT be retained
                // forever. Mirror the reconcile / timeout paths: treat it as
                // success, re-read the authoritative decision (when the backend
                // exposes it) and reflect it in memory during the apply phase.
                Err(ApprovalError::AlreadyDecided(_)) => {
                    let result = match persistence.load_decision_with_time(id).await {
                        Ok(Some(loaded)) => ExpiryCommit::Authoritative(loaded),
                        Ok(None) => ExpiryCommit::LostAck,
                        Err(error) => {
                            tracing::warn!(
                                error_category = error.category(),
                                request_id = %id,
                                "durable decision re-read failed after prune expiry CAS loss"
                            );
                            ExpiryCommit::LostAck
                        }
                    };
                    to_settle.push(ExpiredCommit {
                        id,
                        result,
                        _claim: claim,
                    });
                }
                Err(error) => {
                    tracing::warn!(
                        error_category = error.category(),
                        request_id = %id,
                        "failed to persist expiration during prune; keeping entry for retry"
                    );
                    // Release the claim so the next sweep can classify this
                    // entry again; otherwise it would stay `deciding` forever.
                    drop(claim);
                }
            }
        }
        PruneSweep {
            to_settle,
            to_drop: plan.to_drop,
        }
    }

    /// Stamp and evict committed expiries in one short write critical section
    /// (R5-C phase 3), held by the caller together with the registration of
    /// the new request. The RAII claim lives until after the eviction so a
    /// concurrent `decide` cannot publish over a committed expiry.
    fn apply_prune(entries: &mut HashMap<Uuid, Entry>, sweep: PruneSweep, now: DateTime<Utc>) {
        for outcome in sweep.to_settle {
            if let Some(entry) = entries.get_mut(&outcome.id) {
                match outcome.result {
                    ExpiryCommit::Committed => {
                        if entry.decided_at.is_none() {
                            entry.decision = Some(ApprovalDecision::Expired);
                            entry.decided_at = Some(now);
                        }
                        // Wake a still-waiting receiver with the same decision
                        // the ordinary timeout path produces. Ignore send
                        // errors: no receiver means the waiter already left.
                        let _ = entry.tx.send(Some(ApprovalDecision::Expired));
                    }
                    ExpiryCommit::LostAck => {
                        // No authoritative value visible, but the CAS proved
                        // the durable row is decided. Settle locally as
                        // `Expired` so a still-waiting receiver is released.
                        entry.decision = Some(ApprovalDecision::Expired);
                        entry.decided_at = Some(now);
                        let _ = entry.tx.send(Some(ApprovalDecision::Expired));
                    }
                    ExpiryCommit::Authoritative(loaded) => {
                        entry.decision = Some(loaded.decision.clone());
                        entry.decided_at = Some(loaded.decided_at);
                        let _ = entry.tx.send(Some(loaded.decision));
                    }
                }
            }
            entries.remove(&outcome.id);
            // `outcome._claim` drops here, after eviction.
        }
        for id in sweep.to_drop {
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
                    deciding: new_deciding(),
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
                    deciding: new_deciding(),
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

        async fn load_decision(
            &self,
            _id: Uuid,
        ) -> Result<Option<ApprovalDecision>, ApprovalError> {
            Ok(None)
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

        async fn load_decision(
            &self,
            _id: Uuid,
        ) -> Result<Option<ApprovalDecision>, ApprovalError> {
            Ok(None)
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

        async fn load_decision(
            &self,
            _id: Uuid,
        ) -> Result<Option<ApprovalDecision>, ApprovalError> {
            Ok(None)
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

        async fn load_decision(
            &self,
            _id: Uuid,
        ) -> Result<Option<ApprovalDecision>, ApprovalError> {
            Ok(None)
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

        async fn load_decision(
            &self,
            _id: Uuid,
        ) -> Result<Option<ApprovalDecision>, ApprovalError> {
            Ok(None)
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

    // ---- I38-A durable re-read after a lost expiry CAS (mock, no real DB) ---

    /// Lost-ack double: `mark_expired` reports `AlreadyDecided` (a concurrent
    /// decider committed) while no watch notification is ever sent, and
    /// `load_decision` reports the configured durable state. No I/O and no
    /// watch send: exactly the shape that used to split DB=decided from
    /// waiter=Expired.
    #[derive(Debug)]
    struct LostAckPersistence {
        committed: Option<ApprovalDecision>,
        fail_load: bool,
        load_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ApprovalPersistence for LostAckPersistence {
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

        async fn load_decision(
            &self,
            _id: Uuid,
        ) -> Result<Option<ApprovalDecision>, ApprovalError> {
            self.load_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_load {
                Err(ApprovalError::Persistence)
            } else {
                Ok(self.committed.clone())
            }
        }

        async fn mark_expired(
            &self,
            id: Uuid,
            _expired_at: DateTime<Utc>,
        ) -> Result<(), ApprovalError> {
            Err(ApprovalError::AlreadyDecided(id))
        }
    }

    /// The regression: an expiry CAS loss with no watch notification must be
    /// resolved from the durable decision, overwriting both the provisional
    /// in-memory `Expired` and the waiter's return value.
    #[tokio::test]
    async fn lost_expiry_cas_rereads_durable_decision() {
        let committed = approved();
        let persistence = Arc::new(LostAckPersistence {
            committed: Some(committed.clone()),
            fail_load: false,
            load_calls: AtomicUsize::new(0),
        });
        let store = ApprovalStore::with_persistence(
            Duration::from_millis(30),
            DEFAULT_MAX_PENDING,
            persistence.clone(),
        );
        let request = make_request();
        let id = request.id;

        let decision = store.request(request).await;
        assert_eq!(
            decision, committed,
            "the durable decision must win over the provisional Expired"
        );
        assert_eq!(persistence.load_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            store.status(id).await.expect("status"),
            committed,
            "the in-memory entry must be reconciled with the durable decision"
        );
    }

    /// Fail-safe: no committed durable row (or a re-read outage) keeps the
    /// legacy `Expired` fallback; the seam never fabricates a decision the
    /// database does not have.
    #[tokio::test]
    async fn lost_expiry_cas_without_durable_decision_stays_expired() {
        let cases: [(Option<ApprovalDecision>, bool); 2] = [(None, false), (None, true)];
        for (committed, fail_load) in cases {
            let persistence = Arc::new(LostAckPersistence {
                committed,
                fail_load,
                load_calls: AtomicUsize::new(0),
            });
            let store = ApprovalStore::with_persistence(
                Duration::from_millis(30),
                DEFAULT_MAX_PENDING,
                persistence.clone(),
            );
            let decision = store.request(make_request()).await;
            assert!(
                matches!(decision, ApprovalDecision::Expired),
                "no authoritative row must leave the waiter Expired: {decision:?}"
            );
            assert_eq!(persistence.load_calls.load(Ordering::SeqCst), 1);
        }
    }

    // ---- I40-A (b)/(c) parallel re-read + authoritative decision time ------

    /// Lost-expiry-CAS mock: `mark_expired` always reports `AlreadyDecided`,
    /// `load_decision_with_time` records when it was called and can block on a
    /// oneshot to simulate DB latency. No database and no watch notification.
    #[derive(Debug)]
    struct RereadDecisionPersistence {
        committed: LoadedDecision,
        started: Arc<AtomicBool>,
        gate: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    }

    #[async_trait::async_trait]
    impl ApprovalPersistence for RereadDecisionPersistence {
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

        async fn load_decision(
            &self,
            _id: Uuid,
        ) -> Result<Option<ApprovalDecision>, ApprovalError> {
            Ok(Some(self.committed.decision.clone()))
        }

        async fn load_decision_with_time(
            &self,
            _id: Uuid,
        ) -> Result<Option<LoadedDecision>, ApprovalError> {
            self.started.store(true, Ordering::SeqCst);
            if let Some(rx) = self.gate.lock().await.take() {
                let _ = rx.await;
            }
            Ok(Some(self.committed.clone()))
        }

        async fn mark_expired(
            &self,
            id: Uuid,
            _expired_at: DateTime<Utc>,
        ) -> Result<(), ApprovalError> {
            Err(ApprovalError::AlreadyDecided(id))
        }
    }

    /// I40-A (b): the durable re-read must start in parallel with the grace
    /// window, not after it, so the post-deadline wait is
    /// `max(grace, DB RTT)` instead of `grace + DB RTT × 3`.
    #[tokio::test]
    async fn lost_expiry_cas_reread_starts_in_parallel_with_grace() {
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let committed = LoadedDecision {
            decision: approved(),
            decided_at: Utc::now(),
        };
        let started = Arc::new(AtomicBool::new(false));
        let persistence = Arc::new(RereadDecisionPersistence {
            committed: committed.clone(),
            started: started.clone(),
            gate: tokio::sync::Mutex::new(Some(release_rx)),
        });
        let store = ApprovalStore::with_persistence(
            Duration::from_millis(40),
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

        // Well inside the 100ms grace the re-read must already be running.
        tokio::time::sleep(Duration::from_millis(70)).await;
        assert!(
            started.load(Ordering::SeqCst),
            "durable re-read must start in parallel with the grace, not after it"
        );
        assert!(
            !waiter.is_finished(),
            "the waiter must await the authoritative re-read, not expire early"
        );

        release_tx.send(()).expect("release re-read");
        let decision = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter completes")
            .expect("waiter task");
        assert_eq!(decision, committed.decision);
    }

    /// I40-A (c): the authoritative `decided_at` from the durable row must be
    /// copied into the in-memory entry; the local timeout instant must not
    /// overwrite it (prune retention depends on the real decision time).
    #[tokio::test]
    async fn lost_expiry_cas_uses_authoritative_decided_at() {
        let authoritative_at = Utc::now() - chrono::Duration::seconds(30);
        let committed = LoadedDecision {
            decision: approved(),
            decided_at: authoritative_at,
        };
        let persistence = Arc::new(RereadDecisionPersistence {
            committed: committed.clone(),
            started: Arc::new(AtomicBool::new(false)),
            gate: tokio::sync::Mutex::new(None),
        });
        let store = ApprovalStore::with_persistence(
            Duration::from_millis(40),
            DEFAULT_MAX_PENDING,
            persistence,
        );
        let request = make_request();
        let id = request.id;

        assert_eq!(store.request(request).await, committed.decision);
        let entries = store.inner.entries.read().await;
        let entry = entries.get(&id).expect("entry survives reconciliation");
        assert_eq!(
            entry.decided_at,
            Some(authoritative_at),
            "DB commit time must win over the local timeout instant"
        );
    }

    // ---- I42-A (a)/(b) bounded durable re-read after a lost expiry CAS ------

    /// I42-A (a): an immediately failing durable re-read must NOT resolve the
    /// waiter to `Expired` before the grace window is up. A late watch
    /// notification carrying the concurrently committed decision (published
    /// after the transient DB error) must still be adopted, so DB=decided and
    /// waiter=Expired cannot split brain. No real DB, no `decide` preconditions:
    /// the watch send is injected directly into the entry's sender.
    #[tokio::test]
    async fn lost_expiry_cas_reread_error_then_late_watch_wins() {
        #[derive(Debug)]
        struct FailingRereadPersistence {
            load_started: Arc<AtomicBool>,
            load_calls: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl ApprovalPersistence for FailingRereadPersistence {
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

            async fn load_decision_with_time(
                &self,
                _id: Uuid,
            ) -> Result<Option<LoadedDecision>, ApprovalError> {
                self.load_started.store(true, Ordering::SeqCst);
                self.load_calls.fetch_add(1, Ordering::SeqCst);
                // Transient DB error resolved immediately after the CAS loss.
                Err(ApprovalError::Persistence)
            }

            async fn mark_expired(
                &self,
                id: Uuid,
                _expired_at: DateTime<Utc>,
            ) -> Result<(), ApprovalError> {
                Err(ApprovalError::AlreadyDecided(id))
            }
        }

        let load_started = Arc::new(AtomicBool::new(false));
        let persistence = Arc::new(FailingRereadPersistence {
            load_started: load_started.clone(),
            load_calls: AtomicUsize::new(0),
        });
        let store = ApprovalStore::with_persistence(
            Duration::from_millis(30),
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

        // Wait until the durable re-read has failed, proving the recovery path
        // is now re-checking the watch inside the grace window.
        for _ in 0..200 {
            if load_started.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            load_started.load(Ordering::SeqCst),
            "the durable re-read must have failed before the watch decision arrives"
        );
        assert!(
            !waiter.is_finished(),
            "a non-authoritative durable error must not expire the waiter early"
        );

        // Simulate a concurrent decider publishing slightly later. Injecting
        // directly into the watch sender avoids `decide`'s `AlreadyDecided`
        // precondition: after a lost expiry CAS the entry stays Pending until
        // the authoritative re-read resolves, so a real `decide` past the
        // deadline would still be rejected.
        let decision = approved();
        {
            let mut entries = store.inner.entries.write().await;
            let entry = entries
                .get_mut(&id)
                .expect("entry survives until recovery resolves");
            let _ = entry.tx.send(Some(decision.clone()));
        }

        let result = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("waiter completes")
            .expect("waiter task");
        assert_eq!(
            result, decision,
            "a late watch decision must beat the non-authoritative durable error"
        );
        assert_eq!(persistence.load_calls.load(Ordering::SeqCst), 1);
    }

    /// I45-C (a): when the grace sleep fires at the same instant a concurrent
    /// durable commit publishes its decision on the watch, the waiter must adopt
    /// that decision instead of expiring (DB=Approved / waiter=Expired split
    /// brain). The re-read is gated so `durable_fallback` stays `None` and the
    /// only rescue path is the final watch re-check inside the Grace arm, which
    /// is reached after the sleep. No real DB and no `decide` preconditions:
    /// the watch send is injected directly into the entry's sender, exactly as
    /// in the I42-A (a) regression.
    #[tokio::test]
    async fn grace_deadline_late_watch_beats_expired() {
        #[derive(Debug)]
        struct GatedRereadPersistence {
            started: Arc<AtomicBool>,
            gate: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
        }

        #[async_trait::async_trait]
        impl ApprovalPersistence for GatedRereadPersistence {
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

            async fn load_decision_with_time(
                &self,
                _id: Uuid,
            ) -> Result<Option<LoadedDecision>, ApprovalError> {
                self.started.store(true, Ordering::SeqCst);
                if let Some(rx) = self.gate.lock().await.take() {
                    let _ = rx.await;
                }
                // Non-authoritative: the durable re-read itself carries no
                // decision, so only the watch can rescue the waiter.
                Ok(None)
            }

            async fn mark_expired(
                &self,
                id: Uuid,
                _expired_at: DateTime<Utc>,
            ) -> Result<(), ApprovalError> {
                Err(ApprovalError::AlreadyDecided(id))
            }
        }

        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let started = Arc::new(AtomicBool::new(false));
        let persistence = Arc::new(GatedRereadPersistence {
            started: started.clone(),
            gate: tokio::sync::Mutex::new(Some(release_rx)),
        });
        let store = ApprovalStore::with_persistence(
            Duration::from_millis(30),
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

        // Wait until the lost-CAS recovery has started the gated re-read.
        for _ in 0..200 {
            if started.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            started.load(Ordering::SeqCst),
            "the durable re-read must have started"
        );

        // Let the grace deadline fire while the re-read is still blocked. The
        // waiter is now inside the Grace arm's final bounded wait; publishing
        // the decision here is the deadline-boundary race that used to resolve
        // to `Expired` without ever looking at the watch again.
        tokio::time::sleep(RECOVERY_GRACE + Duration::from_millis(70)).await;
        assert!(
            !waiter.is_finished(),
            "the waiter must still be waiting on the gated re-read"
        );

        let decision = approved();
        {
            let mut entries = store.inner.entries.write().await;
            let entry = entries
                .get_mut(&id)
                .expect("entry survives until recovery resolves");
            let _ = entry.tx.send(Some(decision.clone()));
        }
        let _ = release_tx.send(());

        let result = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("waiter completes")
            .expect("waiter task");
        assert_eq!(
            result, decision,
            "a watch decision published at the grace boundary must beat Expired"
        );
    }

    /// I42-A (b): a durable re-read that never resolves (stalled DB RTT) must
    /// be cut off by the timeout wrapper, converging to `Expired` instead of
    /// waiting forever. The sender is deliberately kept alive, so the gate
    /// future stays pending for the whole test.
    #[tokio::test]
    async fn lost_expiry_cas_reread_timeout_falls_back_to_expired() {
        let (_release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let committed = LoadedDecision {
            decision: approved(),
            decided_at: Utc::now(),
        };
        let started = Arc::new(AtomicBool::new(false));
        let persistence = Arc::new(RereadDecisionPersistence {
            committed,
            started: started.clone(),
            gate: tokio::sync::Mutex::new(Some(release_rx)),
        });
        let store = ApprovalStore::with_persistence(
            Duration::from_millis(30),
            DEFAULT_MAX_PENDING,
            persistence,
        );

        // Without the timeout wrapper the re-read gate never opens, so this
        // outer timeout would fire and fail the test. With the bound in place
        // the waiter returns Expired after grace + one bounded re-read wait.
        let decision = tokio::time::timeout(Duration::from_secs(1), store.request(make_request()))
            .await
            .expect("waiter must be bounded by the re-read timeout");
        assert!(
            matches!(decision, ApprovalDecision::Expired),
            "a timed-out re-read must converge to Expired: {decision:?}"
        );
        assert!(
            started.load(Ordering::SeqCst),
            "the parallel durable re-read must have started"
        );
    }

    // ---- I43-B (a)/(b) durable expiry regressions (mock, no real DB) -------

    /// Mock that fails the first N `mark_expired` calls with the dedicated
    /// `ApprovalError::Persistence` (HTTP 503 class), then commits. No DB.
    #[derive(Debug, Default)]
    struct FlakyExpiryPersistence {
        fail_remaining: AtomicUsize,
        mark_calls: AtomicUsize,
        expired: tokio::sync::Mutex<Vec<Uuid>>,
    }

    #[async_trait::async_trait]
    impl ApprovalPersistence for FlakyExpiryPersistence {
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

        async fn load_decision(
            &self,
            _id: Uuid,
        ) -> Result<Option<ApprovalDecision>, ApprovalError> {
            Ok(None)
        }

        async fn mark_expired(
            &self,
            id: Uuid,
            _expired_at: DateTime<Utc>,
        ) -> Result<(), ApprovalError> {
            self.mark_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_remaining.load(Ordering::SeqCst) > 0 {
                self.fail_remaining.fetch_sub(1, Ordering::SeqCst);
                return Err(ApprovalError::Persistence);
            }
            self.expired.lock().await.push(id);
            Ok(())
        }
    }

    /// I43-B (a): a failed durable expiry must not stamp the in-memory entry as
    /// decided. The waiter still gets `Expired` (legacy contract), but the entry
    /// stays Pending so the next `prune` retries the audit write instead of
    /// dropping the entry via the `is_expired` branch.
    #[tokio::test]
    async fn failed_expiry_commit_keeps_entry_pending_for_prune_retry() {
        let persistence = Arc::new(FlakyExpiryPersistence {
            fail_remaining: AtomicUsize::new(1),
            ..Default::default()
        });
        let store = ApprovalStore::with_persistence(
            Duration::from_millis(500),
            DEFAULT_MAX_PENDING,
            persistence.clone(),
        );
        let mut request = make_request();
        request.expires_at = Utc::now() + chrono::Duration::milliseconds(30);
        let id = request.id;

        assert!(
            matches!(store.request(request).await, ApprovalDecision::Expired),
            "the caller still receives Expired when the audit write fails"
        );
        assert_eq!(persistence.mark_calls.load(Ordering::SeqCst), 1);
        assert!(
            matches!(store.status(id).await, Ok(ApprovalDecision::Pending)),
            "a failed audit write must leave the in-memory entry Pending"
        );

        // A fresh request triggers `prune` at the head of `try_request`.
        let next = make_request();
        let next_id = next.id;
        let store_for_waiter = store.clone();
        let waiter = tokio::spawn(async move { store_for_waiter.request(next).await });
        decide_after_registration(&store, next_id, approved()).await;
        assert!(matches!(
            waiter.await.expect("waiter task"),
            ApprovalDecision::Approved { .. }
        ));

        assert_eq!(
            persistence.mark_calls.load(Ordering::SeqCst),
            2,
            "prune must retry mark_expired for the still-pending entry"
        );
        assert_eq!(
            *persistence.expired.lock().await,
            vec![id],
            "the audit write must succeed on the retry"
        );
        assert!(
            matches!(store.status(id).await, Err(ApprovalError::NotFound(_))),
            "the entry is evicted only after the commit succeeded"
        );
    }

    /// Ghost-entry double: `mark_expired` reports `AlreadyDecided` (a concurrent
    /// settlement won the CAS) while `load_decision_with_time` returns the
    /// authoritative decision. No database and no implicit watch send.
    #[derive(Debug)]
    struct GhostPrunePersistence {
        committed: LoadedDecision,
        reread_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ApprovalPersistence for GhostPrunePersistence {
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

        async fn load_decision(
            &self,
            _id: Uuid,
        ) -> Result<Option<ApprovalDecision>, ApprovalError> {
            Ok(Some(self.committed.decision.clone()))
        }

        async fn load_decision_with_time(
            &self,
            _id: Uuid,
        ) -> Result<Option<LoadedDecision>, ApprovalError> {
            self.reread_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Some(self.committed.clone()))
        }

        async fn mark_expired(
            &self,
            id: Uuid,
            _expired_at: DateTime<Utc>,
        ) -> Result<(), ApprovalError> {
            Err(ApprovalError::AlreadyDecided(id))
        }
    }

    /// I43-B (b): `prune` must treat `AlreadyDecided` as success, reflect the
    /// authoritative decision into memory and evict the ghost instead of
    /// retaining it forever (aligning with reconcile and timeout).
    #[tokio::test]
    async fn prune_evicts_ghost_on_already_decided_and_adopts_authoritative_decision() {
        let committed = LoadedDecision {
            decision: approved(),
            decided_at: Utc::now() - chrono::Duration::seconds(5),
        };
        let persistence = Arc::new(GhostPrunePersistence {
            committed: committed.clone(),
            reread_calls: AtomicUsize::new(0),
        });
        let store = ApprovalStore::with_persistence(
            Duration::from_millis(500),
            DEFAULT_MAX_PENDING,
            persistence.clone(),
        );

        // Install a pending entry whose deadline has passed but whose waiter
        // never ran the timeout path: exactly the ghost `prune` must settle.
        let ghost = make_request();
        let id = ghost.id;
        let (tx, rx) = watch::channel(None);
        {
            let mut entries = store.inner.entries.write().await;
            entries.insert(
                id,
                Entry {
                    request: ghost,
                    decision: None,
                    decided_at: None,
                    deadline: Utc::now() - chrono::Duration::seconds(1),
                    deciding: new_deciding(),
                    tx,
                },
            );
        }

        // A fresh request triggers `prune`; its own waiter is decided at once.
        let next = make_request();
        let next_id = next.id;
        let store_for_waiter = store.clone();
        let waiter = tokio::spawn(async move { store_for_waiter.request(next).await });
        decide_after_registration(&store, next_id, approved()).await;
        assert!(matches!(
            waiter.await.expect("waiter task"),
            ApprovalDecision::Approved { .. }
        ));

        assert_eq!(persistence.reread_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            rx.borrow().as_ref(),
            Some(&committed.decision),
            "prune must publish the authoritative decision before evicting"
        );
        assert!(
            matches!(store.status(id).await, Err(ApprovalError::NotFound(_))),
            "the ghost entry must be evicted, not retained forever"
        );
    }

    // ---- R4-C: RAII deciding claim / cancelled-decide recovery -------------

    /// The guard must release its claim on every exit path, including a plain
    /// scope exit that models an aborted future. No timer or lock is involved.
    #[test]
    fn deciding_guard_releases_claim_on_drop() {
        let claim = new_deciding();
        assert!(!claim.is_claimed());
        {
            let _guard = DecidingGuard(Arc::clone(&claim));
            claim.set(true);
            assert!(claim.is_claimed());
        }
        assert!(
            !claim.is_claimed(),
            "Drop must release the claim unconditionally"
        );
    }

    /// Mock that (a) replays one live recovered hold with no waiter, (b) gates
    /// `record_decision` so an operator `decide` can be cancelled mid-commit,
    /// and (c) records `mark_expired` so the test can prove `prune` settled the
    /// abandoned hold durably. No database is contacted.
    #[derive(Debug, Default)]
    struct CancelledDecidePersistence {
        live: tokio::sync::Mutex<Vec<ApprovalRequest>>,
        entered: Arc<AtomicBool>,
        expired: tokio::sync::Mutex<Vec<Uuid>>,
        gate: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    }

    #[async_trait::async_trait]
    impl ApprovalPersistence for CancelledDecidePersistence {
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
            self.entered.store(true, Ordering::SeqCst);
            if let Some(rx) = self.gate.lock().await.take() {
                // Cancelling the `decide` future drops this await, so no
                // in-memory decision is published and no audit row is written.
                let _ = rx.await;
            }
            Ok(())
        }

        async fn load_pending(&self) -> Result<Vec<ApprovalRequest>, ApprovalError> {
            Ok(self.live.lock().await.clone())
        }

        async fn load_decision(
            &self,
            _id: Uuid,
        ) -> Result<Option<ApprovalDecision>, ApprovalError> {
            Ok(None)
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

    /// R4-C regression: a recovered hold has no waiter, so nothing runs its
    /// timeout path. If an operator `decide` is cancelled while its durable
    /// commit is in flight, the old raw `bool` stayed `true` forever: `prune`
    /// skipped the entry and a retry got 409, leaking the slot until restart.
    /// The RAII claim must clear on cancellation, leaving the entry Pending so
    /// `prune` can commit `mark_expired` and evict it.
    #[tokio::test]
    async fn cancelled_decide_on_waiterless_entry_is_reclaimed_by_prune() {
        let (_gate_tx, gate_rx) = tokio::sync::oneshot::channel::<()>();
        let hold = make_request();
        let hold_id = hold.id;
        let persistence = Arc::new(CancelledDecidePersistence {
            live: tokio::sync::Mutex::new(vec![hold]),
            entered: Arc::new(AtomicBool::new(false)),
            expired: tokio::sync::Mutex::new(Vec::new()),
            gate: tokio::sync::Mutex::new(Some(gate_rx)),
        });
        let store = ApprovalStore::with_persistence(
            Duration::from_secs(5),
            DEFAULT_MAX_PENDING,
            persistence.clone(),
        );
        store.reconcile_pending().await.expect("reconcile");

        // No receiver: exactly the startup-recovery shape from `reconcile_pending`.
        let decider = {
            let store = store.clone();
            tokio::spawn(async move { store.decide(hold_id, approved()).await })
        };
        for _ in 0..200 {
            if persistence.entered.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            persistence.entered.load(Ordering::SeqCst),
            "the operator decide must have reached the gated commit"
        );

        // Cancel mid-commit. `record_decision` never returned, so no decision
        // was published; `Drop` of the guard must still release the claim.
        decider.abort();
        let _ = decider.await;
        {
            let entries = store.inner.entries.read().await;
            let entry = entries
                .get(&hold_id)
                .expect("recovered entry is still in the map");
            assert!(
                !entry.is_deciding(),
                "a cancelled decide must release its deciding claim"
            );
            assert!(
                entry.decision.is_none(),
                "an uncommitted decide leaves the entry Pending"
            );
        }

        // Fast-forward past the recovered deadline instead of sleeping: prune's
        // classification is purely deadline-based. A fresh request runs the
        // real lazy sweep at the head of `try_request`.
        {
            let mut entries = store.inner.entries.write().await;
            if let Some(entry) = entries.get_mut(&hold_id) {
                entry.deadline = Utc::now() - chrono::Duration::seconds(1);
            }
        }
        let next = make_request();
        let next_id = next.id;
        let store_for_waiter = store.clone();
        let waiter = tokio::spawn(async move { store_for_waiter.request(next).await });
        decide_after_registration(&store, next_id, approved()).await;
        let _ = waiter.await.expect("waiter task");

        assert!(
            persistence.expired.lock().await.contains(&hold_id),
            "prune must commit mark_expired for the abandoned recovered hold"
        );
        assert!(
            matches!(store.status(hold_id).await, Err(ApprovalError::NotFound(_))),
            "the settled entry must be evicted, not leak the approval slot"
        );
        assert!(
            store.list_pending().await.iter().all(|r| r.id != hold_id),
            "the abandoned hold must no longer count against max_pending"
        );
    }

    // ---- R5-C: lock-free prune commit keeps the approval API responsive ----

    /// Mock whose `mark_expired` can be held open behind a gate. No database.
    #[derive(Debug, Default)]
    struct GatedExpiryPersistence {
        entered: Arc<AtomicBool>,
        expired: tokio::sync::Mutex<Vec<Uuid>>,
        gate: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    }

    #[async_trait::async_trait]
    impl ApprovalPersistence for GatedExpiryPersistence {
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

        async fn load_decision(
            &self,
            _id: Uuid,
        ) -> Result<Option<ApprovalDecision>, ApprovalError> {
            Ok(None)
        }

        async fn mark_expired(
            &self,
            id: Uuid,
            _expired_at: DateTime<Utc>,
        ) -> Result<(), ApprovalError> {
            self.entered.store(true, Ordering::SeqCst);
            if let Some(rx) = self.gate.lock().await.take() {
                // A real backend may reconnect and hang here. The pre-fix
                // prune held `entries.write()` across exactly this await.
                let _ = rx.await;
            }
            self.expired.lock().await.push(id);
            Ok(())
        }
    }

    /// R5-C regression: while prune's durable commit is blocked, `status()`
    /// must still complete. The old code held the store's write guard across
    /// `mark_expired`, so this call timed out behind the gate and the whole
    /// approval API froze whenever the database hung.
    #[tokio::test]
    async fn status_responds_while_prune_commit_is_gated() {
        let (gate_tx, gate_rx) = tokio::sync::oneshot::channel::<()>();
        let entered = Arc::new(AtomicBool::new(false));
        let persistence = Arc::new(GatedExpiryPersistence {
            entered: entered.clone(),
            expired: tokio::sync::Mutex::new(Vec::new()),
            gate: tokio::sync::Mutex::new(Some(gate_rx)),
        });
        let store = ApprovalStore::with_persistence(
            Duration::from_secs(5),
            DEFAULT_MAX_PENDING,
            persistence.clone(),
        );

        // Recovered-hold shape: pending, no waiter, deadline already past.
        let hold = make_request();
        let hold_id = hold.id;
        let (tx, _rx) = watch::channel(None);
        {
            let mut entries = store.inner.entries.write().await;
            entries.insert(
                hold_id,
                Entry {
                    request: hold,
                    decision: None,
                    decided_at: None,
                    deadline: Utc::now() - chrono::Duration::seconds(1),
                    deciding: new_deciding(),
                    tx,
                },
            );
        }

        // A fresh request drives the lazy sweep at the head of `try_request`;
        // its own waiter is decided as soon as it is registered.
        let next = make_request();
        let next_id = next.id;
        let store_for_waiter = store.clone();
        let waiter = tokio::spawn(async move { store_for_waiter.request(next).await });

        for _ in 0..200 {
            if entered.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            entered.load(Ordering::SeqCst),
            "prune must have reached the gated mark_expired"
        );

        // This is the assertion that fails on the pre-fix code: the write guard
        // was held across the gated await, so a read guard could not be taken.
        let observed = tokio::time::timeout(Duration::from_millis(500), store.status(hold_id))
            .await
            .expect("status must respond while prune's commit is gated");
        assert!(
            matches!(observed, Ok(ApprovalDecision::Pending)),
            "the entry stays Pending until the commit succeeds: {observed:?}"
        );

        let _ = gate_tx.send(());
        decide_after_registration(&store, next_id, approved()).await;
        assert!(matches!(
            waiter.await.expect("waiter task"),
            ApprovalDecision::Approved { .. }
        ));
        assert!(
            persistence.expired.lock().await.contains(&hold_id),
            "the gated expiry must still be committed before eviction"
        );
        assert!(matches!(
            store.status(hold_id).await,
            Err(ApprovalError::NotFound(_))
        ));
    }
}
