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
//!
//! I34-D: a successful `commit` whose acknowledgement is lost used to be
//! retried; the retry's zero-row CAS was then reported as `AlreadyDecided`
//! even though this very call had committed (DB `approved`, waiter `Expired`,
//! operator 409). The committed state is now read back both when a transaction
//! attempt fails and when the CAS finds no undecided row: a row carrying the
//! decision we attempted is our own success, while a different decision still
//! reports `AlreadyDecided`. No second audit row is written on that path.
//!
//! I36-B: a transport error that may have hidden a successful `commit` used to
//! give up in two places — a failed reconnect returned `Persistence` before the
//! read-back probe ran, and the probe treated its own errors as `Undecided`,
//! returning `Persistence` on the final attempt without ever opening a fresh
//! connection generation. The post-error read-back now runs on every attempt
//! (including the last) on a fresh generation, and a failed reconnect is not
//! fatal to the probe. A genuine inability to read the durable state still ends
//! in `Persistence`.
//!
//! I38-A: the read-back used to loop [`MAX_DB_ATTEMPTS`] generations *inside*
//! each of the `MAX_DB_ATTEMPTS` transaction attempts (up to nine TCP
//! connections before a 503). Probe and transaction now share one budget:
//! exactly one reconnect + one probe per transaction attempt, so the whole
//! operation touches at most `MAX_DB_ATTEMPTS` generations. A retry's zero-row
//! CAS plus in-transaction read still detects our own lost commit, and the
//! `OurCommit` / `CompetingDecision` classification is unchanged. The new
//! [`ApprovalPersistence::load_decision`] seam also lets the approval store
//! re-read the authoritative decision after a lost expiry CAS.

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

/// The committed decision of one `approval_requests` row, read back from the
/// durable store. `decided_at IS NOT NULL` is the commit marker written by the
/// CAS; the value columns identify *which* decision won, so a lost commit
/// acknowledgement can be told apart from a competing decider.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedDecision {
    decision: String,
    decided_by: Option<String>,
    reason: Option<String>,
}

/// Resolution of a durable-state read after an ambiguous outcome (a lost
/// commit acknowledgement or a zero-row CAS). Pure so it is testable without a
/// database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DurableResolution {
    /// The row is committed with exactly the decision we attempted: our commit
    /// won even though we never observed its acknowledgement.
    OurCommit,
    /// The row is committed with a different decision: a competing decider won.
    CompetingDecision,
    /// No committed decision is visible yet: retry/failure handling continues.
    Undecided,
}

/// Whether a committed row represents the decision this call tried to write.
///
/// `decided_at` is deliberately not compared: it is the commit marker, not an
/// identity token, and a retry can carry a freshly stamped timestamp. Matching
/// the decision label plus its payload is enough to identify "our" commit;
/// when two attempts write the same decision the published value is identical
/// either way, so reporting success is safe and avoids a split brain.
fn is_our_decision(
    recorded: &RecordedDecision,
    decision_label: &str,
    decided_by: Option<&str>,
    reason: Option<&str>,
) -> bool {
    recorded.decision == decision_label
        && recorded.decided_by.as_deref() == decided_by
        && recorded.reason.as_deref() == reason
}

/// Classify a durable-state read against the decision we attempted. The single
/// entry point for both the zero-row CAS path and the post-error probe, so the
/// observable behavior is identical in either case.
fn classify_recorded(
    recorded: Option<&RecordedDecision>,
    decision_label: &str,
    decided_by: Option<&str>,
    reason: Option<&str>,
) -> DurableResolution {
    match recorded {
        None => DurableResolution::Undecided,
        Some(existing) if is_our_decision(existing, decision_label, decided_by, reason) => {
            DurableResolution::OurCommit
        }
        Some(_) => DurableResolution::CompetingDecision,
    }
}

/// Pure decision for one attempt's post-error durable read-back.
///
/// `Ok(recorded)` is a definitive answer from a working connection generation:
/// classify it (reusing [`classify_recorded`]) and stop probing with that
/// resolution — even when it is `Undecided` (a working read showed no committed
/// row, so probing more generations cannot reveal a commit that is not there).
/// `Err(())` is a probe transport error: return `None` so the caller retries the
/// transaction on another generation within the shared [`MAX_DB_ATTEMPTS`]
/// budget (I38-A).
fn probe_iteration_resolution(
    probe: &Result<Option<RecordedDecision>, ()>,
    decision_label: &str,
    decided_by: Option<&str>,
    reason: Option<&str>,
) -> Option<DurableResolution> {
    match probe {
        Ok(recorded) => Some(classify_recorded(
            recorded.as_ref(),
            decision_label,
            decided_by,
            reason,
        )),
        Err(()) => None,
    }
}

/// `decided_at IS NOT NULL` is exactly the state left by a successful CAS +
/// audit commit, so a matching row proves the decision is durable. No URL,
/// payload or reason is ever logged from this read.
const READ_DECISION_SQL: &str = "SELECT decision, decided_by, reason \
                                  FROM approval_requests \
                                 WHERE id = $1 AND decided_at IS NOT NULL";

/// Authoritative decision read for [`ApprovalPersistence::load_decision`]
/// (I38-A). `decided_at` is selected so `Approved.approved_at` can be rebuilt;
/// the `decided_at IS NOT NULL` predicate guarantees only a committed row is
/// returned. No URL, SQL text or decision body is ever logged from this path.
const LOAD_DECISION_SQL: &str = "SELECT decision, decided_by, reason, decided_at \
                                   FROM approval_requests \
                                  WHERE id = $1 AND decided_at IS NOT NULL";

fn recorded_from_row(row: &tokio_postgres::Row) -> RecordedDecision {
    RecordedDecision {
        decision: row.get("decision"),
        decided_by: row.get("decided_by"),
        reason: row.get("reason"),
    }
}

/// Probe on the connection generation that just ran the (failed) transaction.
async fn read_decision_on_client(
    client: &tokio_postgres::Client,
    id: Uuid,
) -> Result<Option<RecordedDecision>, tokio_postgres::Error> {
    let row = client.query_opt(READ_DECISION_SQL, &[&id]).await?;
    Ok(row.as_ref().map(recorded_from_row))
}

/// Probe inside the still-open transaction of a zero-row CAS. Reading through
/// `tx` keeps the check on the same connection/snapshot without writing
/// anything; dropping `tx` afterwards rolls the empty transaction back.
async fn read_decision_in_tx(
    tx: &tokio_postgres::Transaction<'_>,
    id: Uuid,
) -> Result<Option<RecordedDecision>, tokio_postgres::Error> {
    let row = tx.query_opt(READ_DECISION_SQL, &[&id]).await?;
    Ok(row.as_ref().map(recorded_from_row))
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
/// [`ApprovalError`]. Both `Already*` variants are legitimate CAS results and
/// are never retried; only transport/SQL errors trigger a reconnect.
enum TransactionOutcome {
    Committed,
    /// An earlier attempt (or this call's lost-acknowledgement commit) already
    /// wrote the very decision being attempted; safe to report success.
    AlreadyCommittedByUs,
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

    /// Resolve one ambiguous transaction outcome with ONE durable read on a
    /// freshly reconnected generation (I38-A).
    ///
    /// `commit_decision` calls this at most once per transaction attempt, so
    /// the reconnect budget is the *same* [`MAX_DB_ATTEMPTS`] as the
    /// transaction retries — there is no nested probe loop that could multiply
    /// TCP connections before returning 503. The generation that just failed
    /// the transaction may be broken, so reconnect first; a failed reconnect
    /// is *not* fatal, it only leaves the current (possibly stale) generation
    /// in place for the probe. A probe transport error is reported as
    /// `Undecided`, which lets the caller retry the transaction (whose zero-row
    /// CAS then detects our own lost commit) or fail closed on the last try.
    ///
    /// No URL, SQL, decision label or payload is logged: a probe error is
    /// discarded and a reconnect failure is logged generically inside
    /// [`Connection::reconnect`].
    async fn read_back_once(
        &self,
        guard: &mut Connection,
        id: Uuid,
        decision_label: &str,
        decided_by: Option<&str>,
        reason: Option<&str>,
    ) -> DurableResolution {
        // One fresh generation for this probe. Never propagate the reconnect
        // error here: probing must still run on the current generation (I36-B).
        let _ = guard.reconnect(&self.pg_url).await;

        let probe = read_decision_on_client(&guard.client, id)
            .await
            .map_err(|_| ());
        probe_iteration_resolution(&probe, decision_label, decided_by, reason)
            .unwrap_or(DurableResolution::Undecided)
    }

    /// Run the CAS + audit INSERT in a single transaction.
    ///
    /// Returns `Ok(())` only after the commit is known to be durable. A
    /// zero-row CAS normally returns [`ApprovalError::AlreadyDecided`], but a
    /// row committed with the very decision attempted (a lost acknowledgement)
    /// is our own success. Any other database failure is mapped by
    /// [`db_failure`] to [`ApprovalError::Persistence`] (HTTP 503, retry later),
    /// which is distinct from [`ApprovalError::QueueFull`] so callers can tell a
    /// persistence outage from queue exhaustion. Raw database text is never
    /// carried in the error.
    ///
    /// A failed transaction (or a broken connection at `transaction()` time) is
    /// retried on a freshly opened generation up to [`MAX_DB_ATTEMPTS`] times;
    /// the whole transaction is redone because the failed one was rolled back.
    /// Each attempt performs at most one read-back probe on that same fresh
    /// generation, so a commit whose acknowledgement was lost is reported as
    /// success instead of degrading into `AlreadyDecided` / `Expired` split
    /// brain without multiplying the connection budget (I38-A).
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
                    // Zero rows means either a competing decider committed
                    // first, OR a previous attempt of THIS call committed and
                    // only its acknowledgement was lost (the retry now sees
                    // the decided row). Read the committed value inside this
                    // transaction to tell the two apart; the read is read-only
                    // and `tx` is rolled back on return, so no audit row can be
                    // duplicated.
                    let recorded = read_decision_in_tx(&tx, id).await?;
                    return Ok(
                        match classify_recorded(
                            recorded.as_ref(),
                            decision_label,
                            decided_by,
                            reason,
                        ) {
                            DurableResolution::OurCommit => {
                                TransactionOutcome::AlreadyCommittedByUs
                            }
                            DurableResolution::CompetingDecision | DurableResolution::Undecided => {
                                TransactionOutcome::AlreadyDecided
                            }
                        },
                    );
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
                Ok(TransactionOutcome::Committed)
                | Ok(TransactionOutcome::AlreadyCommittedByUs) => return Ok(()),
                Ok(TransactionOutcome::AlreadyDecided) => {
                    return Err(ApprovalError::AlreadyDecided(id));
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        attempt_no,
                        "approval decision transaction attempt failed"
                    );
                    // The error is ambiguous: the server may have committed and
                    // only the acknowledgement was lost. Resolve it with ONE
                    // read-back probe on a fresh generation (I38-A). Probe and
                    // transaction share the same MAX_DB_ATTEMPTS budget: one
                    // reconnect + one probe per attempt, never a nested probe
                    // loop, so a real outage ends in Persistence after at most
                    // MAX_DB_ATTEMPTS connection generations instead of nine.
                    // A failed reconnect does not skip the probe (I36-B).
                    let resolution = self
                        .read_back_once(&mut guard, id, decision_label, decided_by, reason)
                        .await;
                    match resolution {
                        DurableResolution::OurCommit => return Ok(()),
                        DurableResolution::CompetingDecision => {
                            return Err(ApprovalError::AlreadyDecided(id));
                        }
                        DurableResolution::Undecided => {
                            // Either a working generation showed no committed
                            // row, or this generation's probe hit a transport
                            // error. Retry the whole transaction while the
                            // shared budget remains; a retry's zero-row CAS and
                            // in-transaction read also detect our own lost
                            // commit. Otherwise this is genuinely undecidable
                            // -> Persistence.
                            if should_retry(attempt_no, MAX_DB_ATTEMPTS) {
                                continue;
                            }
                            return Err(db_failure(error));
                        }
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

    /// Re-read the committed decision for `id` (I38-A).
    ///
    /// This is the durable authority consulted by the approval store after a
    /// lost expiry CAS. Only rows with `decided_at IS NOT NULL` can match, so a
    /// pending hold is never mistaken for a decision. Retry/reconnect behavior
    /// mirrors `load_pending` and stays inside the single `MAX_DB_ATTEMPTS`
    /// budget; a genuine outage surfaces as `ApprovalError::Persistence`.
    /// Operation errors are logged without the URL, SQL text or decision body.
    async fn load_decision(&self, id: Uuid) -> Result<Option<ApprovalDecision>, ApprovalError> {
        for attempt_no in 1..=MAX_DB_ATTEMPTS {
            let mut guard = self.connection.lock().await;
            let result = guard.client.query_opt(LOAD_DECISION_SQL, &[&id]).await;

            match result {
                Ok(Some(row)) => {
                    let decision: String = row.get("decision");
                    let decided_by: Option<String> = row.get("decided_by");
                    let reason: Option<String> = row.get("reason");
                    let decided_at: DateTime<Utc> = row.get("decided_at");
                    return Ok(decision_from_columns(
                        &decision,
                        decided_by.as_deref(),
                        reason.as_deref(),
                        decided_at,
                    ));
                }
                Ok(None) => return Ok(None),
                Err(error) => {
                    // Fixed-vocabulary log only: a tokio-postgres error can
                    // echo statement text, and this seam must never emit URL,
                    // SQL or decision body into a log record.
                    tracing::warn!(attempt_no, "load_decision database attempt failed");
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

/// Rebuild a canonical [`ApprovalDecision`] from the durable columns selected
/// by [`LOAD_DECISION_SQL`] (I38-A). Only a committed row reaches here, but an
/// unexpected label is treated as "no authoritative decision" (`None`) rather
/// than fabricating a value — the caller keeps its fail-safe `Expired` path.
fn decision_from_columns(
    decision: &str,
    decided_by: Option<&str>,
    reason: Option<&str>,
    decided_at: DateTime<Utc>,
) -> Option<ApprovalDecision> {
    match decision {
        "approved" => Some(ApprovalDecision::Approved {
            approved_by: decided_by.unwrap_or_default().to_owned(),
            approved_at: decided_at,
        }),
        "denied" => Some(ApprovalDecision::Denied {
            reason: reason.unwrap_or_default().to_owned(),
        }),
        "expired" => Some(ApprovalDecision::Expired),
        _ => None,
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

    // ---- I34-D lost-commit-acknowledgement regression (pure, no real DB) ---

    /// A committed row must only be accepted as "our" commit when the decision
    /// label and its payload match what this call attempted. Every other
    /// combination is a competing decision and must keep the `AlreadyDecided`
    /// semantics (HTTP 409).
    #[test]
    fn recorded_decision_identity_is_decision_and_payload_only() {
        let approved = RecordedDecision {
            decision: "approved".into(),
            decided_by: Some("alice".into()),
            reason: None,
        };
        assert!(is_our_decision(&approved, "approved", Some("alice"), None));
        assert!(!is_our_decision(&approved, "denied", None, Some("policy")));
        assert!(!is_our_decision(&approved, "approved", Some("bob"), None));
        assert!(!is_our_decision(&approved, "approved", None, None));

        let denied = RecordedDecision {
            decision: "denied".into(),
            decided_by: None,
            reason: Some("policy".into()),
        };
        assert!(is_our_decision(&denied, "denied", None, Some("policy")));
        assert!(!is_our_decision(&denied, "denied", None, Some("other")));

        let expired = RecordedDecision {
            decision: "expired".into(),
            decided_by: None,
            reason: None,
        };
        assert!(is_our_decision(&expired, "expired", None, None));
        assert!(!is_our_decision(&expired, "approved", Some("alice"), None));
    }

    /// The exact regression: the first attempt committed `approved` but its
    /// acknowledgement was lost; the retry's zero-row CAS must be classified
    /// as `OurCommit` (→ `Ok`, no second audit row), not `AlreadyDecided`.
    /// A different committed decision and an undecided row keep their original
    /// `AlreadyDecided` / retry behavior.
    #[test]
    fn lost_commit_acknowledgement_is_classified_as_our_commit() {
        let committed = RecordedDecision {
            decision: "approved".into(),
            decided_by: Some("operator".into()),
            reason: None,
        };
        assert_eq!(
            classify_recorded(Some(&committed), "approved", Some("operator"), None),
            DurableResolution::OurCommit
        );
        assert_eq!(
            classify_recorded(Some(&committed), "denied", None, Some("policy")),
            DurableResolution::CompetingDecision
        );
        assert_eq!(
            classify_recorded(None, "approved", Some("operator"), None),
            DurableResolution::Undecided
        );
    }

    /// Simulates the durable round-trip without a database: every decision
    /// projected by `decision_columns` (the very values written by the CAS and
    /// the audit INSERT) must be recognised as our own commit once read back.
    /// This guards the `decision` / `decided_by` / `reason` column mapping the
    /// probe relies on, including the `Expired` path used by `mark_expired`.
    #[test]
    fn every_decision_column_projection_round_trips_as_our_commit() {
        let decisions = [
            ApprovalDecision::Approved {
                approved_by: "alice".into(),
                approved_at: Utc::now(),
            },
            ApprovalDecision::Denied {
                reason: "policy".into(),
            },
            ApprovalDecision::Expired,
            ApprovalDecision::Pending,
        ];
        for decision in &decisions {
            let (label, by, reason) = decision_columns(decision);
            let recorded = RecordedDecision {
                decision: label.into(),
                decided_by: by.map(str::to_owned),
                reason: reason.map(str::to_owned),
            };
            assert_eq!(
                classify_recorded(Some(&recorded), label, by, reason),
                DurableResolution::OurCommit,
                "a committed row must round-trip through the same projection: {decision:?}"
            );
        }
    }

    /// The probe SQL must key on `decided_at IS NOT NULL`: an undecided row is
    /// not a commit, even if a stale `decision` value were present. This is a
    /// source-level assertion that stays valid without a live database.
    #[test]
    fn probe_sql_uses_decided_at_as_the_commit_marker() {
        let sql = READ_DECISION_SQL.to_ascii_lowercase();
        assert!(sql.contains("decided_at is not null"));
        assert!(sql.contains("select decision, decided_by, reason"));
        assert!(sql.contains("where id = $1"));
    }

    // ---- I36-B / I38-A lost-ack read-back across attempts (pure, no DB) ---

    /// Drive the pure per-attempt read-back decision over a sequence of
    /// simulated probe outcomes (one per transaction attempt, each on its own
    /// fresh generation) and return the final resolution. Mirrors the single
    /// shared `MAX_DB_ATTEMPTS` budget of `commit_decision` + `read_back_once`
    /// without I/O, so the retry semantics are regression-testable without a
    /// database.
    fn resolve_probe_sequence(
        probes: &[Result<Option<RecordedDecision>, ()>],
        label: &str,
        by: Option<&str>,
        reason: Option<&str>,
    ) -> DurableResolution {
        for probe in probes.iter().take(MAX_DB_ATTEMPTS) {
            if let Some(resolution) = probe_iteration_resolution(probe, label, by, reason) {
                return resolution;
            }
        }
        DurableResolution::Undecided
    }

    #[test]
    fn probe_transport_error_then_fresh_generation_finds_our_commit() {
        let committed = Some(RecordedDecision {
            decision: "approved".into(),
            decided_by: Some("operator".into()),
            reason: None,
        });
        // Generation 1: probe/reconnect transport error -> keep trying.
        // Generation 2: a fresh generation reads the committed approved row.
        let probes = [Err(()), Ok(committed)];
        assert_eq!(
            resolve_probe_sequence(&probes, "approved", Some("operator"), None),
            DurableResolution::OurCommit
        );
    }

    #[test]
    fn probe_keeps_trying_generations_after_each_reconnect_failure() {
        let committed = Some(RecordedDecision {
            decision: "approved".into(),
            decided_by: Some("operator".into()),
            reason: None,
        });
        // Two failed generations/reconnects, then success on the third — still
        // within MAX_DB_ATTEMPTS, so a transient outage cannot lose our commit.
        let probes = [Err(()), Err(()), Ok(committed)];
        assert_eq!(
            resolve_probe_sequence(&probes, "approved", Some("operator"), None),
            DurableResolution::OurCommit
        );
    }

    #[test]
    fn probe_exhausting_budget_stays_undecided_for_persistence() {
        // Every generation's probe fails at the transport level: genuinely
        // undecidable after MAX_DB_ATTEMPTS -> the caller returns Persistence.
        let probes = [Err(()), Err(()), Err(())];
        assert_eq!(
            resolve_probe_sequence(&probes, "approved", Some("operator"), None),
            DurableResolution::Undecided
        );
    }

    #[test]
    fn final_generation_probe_is_not_skipped_and_keeps_competing_semantics() {
        // A definitive read on the last available generation must resolve
        // OurCommit even when no transaction retry budget remains.
        let probes = [
            Err(()),
            Err(()),
            Ok(Some(RecordedDecision {
                decision: "approved".into(),
                decided_by: Some("operator".into()),
                reason: None,
            })),
        ];
        assert_eq!(
            resolve_probe_sequence(&probes, "approved", Some("operator"), None),
            DurableResolution::OurCommit
        );
        // A competing decision on any generation still keeps AlreadyDecided.
        let competing = Ok(Some(RecordedDecision {
            decision: "denied".into(),
            decided_by: None,
            reason: Some("policy".into()),
        }));
        assert_eq!(
            resolve_probe_sequence(&[competing], "approved", Some("operator"), None),
            DurableResolution::CompetingDecision
        );
    }

    // ---- I38-A durable re-read seam (pure, no DB) --------------------------

    /// The re-read SQL must select only committed rows (`decided_at IS NOT
    /// NULL`) and must return the columns needed to rebuild a canonical
    /// decision, including `decided_at` for `Approved.approved_at`. Source-level
    /// assertion that stays valid without a live database.
    #[test]
    fn load_decision_sql_reads_only_committed_rows() {
        let sql = LOAD_DECISION_SQL.to_ascii_lowercase();
        assert!(sql.contains("where id = $1 and decided_at is not null"));
        assert!(sql.contains("select decision, decided_by, reason, decided_at"));
        assert!(sql.contains("from approval_requests"));
    }

    /// Every decision projection written by the CAS must round-trip through the
    /// re-read reconstruction: the value the timeout path returns is the same
    /// one the operator committed. An unknown label is rejected (`None`) rather
    /// than fabricated, so the caller keeps its fail-safe `Expired` fallback.
    #[test]
    fn decision_from_columns_round_trips_the_audit_vocabulary() {
        let decided_at = Utc::now();
        let decisions = [
            ApprovalDecision::Approved {
                approved_by: "alice".into(),
                approved_at: decided_at,
            },
            ApprovalDecision::Denied {
                reason: "policy".into(),
            },
            ApprovalDecision::Expired,
        ];
        for decision in &decisions {
            let (label, by, reason) = decision_columns(decision);
            let rebuilt = decision_from_columns(label, by, reason, decided_at);
            assert_eq!(
                rebuilt.as_ref(),
                Some(decision),
                "committed decision must round-trip through the re-read seam: {decision:?}"
            );
        }
        assert_eq!(
            decision_from_columns("unknown", None, None, decided_at),
            None,
            "an unexpected label must not fabricate a decision"
        );
    }
}
