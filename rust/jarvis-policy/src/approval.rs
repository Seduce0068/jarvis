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
}

struct Entry {
    request: ApprovalRequest,
    decision: Option<ApprovalDecision>,
    decided_at: Option<DateTime<Utc>>,
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

    /// Register `request` and wait for a decision.
    ///
    /// Returns the recorded decision, or [`ApprovalDecision::Expired`] when no
    /// decision arrives within the configured timeout (or the request's own
    /// `expires_at`, whichever comes first).
    pub async fn request(&self, request: ApprovalRequest) -> ApprovalDecision {
        let id = request.id;
        let now = Utc::now();
        let timeout = chrono::Duration::from_std(self.inner.timeout)
            .unwrap_or_else(|_| chrono::Duration::seconds(300));
        let deadline = if request.expires_at > now {
            std::cmp::min(now + timeout, request.expires_at)
        } else {
            now + timeout
        };

        let (tx, mut rx) = watch::channel(None);
        {
            let mut entries = self.inner.entries.write().await;
            Self::prune(&mut entries, self.inner.timeout, now);
            // Fail closed when the queue is saturated: an attacker on
            // loopback must not be able to grow pending state without limit.
            let pending = entries
                .values()
                .filter(|entry| entry.decision.is_none())
                .count();
            if pending >= self.inner.max_pending {
                return ApprovalDecision::Denied {
                    reason: "approval queue full".into(),
                };
            }
            entries.insert(
                id,
                Entry {
                    request,
                    decision: None,
                    decided_at: None,
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

        match decision {
            Some(decision) => decision,
            None => {
                // Re-check under the lock: a decision may have raced the timeout.
                let mut entries = self.inner.entries.write().await;
                if let Some(entry) = entries.get_mut(&id) {
                    if let Some(decision) = &entry.decision {
                        return decision.clone();
                    }
                    entry.decision = Some(ApprovalDecision::Expired);
                    entry.decided_at = Some(Utc::now());
                }
                ApprovalDecision::Expired
            }
        }
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

    /// Drop decided entries that are older than twice the timeout.
    fn prune(entries: &mut HashMap<Uuid, Entry>, timeout: Duration, now: DateTime<Utc>) {
        let retention = timeout.as_secs().saturating_mul(2).min(i64::MAX as u64) as i64;
        let cutoff = now - chrono::Duration::seconds(retention);
        entries.retain(|_, entry| match entry.decided_at {
            Some(decided_at) => decided_at > cutoff,
            None => true,
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

        let second = store.request(make_request()).await;
        assert!(
            matches!(&second, ApprovalDecision::Denied { reason } if reason == "approval queue full"),
            "unexpected: {second:?}"
        );

        store.decide(first_id, approved()).await.expect("decide");
        let _ = waiter.await.expect("waiter task");
    }
}
