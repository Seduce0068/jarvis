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
//!
//! I32-C: the single `tokio_postgres::Client` is treated as a replaceable
//! *connection generation*. A PostgreSQL restart closes that one socket, after
//! which every later operation would fail forever. On a transport error the
//! generation is discarded and a fresh connection is opened, under a bounded
//! retry budget ([`MAX_DB_ATTEMPTS`]), so decisions recover automatically while
//! a genuine outage still ends in the `Persistence`/HTTP 503 channel.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::approval::{ApprovalError, ApprovalPersistence};
use crate::policy::{ApprovalDecision, ApprovalRequest};

/// Upper bound on consecutive attempts for a single persistence operation:
/// one initial attempt plus at most `MAX_DB_ATTEMPTS - 1` reconnects. Keeping
/// this finite prevents a dead PostgreSQL from turning `decide` into an
/// unbounded retry loop; after the budget a transient
/// [`ApprovalError::Persistence`] (HTTP 503, retry later) is returned.
const MAX_DB_ATTEMPTS: usize = 3;

/// Pure retry decision, split out from I/O so it is unit-testable without a
/// database: retry only while another attempt remains in the budget.
fn should_retry(attempt_no: usize, max_attempts: usize) -> bool {
    attempt_no < max_attempts
}

/// One connection generation: the client plus the task driving its socket.
///
/// Both are kept together so a broken generation can be replaced atomically
/// under the outer mutex. Keeping the driver outside the mutex would leave a
/// stale task behind on reconnect (dropping a `JoinHandle` only detaches it).
struct Connection {
    client: tokio_postgres::Client,
    driver: tokio::task::JoinHandle<()>,
}

impl Connection {
    /// Open a fresh generation. The URL is never echoed into the returned
    /// error: connection strings may embed credentials.
    async fn open(pg_url: &str) -> anyhow::Result<Self> {
        let (client, connection) = tokio_postgres::connect(pg_url, tokio_postgres::NoTls)
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "failed to connect to PostgreSQL for approval persistence; \
                     check JARVIS_POLICY_PG_URL"
                )
            })?;

        let driver = tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::error!(%error, "approval persistence connection task ended");
            }
        });

        Ok(Self { client, driver })
    }

    /// Discard this (broken) generation and install a freshly opened one.
    ///
    /// Why reconnect instead of surfacing a permanent error: a
    /// `tokio_postgres::Client` is bound to a single TCP connection, so after a
    /// PostgreSQL restart every later `transaction()`/`execute()`/`query()` on
    /// that client fails forever and all `decide` calls would stay at HTTP 503
    /// indefinitely. The URL is retained solely so the generation can be
    /// rebuilt, and retries are bounded, so a real outage still terminates in
    /// the `Persistence`/503 contract rather than looping forever.
    async fn reconnect(&mut self, pg_url: &str) -> Result<(), ApprovalError> {
        // Abort the dead socket's driver before dropping the generation so no
        // task lingers; dropping a `JoinHandle` alone merely detaches it.
        self.driver.abort();
        *self = Self::open(pg_url).await.map_err(|_| {
            // Never log the URL or the connect error text: both may carry
            // credentials.
            tracing::error!("failed to reconnect approval persistence after connection loss");
            ApprovalError::Persistence
        })?;
        Ok(())
    }
}

/// Outcome of one transactional decision attempt, before mapping to
/// [`ApprovalError`]. `AlreadyDecided` is a legitimate CAS result and is never
/// retried; only transport/SQL errors trigger a reconnect.
enum TransactionOutcome {
    Committed,
    AlreadyDecided,
}

/// PostgreSQL implementation of [`ApprovalPersistence`].
///
/// A dedicated `tokio_postgres::Client` is held rather than a pool so the CAS
/// and the audit INSERT always run on the same connection (a transaction cannot
/// span connections). Approval traffic is human-paced, so a single connection
/// is sufficient and keeps atomicity trivial. The mutex guards that one
/// *connection generation*: on a transport failure the whole generation
/// (client + driver) is swapped for a fresh one, so the CAS + audit INSERT
/// still run on one connection across any reconnect.
pub struct PgApprovalPersistence {
    /// Retained only to rebuild a broken generation. Never logged and never
    /// included in an error: the URL may carry credentials.
    pg_url: String,
    /// `tokio_postgres::Client::transaction` needs `&mut self`, but the
    /// `ApprovalPersistence` trait is object-safe with `&self`; the mutex
    /// reconciles the two while keeping CAS + audit INSERT on one connection.
    connection: tokio::sync::Mutex<Connection>,
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
        let connection = Connection::open(pg_url).await?;
        Ok(Self {
            pg_url: pg_url.to_owned(),
            connection: tokio::sync::Mutex::new(connection),
        })
    }

    /// Run the CAS + audit INSERT in a single transaction.
    ///
    /// Returns `Ok(())` only after the commit succeeded. A zero-row CAS returns
    /// [`ApprovalError::AlreadyDecided`]; any other database failure is mapped by
    /// [`db_failure`] to [`ApprovalError::Persistence`] (HTTP 503, retry later),
    /// which is distinct from [`ApprovalError::QueueFull`] so callers can tell a
    /// persistence outage from queue exhaustion. Raw database text is never
    /// carried in the error.
    ///
    /// A failed transaction (or a broken connection at `transaction()` time) is
    /// retried on a freshly opened generation up to [`MAX_DB_ATTEMPTS`] times;
    /// the whole transaction is redone because the failed one was rolled back.
    async fn commit_decision(
        &self,
        id: Uuid,
        decision: &ApprovalDecision,
        decided_at: DateTime<Utc>,
    ) -> Result<(), ApprovalError> {
        let (decision_label, decided_by, reason) = decision_columns(decision);

        for attempt_no in 1..=MAX_DB_ATTEMPTS {
            let mut guard = self.connection.lock().await;

            // One whole transaction. `AlreadyDecided` is returned as a success
            // of the *attempt* so it is not retried; a `tokio_postgres::Error`
            // from any step drops `tx` (rolling back) and triggers a reconnect.
            // tokio-postgres operation errors never contain the connection URL
            // (that is only ever passed to `connect` and sanitized in
            // `Connection::open`), so logging them keeps the no-URL policy.
            let outcome: Result<TransactionOutcome, tokio_postgres::Error> = async {
                let tx = guard.client.transaction().await?;

                // CAS: a concurrent decider that committed first makes this
                // UPDATE touch zero rows, because the WHERE condition is
                // re-evaluated after the row lock is released. Dropping `tx` on
                // the zero-row path rolls back.
                let updated = tx
                    .query_opt(
                        "UPDATE approval_requests
                            SET decision = $2, decided_at = $3, decided_by = $4, reason = $5
                          WHERE id = $1 AND decided_at IS NULL
                      RETURNING principal, action, resource, context",
                        &[&id, &decision_label, &decided_at, &decided_by, &reason],
                    )
                    .await?;

                let Some(row) = updated else {
                    return Ok(TransactionOutcome::AlreadyDecided);
                };

                let principal: String = row.get("principal");
                let action: String = row.get("action");
                let resource: String = row.get("resource");
                let context: serde_json::Value = row.get("context");
                let event_id = Uuid::new_v4();
                let correlation_id = id.to_string();

                // The audit row is written inside the same transaction as the
                // CAS, so a failure here rolls the decision back as well.
                // `decided_by` is added by migration 003 (ADD COLUMN IF NOT
                // EXISTS, replay safe).
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
                .await?;

                // The caller publishes to `watch` only after this succeeds.
                tx.commit().await?;
                Ok(TransactionOutcome::Committed)
            }
            .await;

            match outcome {
                Ok(TransactionOutcome::Committed) => return Ok(()),
                Ok(TransactionOutcome::AlreadyDecided) => {
                    return Err(ApprovalError::AlreadyDecided(id));
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        attempt_no,
                        "approval decision transaction attempt failed"
                    );
                    if should_retry(attempt_no, MAX_DB_ATTEMPTS) {
                        guard.reconnect(&self.pg_url).await?;
                    } else {
                        return Err(db_failure(error));
                    }
                }
            }
        }
        unreachable!("the final attempt always returns")
    }
}

#[async_trait::async_trait]
impl ApprovalPersistence for PgApprovalPersistence {
    /// Persist the pending request with a single auto-committed INSERT.
    ///
    /// This runs before the store starts waiting (and therefore before any
    /// `decide` CAS), so on the success path the row exists when the decision
    /// arrives. On a failed INSERT, [`db_failure`] logs the error and returns
    /// [`ApprovalError::Persistence`]; the store then rolls back the in-memory
    /// registration and answers HTTP 503 (retry). This fail-closed choice
    /// (R4-PERSIST) prevents an orphan hold whose later `decide` would hit a
    /// zero-row CAS and surface as `AlreadyDecided` (409) with no audit row.
    async fn record_request(
        &self,
        request: &ApprovalRequest,
        deadline: DateTime<Utc>,
    ) -> Result<(), ApprovalError> {
        for attempt_no in 1..=MAX_DB_ATTEMPTS {
            let mut guard = self.connection.lock().await;
            let result = guard
                .client
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
                .await;

            match result {
                Ok(_) => return Ok(()),
                Err(error) => {
                    // Operation errors never contain the connection URL (see
                    // `commit_decision`), so logging them is safe.
                    tracing::warn!(%error, attempt_no, "record_request database attempt failed");
                    if should_retry(attempt_no, MAX_DB_ATTEMPTS) {
                        guard.reconnect(&self.pg_url).await?;
                    } else {
                        return Err(db_failure(error));
                    }
                }
            }
        }
        unreachable!("the final attempt always returns")
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
        for attempt_no in 1..=MAX_DB_ATTEMPTS {
            let mut guard = self.connection.lock().await;
            let result = guard
                .client
                .query(
                    "SELECT id, principal, action, resource, context, requested_at, expires_at
                       FROM approval_requests
                      WHERE decided_at IS NULL",
                    &[],
                )
                .await;

            match result {
                Ok(rows) => {
                    return Ok(rows
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
                        .collect());
                }
                Err(error) => {
                    // Operation errors never contain the connection URL (see
                    // `commit_decision`), so logging them is safe.
                    tracing::warn!(%error, attempt_no, "load_pending database attempt failed");
                    if should_retry(attempt_no, MAX_DB_ATTEMPTS) {
                        guard.reconnect(&self.pg_url).await?;
                    } else {
                        return Err(db_failure(error));
                    }
                }
            }
        }
        unreachable!("the final attempt always returns")
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
    ApprovalError::Persistence
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

    /// Pure retry-budget tests: no database, no connection, no mock. The budget
    /// is `MAX_DB_ATTEMPTS` total tries, so reconnects happen after attempts
    /// 1 and 2 and stop before attempt 3 (which reports `Persistence`).
    #[test]
    fn retry_budget_allows_two_reconnects_then_stops() {
        assert_eq!(MAX_DB_ATTEMPTS, 3);
        assert!(should_retry(1, MAX_DB_ATTEMPTS));
        assert!(should_retry(2, MAX_DB_ATTEMPTS));
        assert!(!should_retry(3, MAX_DB_ATTEMPTS));
        assert!(!should_retry(4, MAX_DB_ATTEMPTS));
    }

    /// The decision is generic over the budget: a budget of one means no
    /// reconnect, and the first failure is surfaced immediately.
    #[test]
    fn retry_budget_respects_explicit_limits() {
        assert!(!should_retry(1, 1));
        assert!(should_retry(1, 2));
        assert!(!should_retry(2, 2));
        assert!(!should_retry(0, 0));
    }
}
