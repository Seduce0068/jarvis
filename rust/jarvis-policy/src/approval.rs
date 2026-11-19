//! In-memory approval workflow.
//!
//! A request registers a pending entry and waits until a decision is made
//! (via the HTTP approve/deny endpoints) or the configured timeout elapses.
//! Decisions are held in memory; PostgreSQL-backed audit persistence and
//! user notification (HA/phone) will be layered on top later.
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
}

/// Reason attached to the legacy `Denied` decision returned by
/// [`ApprovalStore::request`] when the queue is exhausted.
///
/// New callers should match [`ApprovalError::QueueFull`] from
/// [`ApprovalStore::try_request`] instead of comparing this string.
pub const QUEUE_FULL_REASON: &str = "approval queue full";

struct Entry {
    request: ApprovalRequest,
    decision: Option<ApprovalDecision>,
    decided_at: Option<DateTime<Utc>>,
    /// The authoritative deadline computed at registration (the min of the
    /// configured timeout and the caller's `expires_at`). Stored so `prune`
    /// can reclaim pending entries whose waiter was abandoned before it ever
    /// reached the timeout branch.
    deadline: DateTime<Utc>,
    tx: watch::Sender<Option<ApprovalDecision>>,
}

struct Inner {
    timeout: Duration,
    max_pending: usize,
    entries: RwLock<HashMap<Uuid, Entry>>,
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
        Self {
            inner: Arc::new(Inner {
                timeout,
                max_pending,
                entries: RwLock::new(HashMap::new()),
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
            Self::prune(&mut entries, self.inner.timeout, now);
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
                    request,
                    decision: None,
                    decided_at: None,
                    deadline,
                    tx,
                },
            );
        }

        let remaining = (deadline - Utc::now()).to_std().unwrap_or(Duration::ZERO);
        let wait = async {
            loop {
                let current = {
                    let guard = rx.borrow_and_update();
                    (*guard).clone()
                };
                if let Some(decision) = current {
                    return decision;
                }
                if rx.changed().await.is_err() {
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
                // Re-check under the lock: a decision may have raced the timeout.
                let mut entries = self.inner.entries.write().await;
                if let Some(entry) = entries.get_mut(&id) {
                    if let Some(decision) = &entry.decision {
                        return Ok(decision.clone());
                    }
                    entry.decision = Some(ApprovalDecision::Expired);
                    entry.decided_at = Some(Utc::now());
                }
                ApprovalDecision::Expired
            }
        };
        Ok(decision)
    }

    /// Record `decision` for a pending request and wake its waiter.
    pub async fn decide(&self, id: Uuid, decision: ApprovalDecision) -> Result<(), ApprovalError> {
        let mut entries = self.inner.entries.write().await;
        let entry = entries.get_mut(&id).ok_or(ApprovalError::NotFound(id))?;
        if entry.decision.is_some() {
            return Err(ApprovalError::AlreadyDecided(id));
        }
        entry.decision = Some(decision.clone());
        entry.decided_at = Some(Utc::now());
        let _ = entry.tx.send(Some(decision));
        Ok(())
    }

    /// Current decision for `id`; `Pending` while still awaiting one.
    pub async fn status(&self, id: Uuid) -> Result<ApprovalDecision, ApprovalError> {
        let entries = self.inner.entries.read().await;
        let entry = entries.get(&id).ok_or(ApprovalError::NotFound(id))?;
        Ok(entry.decision.clone().unwrap_or(ApprovalDecision::Pending))
    }

    /// Requests still awaiting a decision.
    pub async fn list_pending(&self) -> Vec<ApprovalRequest> {
        let entries = self.inner.entries.read().await;
        entries
            .values()
            .filter(|entry| entry.decision.is_none())
            .map(|entry| entry.request.clone())
            .collect()
    }

    /// Drop entries that can no longer produce a real decision.
    ///
    /// Besides the decided-entry retention window (older than twice the
    /// timeout), two kinds of expired entries are reclaimed:
    ///   * entries already recorded as [`ApprovalDecision::Expired`];
    ///   * pending entries (`decided_at.is_none()`) whose deadline has passed.
    ///
    /// The second case matters when a waiter is cancelled (e.g. the client
    /// disconnects) before `try_request` reaches its timeout branch: without
    /// this, the entry keeps `decision = None` forever and permanently eats a
    /// `max_pending` slot. Called at the front of [`Self::try_request`], so no
    /// periodic task is needed.
    fn prune(entries: &mut HashMap<Uuid, Entry>, timeout: Duration, now: DateTime<Utc>) {
        let retention = timeout.as_secs().saturating_mul(2).min(i64::MAX as u64) as i64;
        let cutoff = now - chrono::Duration::seconds(retention);
        entries.retain(|_, entry| {
            let expired = matches!(entry.decision.as_ref(), Some(ApprovalDecision::Expired));
            let deadline_passed = entry.decided_at.is_none() && now > entry.deadline;
            if expired || deadline_passed {
                // Wake a still-waiting receiver with the same decision the
                // ordinary timeout path produces. Dropping the entry (and its
                // sender) would also end the wait via the closed channel, but
                // an explicit send keeps this path consistent with `decide`
                // and the timeout branch. Ignore send errors: no receiver means
                // the waiter already went away.
                let _ = entry.tx.send(Some(ApprovalDecision::Expired));
                return false;
            }
            match entry.decided_at {
                Some(decided_at) => decided_at > cutoff,
                None => true,
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
