//! PostgreSQL-backed approval persistence (I30-A, 案A).
//!
//! This module reuses the schema from `rust/migrations/001_initial.sql` living
//! in the shared JARVIS database: `approval_requests` for the durable CAS and
//! `audit_log` for the compliance record. A decision is committed in **one**
//! transaction: the `UPDATE ... WHERE decided_at IS NULL RETURNING ...` CAS and
//! the `audit_log` INSERT either both succeed or both roll back.
//!
//! The [`ApprovalStore`](crate::approval::ApprovalStore) publishes the decision
//! to its `watch` channel only after [`ApprovalPersistence::record_decision`]
//! returns `Ok`, which for this backend means after `Transaction::commit`.
//! At startup the same transaction is reused by `mark_expired` to settle holds
//! that expired while the service was down.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::approval::{ApprovalError, ApprovalPersistence};
use crate::policy::{ApprovalDecision, ApprovalRequest};

/// PostgreSQL implementation of [`ApprovalPersistence`].
///
/// A dedicated `tokio_postgres::Client` is held rather than a pool so the CAS
/// and the audit INSERT always run on the same connection (a transaction cannot
/// span connections). Approval traffic is human-paced, so a single connection
/// is sufficient and keeps atomicity trivial.
pub struct PgApprovalPersistence {
    // `tokio_postgres::Client::transaction` needs `&mut self`, but the
    // `ApprovalPersistence` trait is object-safe with `&self`; the mutex
    // reconciles the two while keeping CAS + audit INSERT on one connection.
    client: tokio::sync::Mutex<tokio_postgres::Client>,
    /// Kept alive for the lifetime of the backend: dropping the handle aborts
    /// the task that drives the connection, which would make every query fail.
    _connection: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for PgApprovalPersistence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print connection details: the URL can carry credentials.
        f.debug_struct("PgApprovalPersistence")
            .field("connected", &true)
            .finish_non_exhaustive()
    }
}

impl PgApprovalPersistence {
    /// Open a connection and spawn the driver task required by tokio-postgres.
    ///
    /// The URL is never echoed into the returned error: connection strings may
    /// embed credentials, and startup logs must not leak them.
    pub async fn connect(pg_url: &str) -> anyhow::Result<Self> {
        let (client, connection) = tokio_postgres::connect(pg_url, tokio_postgres::NoTls)
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "failed to connect to PostgreSQL for approval persistence; \
                     check JARVIS_POLICY_PG_URL"
                )
            })?;

        let connection = tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::error!(%error, "approval persistence connection task ended");
            }
        });

        Ok(Self {
            client: tokio::sync::Mutex::new(client),
            _connection: connection,
        })
    }

    /// Run the CAS + audit INSERT in a single transaction.
    ///
    /// Returns `Ok(())` only after the commit succeeded. A zero-row CAS returns
    /// [`ApprovalError::AlreadyDecided`]; any other database failure is reported
    /// through [`ApprovalError::QueueFull`], the boundary's existing transient
    /// infrastructure channel (HTTP 503, retry later). Adding a dedicated
    /// `Persistence` variant would require editing the transport match, which is
    /// outside this slice's scope, so the transient variant is reused here.
    async fn commit_decision(
        &self,
        id: Uuid,
        decision: &ApprovalDecision,
        decided_at: DateTime<Utc>,
    ) -> Result<(), ApprovalError> {
        let (decision_label, decided_by, reason) = decision_columns(decision);

        let mut client = self.client.lock().await;
        let tx = client.transaction().await.map_err(db_failure)?;

        // CAS: a concurrent decider that committed first makes this UPDATE touch
        // zero rows, because the WHERE condition is re-evaluated after the row
        // lock is released. Dropping `tx` on the zero-row path rolls back.
        let updated = tx
            .query_opt(
                "UPDATE approval_requests
                    SET decision = $2, decided_at = $3, decided_by = $4, reason = $5
                  WHERE id = $1 AND decided_at IS NULL
              RETURNING principal, action, resource, context",
                &[&id, &decision_label, &decided_at, &decided_by, &reason],
            )
            .await
            .map_err(db_failure)?;

        let Some(row) = updated else {
            return Err(ApprovalError::AlreadyDecided(id));
        };

        let principal: String = row.get("principal");
        let action: String = row.get("action");
        let resource: String = row.get("resource");
        let context: serde_json::Value = row.get("context");
        let event_id = Uuid::new_v4();
        let correlation_id = id.to_string();

        // The audit row is written inside the same transaction as the CAS, so
        // a failure here rolls the decision back as well. `decided_by` is added
        // by migration 003 (ADD COLUMN IF NOT EXISTS, replay safe).
        tx.execute(
            "INSERT INTO audit_log
                (event_id, correlation_id, timestamp, principal, action,
                 resource, decision, decided_by, context)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[
                &event_id,
                &correlation_id,
                &decided_at,
                &principal,
                &action,
                &resource,
                &decision_label,
                &decided_by,
                &context,
            ],
        )
        .await
        .map_err(db_failure)?;

        // The caller publishes to `watch` only after this returns Ok.
        tx.commit().await.map_err(db_failure)?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl ApprovalPersistence for PgApprovalPersistence {
    /// Persist the pending request with a single auto-committed INSERT.
    ///
    /// This runs before the store starts waiting (and therefore before any
    /// `decide` CAS), so on the success path the row exists when the decision
    /// arrives. On a failed INSERT, [`db_failure`] logs the error and returns
    /// [`ApprovalError::QueueFull`]; the store then rolls back the in-memory
    /// registration and answers HTTP 503 (retry). This fail-closed choice
    /// (R4-PERSIST) prevents an orphan hold whose later `decide` would hit a
    /// zero-row CAS and surface as `AlreadyDecided` (409) with no audit row.
    async fn record_request(
        &self,
        request: &ApprovalRequest,
        deadline: DateTime<Utc>,
    ) -> Result<(), ApprovalError> {
        self.client
            .lock()
            .await
            .execute(
                "INSERT INTO approval_requests
                    (id, principal, action, resource, context, requested_at, expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (id) DO NOTHING",
                &[
                    &request.id,
                    &request.principal,
                    &request.action,
                    &request.resource,
                    &request.context,
                    &request.requested_at,
                    &deadline,
                ],
            )
            .await
            .map_err(db_failure)?;
        Ok(())
    }

    /// CAS + audit INSERT + commit in one transaction (see `commit_decision`).
    async fn record_decision(
        &self,
        id: Uuid,
        decision: &ApprovalDecision,
        decided_at: DateTime<Utc>,
    ) -> Result<(), ApprovalError> {
        self.commit_decision(id, decision, decided_at).await
    }

    /// Return every undecided row, including holds whose `expires_at` has
    /// passed. `ApprovalStore::reconcile_pending` keeps those with
    /// `expires_at > now` as "no waiter" entries and settles the rest through
    /// `mark_expired`; narrowing this query to live rows would leave expired
    /// holds undecided and unaudited after a restart.
    async fn load_pending(&self) -> Result<Vec<ApprovalRequest>, ApprovalError> {
        let rows = self
            .client
            .lock()
            .await
            .query(
                "SELECT id, principal, action, resource, context, requested_at, expires_at
                   FROM approval_requests
                  WHERE decided_at IS NULL",
                &[],
            )
            .await
            .map_err(db_failure)?;

        Ok(rows
            .into_iter()
            .map(|row| ApprovalRequest {
                id: row.get("id"),
                principal: row.get("principal"),
                action: row.get("action"),
                resource: row.get("resource"),
                context: row.get("context"),
                requested_at: row.get("requested_at"),
                expires_at: row.get("expires_at"),
            })
            .collect())
    }

    /// Record a timeout the same way as any other decision, with a single
    /// transactional CAS + audit INSERT. The store does not send a watch update
    /// on this path (the timing-out waiter already returns `Expired`).
    async fn mark_expired(&self, id: Uuid, expired_at: DateTime<Utc>) -> Result<(), ApprovalError> {
        self.commit_decision(id, &ApprovalDecision::Expired, expired_at)
            .await
    }
}

/// Project a decision onto the `decision` / `decided_by` / `reason` columns used
/// by both `approval_requests` and `audit_log`.
fn decision_columns(decision: &ApprovalDecision) -> (&'static str, Option<&str>, Option<&str>) {
    match decision {
        ApprovalDecision::Approved { approved_by, .. } => {
            ("approved", Some(approved_by.as_str()), None)
        }
        ApprovalDecision::Denied { reason } => ("denied", None, Some(reason.as_str())),
        ApprovalDecision::Expired => ("expired", None, None),
        ApprovalDecision::Pending => ("pending", None, None),
    }
}

/// Log the database error for operators, but never surface its raw text: a
/// tokio-postgres error may embed statement/connection details.
fn db_failure(error: tokio_postgres::Error) -> ApprovalError {
    tracing::error!(error = %error, "approval persistence operation failed");
    ApprovalError::QueueFull
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_columns_match_audit_vocabulary() {
        let approved = ApprovalDecision::Approved {
            approved_by: "alice".into(),
            approved_at: Utc::now(),
        };
        let (label, by, reason) = decision_columns(&approved);
        assert_eq!(label, "approved");
        assert_eq!(by, Some("alice"));
        assert_eq!(reason, None);

        let denied = ApprovalDecision::Denied {
            reason: "policy".into(),
        };
        let (label, by, reason) = decision_columns(&denied);
        assert_eq!(label, "denied");
        assert_eq!(by, None);
        assert_eq!(reason, Some("policy"));

        let (label, by, reason) = decision_columns(&ApprovalDecision::Expired);
        assert_eq!(label, "expired");
        assert_eq!(by, None);
        assert_eq!(reason, None);
    }
}
