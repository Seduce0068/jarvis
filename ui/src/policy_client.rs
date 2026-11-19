//! Loopback HTTP client for the `jarvis-policy` approval API.
//!
//! Endpoints (see `rust/jarvis-policy/src/http.rs`):
//! - `GET  /v1/approvals`             -> `{ "pending": [ApprovalRequest, ..] }`
//! - `POST /v1/approval/{id}/approve` -> `{ "decision": "Approved", .. }`
//! - `POST /v1/approval/{id}/deny`    -> `{ "decision": "Denied", .. }`
//!
//! The POST endpoints return a canonical tagged decision JSON. That body is
//! parsed through the core boundary on the real RUN path (never only in tests)
//! before it reaches the UI; see [`decision_from_json`] and [`PolicyClient::decide`].
//!
//! This client is intentionally **blocking**: GPUI drives its own executor,
//! so a Tokio-based client such as reqwest has no reactor here. Callers must
//! run these methods on GPUI's background executor (see `approvals.rs`).
//! The console only ever talks to the configured loopback base URL.

use anyhow::{Context as _, Result, bail};
use jarvis_core::approval::{ApprovalDecision as Decision, ApprovalRequest as CoreApprovalRequest};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

/// Canonical env var for the gateway-facing approval API token (list/request/status).
pub const ENV_APPROVAL_API_TOKEN: &str = "JARVIS_POLICY_APPROVAL_API_TOKEN";
/// Deprecated alias, honoured temporarily with a startup warning.
pub const ENV_LEGACY_APPROVAL_TOKEN: &str = "JARVIS_POLICY_APPROVAL_TOKEN";
/// Env var for the token guarding approve/deny on the policy side.
pub const ENV_APPROVAL_ADMIN_TOKEN: &str = "JARVIS_POLICY_APPROVAL_ADMIN_TOKEN";

/// A secret value whose `Debug` output never contains the secret itself.
#[derive(Clone)]
struct RedactedToken(String);

impl RedactedToken {
    fn new(value: String) -> Self {
        Self(value)
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RedactedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RedactedToken(<redacted>)")
    }
}

/// Version marker for the two-token bundle transported through the single
/// `Option<String>` channel exposed by `ApprovalsView::new` (which lives in a
/// file outside this change's scope). Env values cannot contain NUL, and the
/// payload is JSON behind this marker, so plain legacy tokens stay unambiguous.
const TOKEN_WIRE_PREFIX: &str = "jarvis-tokens-v1:";

/// Fixed 409 body emitted by `jarvis-policy` when the emergency stop blocked a
/// decision. Aliased from `jarvis-core` (I54-C) so this client cannot drift from
/// `approval_error_reasons::KILL_SWITCH`. Since I58-B the halt path is the
/// default for every 409 body that is not the already-decided literal; this
/// constant stays the explicit first branch of the classifier and its wire
/// contract pin in tests.
const KILL_SWITCH_409_BODY: &str = jarvis_core::KILL_SWITCH_409_BODY;

/// Fixed 409 body emitted by `jarvis-policy` when a competing operator already
/// decided the request. Aliased from `jarvis-core` (I58-B) so the UI and policy
/// classify the same literal: only an exact (trimmed) match to this body may
/// proceed to `winner_decision`. An unknown body from an older peer, a rolling
/// build mismatch or a rewritten relay response must never be mistaken for a
/// benign "already decided" result.
const ALREADY_DECIDED_409_BODY: &str = jarvis_core::ALREADY_DECIDED_409_BODY;

/// Classify the bounded read of a 409 response body (I56-B, I58-B).
///
/// `true` pins the halt ([`DecisionOutcome::KillSwitchBlocked`]); only `false`
/// allows the winner readback. The emergency stop and the already-decided
/// winner share HTTP 409, so classification is by the fixed body alone:
///
/// - the known kill-switch body and every unrecognized body (including a body
///   rewritten by an older peer, a rolling build or a relay) pin the halt;
/// - a read failure (truncated or size-limited body) or an empty/whitespace
///   body is unclassifiable and pins the halt as well;
/// - only an exact match to the shared already-decided literal is benign.
///
/// This is fail-closed: a genuine emergency stop can no longer be reported as
/// "already decided" because its body changed. The body is never returned,
/// displayed or logged.
fn kill_switch_body_blocks<E>(body: &std::result::Result<String, E>) -> bool {
    match body {
        Ok(body) => {
            let body = body.trim();
            if body == KILL_SWITCH_409_BODY {
                return true;
            }
            // I58-B: the winner path is the narrow exception, not the default.
            body != ALREADY_DECIDED_409_BODY
        }
        Err(_) => true,
    }
}

/// Tokens resolved for the two approval scopes.
///
/// Mirrors `jarvis-policy`: API routes (request/list/status) use `api_token`,
/// while approve/deny use `admin_token` and fall back to `api_token` when no
/// admin token is configured.
#[derive(Clone)]
pub struct PolicyTokens {
    api_token: Option<String>,
    admin_token: Option<String>,
    /// True when only the deprecated env alias supplied the API token.
    legacy_api_env_used: bool,
}

impl PolicyTokens {
    fn new(api_token: Option<String>, admin_token: Option<String>) -> Self {
        Self {
            api_token,
            admin_token,
            legacy_api_env_used: false,
        }
    }

    /// Effective token for approve/deny: admin token, else API token fallback.
    /// Test-only: the runtime scope resolution lives on `PolicyClient`.
    #[cfg(test)]
    fn effective_admin_token(&self) -> Option<&str> {
        self.admin_token.as_deref().or(self.api_token.as_deref())
    }

    /// Whether startup should warn about the deprecated env alias.
    pub fn legacy_api_env_used(&self) -> bool {
        self.legacy_api_env_used
    }

    /// Serialize both tokens into the single `Option<String>` channel.
    /// Returns `None` when no token is configured at all.
    pub fn encode(&self) -> Option<String> {
        if self.api_token.is_none() && self.admin_token.is_none() {
            return None;
        }
        #[derive(Serialize)]
        struct Wire<'a> {
            api: Option<&'a str>,
            admin: Option<&'a str>,
        }
        let wire = Wire {
            api: self.api_token.as_deref(),
            admin: self.admin_token.as_deref(),
        };
        let json = serde_json::to_string(&wire).expect("token wire is infallible");
        Some(format!("{TOKEN_WIRE_PREFIX}{json}"))
    }

    /// Decode a channel value. Anything without the version marker is a plain
    /// legacy token used for both scopes (the old single-token contract).
    fn decode_or_legacy(raw: Option<String>) -> Self {
        let Some(raw) = raw else {
            return Self::new(None, None);
        };
        if let Some(json) = raw.strip_prefix(TOKEN_WIRE_PREFIX) {
            #[derive(Deserialize)]
            struct Wire {
                api: Option<String>,
                admin: Option<String>,
            }
            if let Ok(wire) = serde_json::from_str::<Wire>(json) {
                return Self::new(wire.api, wire.admin);
            }
        }
        Self::new(Some(raw), None)
    }
}

impl fmt::Debug for PolicyTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PolicyTokens")
            .field("api_token", &self.api_token.as_ref().map(|_| "<redacted>"))
            .field(
                "admin_token",
                &self.admin_token.as_ref().map(|_| "<redacted>"),
            )
            .field("legacy_api_env_used", &self.legacy_api_env_used)
            .finish()
    }
}

/// Resolve tokens from `std::env`. Called once at startup by `main`, which
/// emits the deprecation warning when [`PolicyTokens::legacy_api_env_used`].
pub fn tokens_from_env() -> PolicyTokens {
    tokens_from_env_with(|key| std::env::var(key).ok())
}

/// Blank values count as unset, matching the policy service's blank-token
/// startup guard. Pure so the env-name -> scope contract is testable without
/// mutating the process environment.
fn tokens_from_env_with(getter: impl Fn(&str) -> Option<String>) -> PolicyTokens {
    fn non_blank(value: String) -> Option<String> {
        (!value.trim().is_empty()).then_some(value)
    }
    let canonical = getter(ENV_APPROVAL_API_TOKEN).and_then(non_blank);
    let legacy = getter(ENV_LEGACY_APPROVAL_TOKEN).and_then(non_blank);
    let legacy_api_env_used = canonical.is_none() && legacy.is_some();
    PolicyTokens {
        api_token: canonical.or(legacy),
        admin_token: getter(ENV_APPROVAL_ADMIN_TOKEN).and_then(non_blank),
        legacy_api_env_used,
    }
}

/// A pending approval request as returned by `jarvis-policy`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ApprovalRequest {
    /// Legacy peers may omit the id entirely; `""` keeps such an entry
    /// renderable instead of failing the whole poll. Canonical conversion
    /// rejects it explicitly at the boundary (see `request_to_core`).
    #[serde(default)]
    pub id: String,
    pub principal: String,
    pub action: String,
    pub resource: String,
    #[serde(default)]
    pub context: serde_json::Value,
    #[serde(default)]
    pub requested_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Thin wire DTO for decision JSON returned by `jarvis-policy`.
///
/// Same boundary pattern as `jarvis-mcp-gw`: the HTTP contract carries
/// `approved_at` as an RFC 3339 *string*, while the canonical core type stores
/// a `DateTime<Utc>`. The serde tag and field names below are the unchanged
/// wire contract; only the receiving type is thin.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "decision")]
enum DecisionWire {
    Approved {
        approved_by: String,
        approved_at: String,
    },
    Denied {
        reason: String,
    },
    Pending,
    Expired,
}

/// Boundary: incoming decision JSON -> canonical core [`Decision`].
///
/// The tagged shape is the current `jarvis-policy` contract: `approve`/`deny`
/// respond with the serialized canonical `ApprovalDecision` (see
/// `rust/jarvis-policy/src/http.rs`). The legacy two-variant shape
/// (`"Approved"` / `{"Denied":{"reason":".."}}`) was removed together with the
/// core legacy type (P1-7 slice 4) and is **explicitly rejected** here: an
/// unrecognized shape or a malformed RFC 3339 `approved_at` fails closed. No
/// author/timestamp is fabricated for a partially populated decision.
///
/// This boundary is on the real RUN path: [`PolicyClient::decide`] parses the
/// POST approve/deny response body through it, and `approvals.rs` projects the
/// returned core decision onto the UI-facing decision state.
pub fn decision_from_json(value: &serde_json::Value) -> Result<Decision> {
    if let Ok(wire) = serde_json::from_value::<DecisionWire>(value.clone()) {
        return match wire {
            DecisionWire::Approved {
                approved_by,
                approved_at,
            } => {
                let approved_at = chrono::DateTime::parse_from_rfc3339(&approved_at)
                    .with_context(|| format!("invalid approved_at: {approved_at}"))?
                    .with_timezone(&chrono::Utc);
                Ok(Decision::Approved {
                    approved_by,
                    approved_at,
                })
            }
            DecisionWire::Denied { reason } => Ok(Decision::Denied { reason }),
            DecisionWire::Pending => Ok(Decision::Pending),
            DecisionWire::Expired => Ok(Decision::Expired),
        };
    }
    // The legacy two-variant core type was removed with P1-7 slice 4; an
    // unrecognized tagged decision fails explicitly (fail-closed) rather than
    // being misread as a legacy shape or defaulted into an approval.
    Err(anyhow::anyhow!("unrecognized approval decision JSON"))
}

/// Outcome of an approve/deny POST.
///
/// A `2xx` body crosses the canonical decision boundary and yields [`Self::Decided`].
/// An HTTP 410 (I60-B) is likewise reported as [`Self::Decided`] carrying the
/// canonical `Expired` decision: the store stamped nothing, but expiry is a
/// real terminal decision with no winner to read back.
/// An HTTP 409 is classified by its fixed body (I52-A, I58-B): only the
/// already-decided literal means the row was settled by a competing winner, so
/// the authoritative decision is read back and reported as [`Self::AlreadyDecided`]
/// (I48-C / I50-C (b)). The kill-switch literal and *every other* body (empty,
/// unreadable, unknown) mean no decision was made at all; they are reported as
/// [`Self::KillSwitchBlocked`] so the UI pins its halt display instead of
/// claiming a false success.
#[derive(Debug, Clone, PartialEq)]
pub enum DecisionOutcome {
    Decided(Decision),
    AlreadyDecided(Decision),
    /// The decision POST was rejected by the emergency stop. No winner is
    /// consulted and no decision is fabricated: there is nothing to read back.
    KillSwitchBlocked,
}

impl DecisionOutcome {
    /// The canonical decision to display, if the service actually made one.
    /// A kill-switch rejection carries no decision by design.
    pub fn decision(&self) -> Option<&Decision> {
        match self {
            Self::Decided(decision) | Self::AlreadyDecided(decision) => Some(decision),
            Self::KillSwitchBlocked => None,
        }
    }

    /// Consume the outcome and return the canonical decision. Keeps the legacy
    /// `approve`/`deny` signatures unchanged for existing tests; production
    /// call sites use `decide_outcome` directly. A kill-switch rejection has no
    /// decision to project and must use `approve_outcome`/`deny_outcome`.
    #[cfg(test)]
    pub fn into_decision(self) -> Decision {
        match self {
            Self::Decided(decision) | Self::AlreadyDecided(decision) => decision,
            Self::KillSwitchBlocked => panic!("kill-switch 409 has no decision"),
        }
    }
}

/// Boundary: UI wire request -> canonical core [`CoreApprovalRequest`].
///
/// The UI wire DTO intentionally keeps a `String` id and nullable timestamps so
/// legacy payloads still render. Conversion constructs the canonical core type
/// directly: missing timestamps become the epoch, while a missing/malformed
/// UUID id fails explicitly instead of silently producing a canonical request
/// with a fabricated identity.
///
/// Called at decision time by `ApprovalsView::decide`: the target id is
/// resolved through this boundary before it is used in the POST URL, so a
/// tolerant-wire id can never be mistaken for a canonical one.
pub fn request_to_core(wire: &ApprovalRequest) -> Result<CoreApprovalRequest> {
    let id = uuid::Uuid::parse_str(&wire.id).context("invalid approval id at UI boundary")?;
    let epoch = chrono::DateTime::from_timestamp(0, 0).expect("epoch is valid");
    Ok(CoreApprovalRequest {
        id,
        principal: wire.principal.clone(),
        action: wire.action.clone(),
        resource: wire.resource.clone(),
        context: wire.context.clone(),
        requested_at: wire.requested_at.unwrap_or(epoch),
        expires_at: wire.expires_at.unwrap_or(epoch),
    })
}

#[derive(Debug, Deserialize)]
struct PendingResponse {
    #[serde(default)]
    pending: Vec<ApprovalRequest>,
}

#[derive(Serialize)]
struct ApproveBody<'a> {
    approved_by: &'a str,
}

#[derive(Serialize)]
struct DenyBody<'a> {
    reason: &'a str,
}

/// Overall deadline for a single policy HTTP exchange.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Connect deadline (loopback connects are immediate).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Blocking client for the policy approval endpoints.
#[derive(Clone)]
pub struct PolicyClient {
    base_url: String,
    /// Guards the API scope (`GET /v1/approvals`).
    api_token: Option<RedactedToken>,
    /// Guards the admin scope (POST approve/deny); API token is the fallback.
    admin_token: Option<RedactedToken>,
    agent: ureq::Agent,
    /// Decision POSTs use this agent so a 409 arrives as a response whose fixed
    /// body can be classified (kill switch vs already decided, I52-A). The main
    /// agent keeps status-as-error for the list/status GETs. Same deadlines and
    /// no redirects, because this agent carries the admin token.
    decide_agent: ureq::Agent,
    /// Display-only health probe agent. Shares the main agent's deadlines
    /// (`timeout_global` / `timeout_connect`, I50-C (c)) so an unresponsive
    /// policy service cannot hang the poller, disables status-as-error so the
    /// fixed `503` degraded body stays readable (I48-D), and disables redirects
    /// so a proxied `302 -> 200` cannot be mistaken for a healthy answer
    /// (I60-A). No token is ever attached to this agent.
    probe_agent: ureq::Agent,
}

impl fmt::Debug for PolicyClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PolicyClient")
            .field("base_url", &self.base_url)
            .field("api_token", &self.api_token)
            .field("admin_token", &self.admin_token)
            .finish_non_exhaustive()
    }
}

impl PolicyClient {
    /// Convenience constructor (used by tests and embedders).
    #[allow(dead_code)]
    pub fn new(base_url: impl Into<String>) -> Self {
        Self::new_with_token(base_url, None)
    }

    /// Single-token constructor kept for compatibility. `token` may be a full
    /// [`PolicyTokens::encode`] bundle or a plain legacy token.
    pub fn new_with_token(base_url: impl Into<String>, token: Option<String>) -> Self {
        Self::with_tokens(
            base_url,
            PolicyTokens::decode_or_legacy(token),
            REQUEST_TIMEOUT,
            CONNECT_TIMEOUT,
        )
    }

    /// Explicit two-token constructor (API scope vs admin scope).
    /// Used by the env-name -> header contract tests and by embedders.
    #[allow(dead_code)]
    pub fn new_with_tokens(
        base_url: impl Into<String>,
        api_token: Option<String>,
        admin_token: Option<String>,
    ) -> Self {
        Self::with_tokens(
            base_url,
            PolicyTokens::new(api_token, admin_token),
            REQUEST_TIMEOUT,
            CONNECT_TIMEOUT,
        )
    }

    /// Constructor with explicit deadlines (used by timeout tests).
    #[allow(dead_code)]
    fn with_timeouts(
        base_url: impl Into<String>,
        token: Option<String>,
        request_timeout: Duration,
        connect_timeout: Duration,
    ) -> Self {
        Self::with_tokens(
            base_url,
            PolicyTokens::decode_or_legacy(token),
            request_timeout,
            connect_timeout,
        )
    }

    fn with_tokens(
        base_url: impl Into<String>,
        tokens: PolicyTokens,
        request_timeout: Duration,
        connect_timeout: Duration,
    ) -> Self {
        // Both polling and decision posts are short; a hung policy service
        // must not occupy a GPUI background thread indefinitely.
        // REQUEST_TIMEOUT / CONNECT_TIMEOUT (via the constructor arguments) are
        // applied to *all three* agents below so neither the decision POST nor
        // the health probe can silently fall back to ureq's unbounded defaults
        // (I50-C (c)).
        let config = ureq::config::Config::builder()
            .timeout_global(Some(request_timeout))
            .timeout_connect(Some(connect_timeout))
            // Never follow redirects: ureq strips only Authorization/Cookie
            // headers on redirect, so X-Jarvis-Approval-Token would be replayed
            // to the redirect target (possibly a different host). With 0 the
            // 3xx response is returned as-is and the token stays on loopback.
            .max_redirects(0)
            .build();
        let decide_config = ureq::config::Config::builder()
            .timeout_global(Some(request_timeout))
            .timeout_connect(Some(connect_timeout))
            // A 409 is a response, not an error: the kill-switch / already-
            // decided split lives in its fixed body (I52-A).
            .http_status_as_error(false)
            // Same token-replay guard as the main agent: never follow a 3xx.
            .max_redirects(0)
            .build();
        let probe_config = ureq::config::Config::builder()
            .timeout_global(Some(request_timeout))
            .timeout_connect(Some(connect_timeout))
            // The probe reads the fixed 503 body, which the main agent's
            // status-as-error default would otherwise discard.
            .http_status_as_error(false)
            // Never follow a redirect: a reverse proxy answering 302 -> 200
            // must not be read as an explicit "switch lifted" answer. With 0
            // the 3xx arrives as-is and `kill_switch_probe` fails closed
            // (I60-A), matching the main and decide agents' guard.
            .max_redirects(0)
            .build();
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_token: tokens.api_token.map(RedactedToken::new),
            admin_token: tokens.admin_token.map(RedactedToken::new),
            agent: ureq::Agent::new_with_config(config),
            decide_agent: ureq::Agent::new_with_config(decide_config),
            probe_agent: ureq::Agent::new_with_config(probe_config),
        }
    }

    /// Effective token for approve/deny: admin token, else API token fallback.
    fn effective_admin_token(&self) -> Option<&RedactedToken> {
        self.admin_token.as_ref().or(self.api_token.as_ref())
    }

    /// Requests currently awaiting an operator decision.
    ///
    /// This endpoint returns only request DTOs — no decision field — so the
    /// decision JSON boundary lives on the POST approve/deny path instead;
    /// see [`PolicyClient::decide`].
    pub fn pending(&self) -> Result<Vec<ApprovalRequest>> {
        let url = format!("{}/v1/approvals", self.base_url);
        let mut request = self.agent.get(&url);
        // API scope: the list endpoint is guarded by the API token, never by
        // the admin token (jarvis-policy ApprovalScope::Api).
        if let Some(token) = &self.api_token {
            request = request.header("X-Jarvis-Approval-Token", token.expose());
        }
        let mut response = request
            .call()
            .with_context(|| format!("GET {url} failed"))?;
        // A 3xx is returned as-is because redirects are disabled; without this
        // check its (possibly empty) body could be mistaken for an empty queue.
        let status = response.status();
        if !status.is_success() {
            bail!("GET {url} returned HTTP {}", status.as_u16());
        }
        let body: PendingResponse = response
            .body_mut()
            .read_json()
            .context("invalid JSON from /v1/approvals")?;
        Ok(body.pending)
    }

    /// Read-only, unauthenticated probe of the policy service's emergency-stop
    /// state (I48-D, I60-A). `GET /health` answers `503` with the fixed body
    /// `{"status":"degraded","reason":"kill_switch"}` while the kill switch is
    /// active, `503` with another reason such as `no_policy` when degraded for
    /// another reason, and `200 {"status":"ok"}` otherwise. This is a *display*
    /// signal only: the decision POST still returns `200 Approved` when a stop
    /// lands mid-commit, while `jarvis-mcp-gw` re-checks the kill switch before
    /// the tool runs.
    ///
    /// Only two definitive answers exist: a `503 reason=kill_switch` reports
    /// `true`; a `200 status=ok` and any other `503` reason are an explicit
    /// release and report `false`. Everything else is an *incomplete* probe and
    /// **fails closed**: a timeout, a connection error, an unreadable or
    /// unexpected body, a redirect, or any status other than `200`/`503`
    /// (including a proxy `500`/`502`/`504`) returns `Err`, which this method
    /// projects to `true` through `unwrap_or(true)` (I54-A, I60-A). Otherwise a
    /// transient failure right after a kill-switch 409 would masquerade as
    /// "switch lifted" and drop the halt pin. No reason text, token, or request
    /// data is surfaced.
    pub fn kill_switch_active(&self) -> bool {
        self.kill_switch_probe().unwrap_or(true)
    }

    /// Raw probe result. Kept separate from [`Self::kill_switch_active`] so a
    /// timeout regression can observe the deadline error directly instead of
    /// only seeing the `bool` projection (I52-A (c)). Errors never carry a
    /// reason body, token or request data.
    fn kill_switch_probe(&self) -> Result<bool> {
        let url = format!("{}/health", self.base_url);
        // The probe-only agent was built in `with_tokens` with the same
        // `timeout_global` / `timeout_connect` as the main agent (I50-C (c)),
        // with status-as-error disabled so the fixed degraded body can be read,
        // and with redirects disabled so a 3xx is never followed (I60-A).
        // It carries no token and never touches the main approval agent.
        let mut response = self
            .probe_agent
            .get(&url)
            .call()
            .with_context(|| format!("GET {url} failed"))?;
        let status = response.status().as_u16();
        // Only the two documented /health states are definitive. A 3xx cannot
        // reach here as a followed 200 because the probe agent has
        // `max_redirects(0)`. A 4xx/5xx from a reverse proxy is an incomplete
        // probe, not a "switch lifted" answer (I60-A).
        if status != 200 && status != 503 {
            bail!("GET {url} returned unexpected HTTP {status}");
        }
        let value = response
            .body_mut()
            .read_json::<serde_json::Value>()
            .with_context(|| format!("invalid health JSON from GET {url}"))?;
        if status == 200 {
            // The healthy body is exactly `{"status":"ok"}` with no reason.
            // A 200 carrying any other status is not an explicit release and
            // must not clear a halt pin (I60-A).
            if value.get("status").and_then(|s| s.as_str()) == Some("ok") {
                return Ok(false);
            }
            bail!("unexpected health body from GET {url}");
        }
        // 503 is degraded: only the fixed kill-switch reason means "stopped";
        // every other reason (e.g. `no_policy`) is an explicit release.
        Ok(value.get("reason").and_then(|reason| reason.as_str()) == Some("kill_switch"))
    }

    /// Approve the request `id` as `approved_by`.
    ///
    /// Returns the canonical decision parsed from the response body. An
    /// already-decided 409 is resolved to the authoritative winner via
    /// [`Self::winner_decision`], so the legacy `Result<Decision>` signature
    /// keeps its meaning. Test-only: production call sites use
    /// `approve_outcome`, which also distinguishes a kill-switch rejection (a
    /// 409 with no decision to project).
    #[cfg(test)]
    pub fn approve(&self, id: &str, approved_by: &str) -> Result<Decision> {
        Ok(self.approve_outcome(id, approved_by)?.into_decision())
    }

    /// Deny the request `id` with `reason`.
    ///
    /// Mirrors [`Self::approve`] for the deny endpoint. Test-only.
    #[cfg(test)]
    pub fn deny(&self, id: &str, reason: &str) -> Result<Decision> {
        Ok(self.deny_outcome(id, reason)?.into_decision())
    }

    /// Approve with the full outcome, distinguishing a fresh 2xx commit from a
    /// winner-preservation 409 (I50-C (b)). Used by the approval console.
    pub fn approve_outcome(&self, id: &str, approved_by: &str) -> Result<DecisionOutcome> {
        self.decide_outcome(id, "approve", &ApproveBody { approved_by })
    }

    /// Deny with the full outcome, mirroring [`Self::approve_outcome`].
    pub fn deny_outcome(&self, id: &str, reason: &str) -> Result<DecisionOutcome> {
        self.decide_outcome(id, "deny", &DenyBody { reason })
    }

    fn decide_outcome<B: Serialize + ?Sized>(
        &self,
        id: &str,
        verb: &str,
        body: &B,
    ) -> Result<DecisionOutcome> {
        let url = format!("{}/v1/approval/{id}/{verb}", self.base_url);
        // `decide_agent` has status-as-error disabled: a 409 arrives as a
        // response whose fixed body can be classified (I52-A (a)). It keeps the
        // same deadlines and redirect guard as the main agent.
        let mut request = self.decide_agent.post(&url);
        // Admin scope: approve/deny require the separate admin token when it
        // is configured, otherwise the API token (same fallback as policy).
        // The header name is unchanged; only the value is scope-specific.
        if let Some(token) = self.effective_admin_token() {
            request = request.header("X-Jarvis-Approval-Token", token.expose());
        }
        let mut response = request
            .send_json(body)
            .with_context(|| format!("POST {url} failed"))?;
        let status = response.status();
        if status.as_u16() == 409 {
            // Bounded read: only the fixed vocabulary matters. The body is
            // never returned, displayed or logged. Keep the read as a Result:
            // `.unwrap_or_default()` turned a read error into "" and let an
            // unclassifiable kill-switch 409 fall through to the winner
            // readback — fail-open on the exact path that must fail closed
            // (I56-B).
            let error_body = response
                .body_mut()
                .with_config()
                .limit(256)
                .read_to_string();
            // I58-B: only the exact already-decided body from `jarvis-core`
            // (which `jarvis-policy` also emits) may reach `winner_decision`.
            // The kill-switch body, an empty body, a read error, and any body
            // rewritten by an older peer or a relay all pin the halt. Keeping
            // the winner path narrow prevents a genuine stop from being shown
            // as "already decided".
            if kill_switch_body_blocks(&error_body) {
                return Ok(DecisionOutcome::KillSwitchBlocked);
            }
            return Ok(DecisionOutcome::AlreadyDecided(self.winner_decision(id)?));
        }
        // I60-B: the policy service maps a post-deadline approve/deny to 410
        // Gone with its own fixed body. No decision was committed and no winner
        // exists, so surface the canonical `Expired` decision directly. This
        // keeps expiry out of the 409 winner branch that used to render
        // "既に決定済み: 保留中" and never touches the shared 409 bodies.
        if status.as_u16() == 410 {
            return Ok(DecisionOutcome::Decided(Decision::Expired));
        }
        // A 401/500/3xx must not be reported to the operator as a successful
        // approve/deny. The fixed error body is never surfaced.
        if !status.is_success() {
            bail!("POST {url} returned HTTP {}", status.as_u16());
        }
        // A 2xx body is the canonical tagged decision. Route it through the
        // same boundary as any other decision JSON before it reaches the UI:
        // this 2xx -> canonical decision boundary is unchanged (I50-C (b)).
        let value: serde_json::Value = response
            .body_mut()
            .read_json()
            .with_context(|| format!("invalid decision JSON from POST {url}"))?;
        Ok(DecisionOutcome::Decided(
            decision_from_json(&value)
                .with_context(|| format!("unexpected decision JSON from POST {url}"))?,
        ))
    }

    /// Token for the read-only status GET. Prefer the API token so a
    /// two-token deployment keeps its existing readback contract; fall back to
    /// the admin token when no API token exists. `jarvis-policy` now accepts
    /// either scope on this route (I52-A (b)), so an admin-only console can
    /// read the winner after a 409 instead of being rejected with 401/503.
    fn effective_status_token(&self) -> Option<&RedactedToken> {
        self.api_token.as_ref().or(self.admin_token.as_ref())
    }

    /// Read the authoritative decision from the status endpoint
    /// (`GET /v1/approval/{id}`) after a 409, parsed through the same canonical
    /// boundary as a 2xx body. No reason text, token or error body is surfaced.
    fn winner_decision(&self, id: &str) -> Result<Decision> {
        let url = format!("{}/v1/approval/{id}", self.base_url);
        let mut request = self.agent.get(&url);
        // Status accepts both scopes on the policy side (I52-A (b)); send the
        // API token when present and the admin token otherwise.
        if let Some(token) = self.effective_status_token() {
            request = request.header("X-Jarvis-Approval-Token", token.expose());
        }
        let mut response = request
            .call()
            .with_context(|| format!("GET {url} failed"))?;
        // A 3xx is returned as-is (redirects disabled) and must not be parsed
        // as a decision; other non-2xx statuses are explicit errors too.
        let status = response.status();
        if !status.is_success() {
            bail!("GET {url} returned HTTP {}", status.as_u16());
        }
        let value: serde_json::Value = response
            .body_mut()
            .read_json()
            .with_context(|| format!("invalid decision JSON from GET {url}"))?;
        decision_from_json(&value)
            .with_context(|| format!("unexpected decision JSON from GET {url}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::thread::JoinHandle;

    /// Bind a loopback server, accept `requests` connections and let `handle`
    /// decide what to answer. Used by every mock policy endpoint below so the
    /// socket setup is written only once.
    fn spawn_test_server(
        mut handle: impl FnMut(&mut TcpStream, String) + Send + 'static,
        requests: usize,
    ) -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("test address");
        let server = std::thread::spawn(move || {
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).expect("read");
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                handle(&mut stream, request);
            }
        });
        (addr, server)
    }

    fn respond(stream: &mut TcpStream, response: &str) {
        stream.write_all(response.as_bytes()).expect("write");
    }

    /// Direct observation of the configured deadline: an `anyhow` context
    /// wraps the ureq error, so inspect the whole source chain. A server that
    /// replies slowly produces `Ok` or `Io`, never `Timeout`.
    fn is_timeout(error: &anyhow::Error) -> bool {
        error.chain().any(|cause| {
            matches!(
                cause.downcast_ref::<ureq::Error>(),
                Some(ureq::Error::Timeout(_))
            )
        })
    }

    fn approval_token(request: &str) -> Option<String> {
        request
            .lines()
            .find(|line| {
                line.to_ascii_lowercase()
                    .starts_with("x-jarvis-approval-token:")
            })
            .and_then(|line| line.split_once(':'))
            .map(|(_, value)| value.trim().to_string())
    }

    #[test]
    fn parses_pending_list_and_tolerates_extra_fields() {
        let body = serde_json::json!({
            "pending": [{
                "id": "4d1a1f2e-0000-0000-0000-000000000000",
                "principal": "opencode",
                "action": "execute",
                "resource": "tool:shell.execute",
                "context": {"cmd": "ls"},
                "requested_at": "2026-11-15T08:00:00Z",
                "expires_at": "2026-11-15T08:05:00Z",
                "unknown_future_field": true
            }]
        });
        let parsed: PendingResponse = serde_json::from_value(body).expect("parses");
        assert_eq!(parsed.pending.len(), 1);
        assert_eq!(parsed.pending[0].principal, "opencode");
        assert_eq!(parsed.pending[0].resource, "tool:shell.execute");
    }

    #[test]
    fn empty_pending_list_is_accepted() {
        let parsed: PendingResponse =
            serde_json::from_value(serde_json::json!({ "pending": [] })).expect("parses");
        assert!(parsed.pending.is_empty());
    }

    #[test]
    fn base_url_trailing_slash_is_normalized() {
        let client = PolicyClient::new("http://127.0.0.1:8081/");
        assert_eq!(client.base_url, "http://127.0.0.1:8081");
    }

    #[test]
    fn hung_policy_endpoint_times_out() {
        // I52-A (c): answer only after 5s, far beyond the 150 ms deadline. A
        // vacuous test used to answer after 600 ms and only asserted < 3s,
        // which passed even with no timeout at all.
        let (addr, handle) = spawn_test_server(
            |stream, _request| {
                std::thread::sleep(Duration::from_secs(5));
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            },
            1,
        );

        let client = PolicyClient::with_timeouts(
            format!("http://{addr}"),
            None,
            Duration::from_millis(150),
            Duration::from_secs(1),
        );
        let started = std::time::Instant::now();
        let error = client.pending().expect_err("hung endpoint must error");
        // Directly observe the deadline firing, not merely "some error".
        assert!(is_timeout(&error), "must fail via the timeout: {error:#}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "deadline must fire long before the 5s server response"
        );
        let _ = handle.join();
    }

    /// The UI must send the configured token to jarvis-policy as well.
    #[test]
    fn approval_token_is_sent_as_header() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let seen_for_thread = seen.clone();
        let (addr, handle) = spawn_test_server(
            move |stream, request| {
                *seen_for_thread.lock().expect("lock") = approval_token(&request);
                respond(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 14\r\n\r\n{\"pending\":[]}",
                );
            },
            1,
        );

        let client =
            PolicyClient::new_with_token(format!("http://{addr}"), Some("ui-token".to_string()));
        // The request fails to parse only if the body is malformed; an empty
        // pending list is valid, so this must succeed.
        let result = client.pending();
        handle.join().expect("server thread");
        assert!(result.is_ok(), "unexpected: {result:?}");
        assert_eq!(seen.lock().expect("lock").as_deref(), Some("ui-token"));
    }

    /// A 3xx from jarvis-policy must not be followed: ureq forwards custom
    /// headers on redirect, so following it would replay the approval token
    /// to the redirect target (potentially a different host).
    #[test]
    fn approval_token_is_not_replayed_on_redirect() {
        use std::sync::mpsc;
        use std::time::Instant;

        // Redirect target. If ureq follows the 3xx, this server sees the
        // replayed X-Jarvis-Approval-Token header.
        let target = TcpListener::bind("127.0.0.1:0").expect("bind target");
        let target_addr = target.local_addr().expect("target addr");
        let (seen_tx, seen_rx) = mpsc::channel();
        let target_thread = std::thread::spawn(move || {
            target
                .set_nonblocking(true)
                .expect("set nonblocking on target");
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline {
                match target.accept() {
                    Ok((mut stream, _)) => {
                        let mut buf = [0u8; 4096];
                        let n = stream.read(&mut buf).unwrap_or(0);
                        let request = String::from_utf8_lossy(&buf[..n]).to_string();
                        let leaked = approval_token(&request).is_some();
                        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
                        let _ = seen_tx.send(Some(leaked));
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => return,
                }
            }
            let _ = seen_tx.send(None);
        });

        // Policy-side endpoint answering with a redirect to the other
        // listener (a different port, hence a different target).
        let (origin_addr, origin_thread) = spawn_test_server(
            move |stream, _request| {
                let response = format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://{target_addr}/leak\r\nContent-Length: 0\r\n\r\n"
                );
                let _ = stream.write_all(response.as_bytes());
            },
            1,
        );

        let client = PolicyClient::new_with_token(
            format!("http://{origin_addr}"),
            Some("redirect-token".to_string()),
        );
        // The raw 3xx must surface as an explicit error (not an empty queue),
        // and no follow-up request may reach the redirect target.
        let result = client.pending();
        assert!(result.is_err(), "3xx must be an error: {result:?}");
        let message = format!("{:#}", result.unwrap_err());
        assert!(
            message.contains("302"),
            "error must surface the HTTP status: {message}"
        );

        origin_thread.join().expect("origin thread");
        let target_seen = seen_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("target probe result");
        target_thread.join().expect("target thread");
        assert_eq!(
            target_seen, None,
            "redirect must not be followed, otherwise the token is replayed"
        );
    }

    /// A 500 with a *valid* body must still be an error: otherwise a broken
    /// policy service would show up as an empty approval queue.
    #[test]
    fn non_2xx_pending_status_is_an_error() {
        let (addr, server) = spawn_test_server(
            |stream, _request| {
                respond(
                    stream,
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: 14\r\nConnection: close\r\n\r\n{\"pending\":[]}",
                );
            },
            1,
        );

        let client = PolicyClient::new(format!("http://{addr}"));
        let result = client.pending();
        server.join().expect("server thread");
        assert!(
            result.is_err(),
            "500 must not look like an empty queue: {result:?}"
        );
        let message = format!("{:#}", result.unwrap_err());
        assert!(
            message.contains("500"),
            "error must surface the HTTP status: {message}"
        );
    }

    /// A 2xx from POST /approve must be reported as success and must carry the
    /// configured approval token. The GET path is covered separately by
    /// `approval_token_is_sent_as_header`.
    #[test]
    fn approve_2xx_is_ok_and_sends_token() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let seen_for_thread = seen.clone();
        let (addr, handle) = spawn_test_server(
            move |stream, request| {
                *seen_for_thread.lock().expect("lock") = Some(request);
                respond(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 82\r\nConnection: close\r\n\r\n{\"decision\":\"Approved\",\"approved_by\":\"koppe\",\"approved_at\":\"2026-11-15T08:00:00Z\"}",
                );
            },
            1,
        );

        let client = PolicyClient::new_with_token(
            format!("http://{addr}"),
            Some("approve-token".to_string()),
        );
        let result = client.approve("req-1", "koppe");
        handle.join().expect("server thread");
        assert!(result.is_ok(), "2xx approve must succeed: {result:?}");
        assert!(matches!(
            result.expect("canonical decision returned"),
            Decision::Approved { .. }
        ));

        let request = seen.lock().expect("lock").clone().expect("request seen");
        assert!(
            request.starts_with("POST /v1/approval/req-1/approve "),
            "approve must POST to the approve endpoint: {request}"
        );
        assert_eq!(approval_token(&request).as_deref(), Some("approve-token"));
    }

    /// A 2xx from POST /deny must be reported as success and must carry the
    /// configured approval token, mirroring the approve success path.
    #[test]
    fn deny_2xx_is_ok_and_sends_token() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let seen_for_thread = seen.clone();
        let (addr, handle) = spawn_test_server(
            move |stream, request| {
                *seen_for_thread.lock().expect("lock") = Some(request);
                respond(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 40\r\nConnection: close\r\n\r\n{\"decision\":\"Denied\",\"reason\":\"because\"}",
                );
            },
            1,
        );

        let client =
            PolicyClient::new_with_token(format!("http://{addr}"), Some("deny-token".to_string()));
        let result = client.deny("req-2", "because");
        handle.join().expect("server thread");
        assert!(result.is_ok(), "2xx deny must succeed: {result:?}");
        assert!(matches!(
            result.expect("canonical decision returned"),
            Decision::Denied { .. }
        ));

        let request = seen.lock().expect("lock").clone().expect("request seen");
        assert!(
            request.starts_with("POST /v1/approval/req-2/deny "),
            "deny must POST to the deny endpoint: {request}"
        );
        assert_eq!(approval_token(&request).as_deref(), Some("deny-token"));
    }

    /// send_json() returns Ok for any HTTP response, so the status must be
    /// checked before reporting an approve/deny outcome to the operator.
    #[test]
    fn non_2xx_decide_status_is_an_error() {
        let mut responses = [
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ]
        .into_iter();
        let (addr, server) = spawn_test_server(
            move |stream, _request| {
                let response = responses.next().expect("queued response");
                respond(stream, response);
            },
            2,
        );

        let client = PolicyClient::new(format!("http://{addr}"));
        let approved = client.approve("req-1", "koppe");
        let denied = client.deny("req-2", "because");
        server.join().expect("server thread");
        assert!(
            approved.is_err(),
            "401 must not look successful: {approved:?}"
        );
        assert!(denied.is_err(), "500 must not look successful: {denied:?}");

        let approved_message = format!("{:#}", approved.unwrap_err());
        let denied_message = format!("{:#}", denied.unwrap_err());
        assert!(
            approved_message.contains("401"),
            "approve error must surface the status: {approved_message}"
        );
        assert!(
            denied_message.contains("500"),
            "deny error must surface the status: {denied_message}"
        );
    }

    fn timestamp(secs: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(secs, 0).expect("valid timestamp")
    }

    /// I30-D regression: the POST approve response body crosses the canonical
    /// core boundary on the real RUN path, not only in a direct unit test.
    #[test]
    fn approve_response_body_crosses_the_core_boundary() {
        let (addr, handle) = spawn_test_server(
            |stream, _request| {
                respond(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 82\r\nConnection: close\r\n\r\n{\"decision\":\"Approved\",\"approved_by\":\"koppe\",\"approved_at\":\"2026-11-15T08:00:00Z\"}",
                );
            },
            1,
        );
        let client = PolicyClient::new(format!("http://{addr}"));
        let decision = client.approve("4d1a1f2e-0000-0000-0000-000000000000", "koppe");
        handle.join().expect("server thread");
        match decision.expect("approve response parses as canonical decision") {
            Decision::Approved {
                approved_by,
                approved_at,
            } => {
                assert_eq!(approved_by, "koppe");
                assert_eq!(
                    approved_at,
                    chrono::DateTime::parse_from_rfc3339("2026-11-15T08:00:00Z")
                        .expect("timestamp parses")
                        .with_timezone(&chrono::Utc)
                );
            }
            other => panic!("expected canonical Approved, got {other:?}"),
        }
    }

    /// I30-D regression: the same boundary is applied to deny responses.
    #[test]
    fn deny_response_body_crosses_the_core_boundary() {
        let (addr, handle) = spawn_test_server(
            |stream, _request| {
                respond(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 40\r\nConnection: close\r\n\r\n{\"decision\":\"Denied\",\"reason\":\"because\"}",
                );
            },
            1,
        );
        let client = PolicyClient::new(format!("http://{addr}"));
        let decision = client.deny("4d1a1f2e-0000-0000-0000-000000000000", "because");
        handle.join().expect("server thread");
        match decision.expect("deny response parses as canonical decision") {
            Decision::Denied { reason } => assert_eq!(reason, "because"),
            other => panic!("expected canonical Denied, got {other:?}"),
        }
    }

    /// I30-D regression: a malformed decision on the real path fails closed
    /// instead of being reported as a successful approval.
    #[test]
    fn malformed_decision_response_fails_on_the_real_path() {
        let (addr, handle) = spawn_test_server(
            |stream, _request| {
                respond(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 77\r\nConnection: close\r\n\r\n{\"decision\":\"Approved\",\"approved_by\":\"koppe\",\"approved_at\":\"not-a-timestamp\"}",
                );
            },
            1,
        );
        let client = PolicyClient::new(format!("http://{addr}"));
        let decision = client.approve("4d1a1f2e-0000-0000-0000-000000000000", "koppe");
        handle.join().expect("server thread");
        assert!(
            decision.is_err(),
            "a malformed 2xx decision must not be reported as success: {decision:?}"
        );
    }

    /// TC-U5: tagged decision JSON converts to the canonical core type; the
    /// wire contract (tag + field names + RFC 3339 timestamp) stays unchanged.
    #[test]
    fn tagged_decision_json_converts_to_canonical_core() {
        let approved = serde_json::json!({
            "decision": "Approved",
            "approved_by": "koppe",
            "approved_at": "2026-11-15T08:00:00Z",
        });
        assert_eq!(
            decision_from_json(&approved).expect("approved converts"),
            Decision::Approved {
                approved_by: "koppe".into(),
                approved_at: chrono::DateTime::parse_from_rfc3339("2026-11-15T08:00:00Z")
                    .expect("timestamp parses")
                    .with_timezone(&chrono::Utc),
            }
        );

        let denied = serde_json::json!({ "decision": "Denied", "reason": "not now" });
        assert_eq!(
            decision_from_json(&denied).expect("denied converts"),
            Decision::Denied {
                reason: "not now".into()
            }
        );
        assert_eq!(
            decision_from_json(&serde_json::json!({ "decision": "Pending" }))
                .expect("pending converts"),
            Decision::Pending
        );
        assert_eq!(
            decision_from_json(&serde_json::json!({ "decision": "Expired" }))
                .expect("expired converts"),
            Decision::Expired
        );
    }

    /// TC-U6: legacy two-variant decisions are rejected explicitly after the
    /// core legacy type removal (P1-7 slice 4), and a malformed RFC 3339
    /// timestamp fails closed instead of being defaulted.
    #[test]
    fn legacy_decision_json_is_rejected_at_boundary() {
        let legacy_approved = serde_json::json!("Approved");
        assert!(decision_from_json(&legacy_approved).is_err());
        let legacy_denied = serde_json::json!({ "Denied": { "reason": "legacy no" } });
        assert!(decision_from_json(&legacy_denied).is_err());

        let malformed = serde_json::json!({
            "decision": "Approved",
            "approved_by": "koppe",
            "approved_at": "not-a-timestamp",
        });
        assert!(
            decision_from_json(&malformed).is_err(),
            "malformed timestamp must fail at the boundary"
        );
    }

    /// TC-U7: a full wire request projects onto the canonical core type with
    /// every field preserved, and the core JSON shape stays RFC 3339/UUID.
    #[test]
    fn wire_request_converts_to_canonical_core_keeping_the_json_shape() {
        let wire = ApprovalRequest {
            id: "3f9a1c2e-7b4d-4a1f-9c33-0d2e5f6a7b88".into(),
            principal: "opencode".into(),
            action: "execute".into(),
            resource: "tool:shell.execute".into(),
            context: serde_json::json!({ "cmd": "ls" }),
            requested_at: Some(timestamp(0)),
            expires_at: Some(timestamp(60)),
        };
        let core = request_to_core(&wire).expect("converts");
        assert_eq!(core.principal, "opencode");
        assert_eq!(core.action, "execute");
        assert_eq!(core.resource, "tool:shell.execute");
        assert_eq!(core.context, serde_json::json!({ "cmd": "ls" }));
        assert_eq!(core.requested_at, timestamp(0));
        assert_eq!(core.expires_at, timestamp(60));

        let json = serde_json::to_value(&core).expect("canonical request serializes");
        assert_eq!(json["id"], "3f9a1c2e-7b4d-4a1f-9c33-0d2e5f6a7b88");
        assert_eq!(json["requested_at"], "1970-01-01T00:00:00Z");
        assert_eq!(json["expires_at"], "1970-01-01T00:01:00Z");
    }

    /// TC-U8: legacy payloads that omit `requested_at`/`expires_at` still cross
    /// the boundary (nullable timestamps become the epoch), while a missing or
    /// malformed UUID id is rejected explicitly instead of being fabricated.
    #[test]
    fn legacy_wire_request_tolerance_and_explicit_id_error() {
        let legacy_json = serde_json::json!({
            "principal": "jarvis",
            "action": "execute",
            "resource": "tool:git.commit",
        });
        let wire: ApprovalRequest = serde_json::from_value(legacy_json).expect("legacy parses");
        assert_eq!(wire.requested_at, None);
        assert_eq!(wire.expires_at, None);
        assert!(
            request_to_core(&wire).is_err(),
            "empty id must fail explicitly"
        );

        let with_bad_id = ApprovalRequest {
            id: "not-a-uuid".into(),
            requested_at: None,
            expires_at: None,
            ..wire
        };
        assert!(
            request_to_core(&with_bad_id).is_err(),
            "malformed id must fail explicitly"
        );
    }

    // --- R4-UI: env name -> header scope contract ---------------------------

    fn env_getter<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_string())
        }
    }

    #[test]
    fn canonical_env_name_resolves_to_api_token() {
        let pairs = [(ENV_APPROVAL_API_TOKEN, "canonical-api-value")];
        let tokens = tokens_from_env_with(env_getter(&pairs));
        assert_eq!(tokens.api_token.as_deref(), Some("canonical-api-value"));
        assert_eq!(tokens.admin_token, None);
        assert!(!tokens.legacy_api_env_used());
    }

    #[test]
    fn legacy_env_name_falls_back_and_flags_deprecation() {
        let pairs = [(ENV_LEGACY_APPROVAL_TOKEN, "legacy-api-value")];
        let tokens = tokens_from_env_with(env_getter(&pairs));
        assert_eq!(tokens.api_token.as_deref(), Some("legacy-api-value"));
        assert!(tokens.legacy_api_env_used());

        // The canonical name wins when both are set; no warning is then due.
        let both_pairs = [
            (ENV_APPROVAL_API_TOKEN, "canonical-api-value"),
            (ENV_LEGACY_APPROVAL_TOKEN, "legacy-api-value"),
        ];
        let both = tokens_from_env_with(env_getter(&both_pairs));
        assert_eq!(both.api_token.as_deref(), Some("canonical-api-value"));
        assert!(!both.legacy_api_env_used());
    }

    #[test]
    fn admin_env_token_is_scoped_and_falls_back_to_api() {
        let admin_pairs = [
            (ENV_APPROVAL_API_TOKEN, "api-value"),
            (ENV_APPROVAL_ADMIN_TOKEN, "admin-value"),
        ];
        let with_admin = tokens_from_env_with(env_getter(&admin_pairs));
        assert_eq!(with_admin.api_token.as_deref(), Some("api-value"));
        assert_eq!(with_admin.admin_token.as_deref(), Some("admin-value"));
        assert_eq!(with_admin.effective_admin_token(), Some("admin-value"));

        let api_pairs = [(ENV_APPROVAL_API_TOKEN, "api-value")];
        let without_admin = tokens_from_env_with(env_getter(&api_pairs));
        assert_eq!(without_admin.admin_token, None);
        assert_eq!(without_admin.effective_admin_token(), Some("api-value"));
    }

    #[test]
    fn blank_env_values_are_treated_as_unset() {
        let pairs = [
            (ENV_APPROVAL_API_TOKEN, "   "),
            (ENV_LEGACY_APPROVAL_TOKEN, "legacy-api-value"),
            (ENV_APPROVAL_ADMIN_TOKEN, ""),
        ];
        let tokens = tokens_from_env_with(env_getter(&pairs));
        assert_eq!(tokens.api_token.as_deref(), Some("legacy-api-value"));
        assert!(tokens.legacy_api_env_used());
        assert_eq!(tokens.admin_token, None);
    }

    #[test]
    fn token_bundle_round_trips_through_single_string_channel() {
        let tokens = PolicyTokens::new(Some("api".into()), Some("admin".into()));
        let encoded = tokens.encode().expect("bundle encoded");
        assert!(encoded.starts_with(TOKEN_WIRE_PREFIX));
        let decoded = PolicyTokens::decode_or_legacy(Some(encoded));
        assert_eq!(decoded.api_token.as_deref(), Some("api"));
        assert_eq!(decoded.admin_token.as_deref(), Some("admin"));

        let legacy = PolicyTokens::decode_or_legacy(Some("plain-legacy".into()));
        assert_eq!(legacy.api_token.as_deref(), Some("plain-legacy"));
        assert_eq!(legacy.admin_token, None);

        let malformed_raw = format!("{TOKEN_WIRE_PREFIX}not-json");
        let malformed = PolicyTokens::decode_or_legacy(Some(malformed_raw.clone()));
        assert_eq!(malformed.api_token.as_deref(), Some(malformed_raw.as_str()));
        assert_eq!(PolicyTokens::new(None, None).encode(), None);
    }

    #[test]
    fn pending_sends_api_token_when_admin_token_is_separate() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let seen_for_thread = seen.clone();
        let (addr, handle) = spawn_test_server(
            move |stream, request| {
                *seen_for_thread.lock().expect("lock") = Some(request);
                respond(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 14\r\n\r\n{\"pending\":[]}",
                );
            },
            1,
        );

        let client = PolicyClient::new_with_tokens(
            format!("http://{addr}"),
            Some("api-scope-value".to_string()),
            Some("admin-scope-value".to_string()),
        );
        assert!(client.pending().is_ok());
        handle.join().expect("server thread");
        let request = seen.lock().expect("lock").clone().expect("request seen");
        assert_eq!(approval_token(&request).as_deref(), Some("api-scope-value"));
        assert!(!request.contains("admin-scope-value"));
    }

    #[test]
    fn approve_and_deny_send_admin_token_not_api_token() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_for_thread = seen.clone();
        let mut responses = [
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 82\r\nConnection: close\r\n\r\n{\"decision\":\"Approved\",\"approved_by\":\"koppe\",\"approved_at\":\"2026-11-15T08:00:00Z\"}",
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 40\r\nConnection: close\r\n\r\n{\"decision\":\"Denied\",\"reason\":\"because\"}",
        ]
        .into_iter();
        let (addr, handle) = spawn_test_server(
            move |stream, request| {
                seen_for_thread.lock().expect("lock").push(request);
                respond(stream, responses.next().expect("queued response"));
            },
            2,
        );

        let client = PolicyClient::new_with_tokens(
            format!("http://{addr}"),
            Some("api-scope-value".to_string()),
            Some("admin-scope-value".to_string()),
        );
        assert!(client.approve("req-1", "koppe").is_ok());
        assert!(client.deny("req-2", "because").is_ok());
        handle.join().expect("server thread");
        let requests = seen.lock().expect("lock");
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert_eq!(
                approval_token(request).as_deref(),
                Some("admin-scope-value")
            );
            assert!(!request.contains("api-scope-value"));
        }
    }

    #[test]
    fn decide_falls_back_to_api_token_without_admin_token() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let seen_for_thread = seen.clone();
        let (addr, handle) = spawn_test_server(
            move |stream, request| {
                *seen_for_thread.lock().expect("lock") = Some(request);
                respond(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 40\r\nConnection: close\r\n\r\n{\"decision\":\"Denied\",\"reason\":\"because\"}",
                );
            },
            1,
        );

        let client = PolicyClient::new_with_tokens(
            format!("http://{addr}"),
            Some("fallback-api-value".to_string()),
            None,
        );
        assert!(client.deny("req-2", "because").is_ok());
        handle.join().expect("server thread");
        let request = seen.lock().expect("lock").clone().expect("request seen");
        assert_eq!(
            approval_token(&request).as_deref(),
            Some("fallback-api-value")
        );
    }

    #[test]
    fn debug_output_redacts_every_token_value() {
        let tokens = PolicyTokens::new(Some("api-secret".into()), Some("admin-secret".into()));
        let client = PolicyClient::new_with_tokens(
            "http://127.0.0.1:8081",
            Some("api-secret".into()),
            Some("admin-secret".into()),
        );
        let tokens_debug = format!("{tokens:?}");
        let client_debug = format!("{client:?}");
        for rendered in [&tokens_debug, &client_debug] {
            assert!(!rendered.contains("api-secret"), "{rendered}");
            assert!(!rendered.contains("admin-secret"), "{rendered}");
            assert!(rendered.contains("redacted"), "{rendered}");
        }
    }

    // --- I38-D / TC-U13: real POST send path scope resolution ---------------

    /// Fixed sentinels only: real token values must never reach logs, errors
    /// or assertion text. Assertions below compare values without printing
    /// them, so even a failure keeps the sentinel out of the output.
    const TC_U13_API_SCOPE_TOKEN: &str = "tc-u13-api-scope-token";
    const TC_U13_ADMIN_SCOPE_TOKEN: &str = "tc-u13-admin-scope-token";

    /// Drive both decision POSTs through a loopback stub and return the raw
    /// requests in call order (`approve`, then `deny`). The factory receives
    /// the stub base URL so each test picks the production wiring to exercise.
    fn capture_decide_requests(client_factory: impl FnOnce(String) -> PolicyClient) -> Vec<String> {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_for_thread = seen.clone();
        let mut responses = [
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 82\r\nConnection: close\r\n\r\n{\"decision\":\"Approved\",\"approved_by\":\"koppe\",\"approved_at\":\"2026-11-15T08:00:00Z\"}",
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 40\r\nConnection: close\r\n\r\n{\"decision\":\"Denied\",\"reason\":\"because\"}",
        ]
        .into_iter();
        let (addr, handle) = spawn_test_server(
            move |stream, request| {
                seen_for_thread.lock().expect("lock").push(request);
                respond(stream, responses.next().expect("queued response"));
            },
            2,
        );

        let client = client_factory(format!("http://{addr}"));
        let approved = client.approve("req-1", "koppe");
        let denied = client.deny("req-2", "because");
        handle.join().expect("server thread");
        assert!(approved.is_ok(), "approve must succeed against the stub");
        assert!(denied.is_ok(), "deny must succeed against the stub");
        let requests = seen.lock().expect("lock").clone();
        assert_eq!(requests.len(), 2, "both POSTs must reach the stub");
        requests
    }

    /// TC-U13 real send path: without an admin scope, `approve` must fall back
    /// to the API-scope token in `X-Jarvis-Approval-Token`.
    #[test]
    fn tc_u13_approve_sends_api_scope_token_when_admin_scope_is_unset() {
        let requests = capture_decide_requests(|base_url| {
            PolicyClient::new_with_tokens(base_url, Some(TC_U13_API_SCOPE_TOKEN.into()), None)
        });
        let request = &requests[0];
        assert!(
            request.starts_with("POST /v1/approval/req-1/approve "),
            "approve must POST to the approve endpoint"
        );
        assert!(
            approval_token(request).as_deref() == Some(TC_U13_API_SCOPE_TOKEN),
            "approve must send the API-scope token when admin scope is unset"
        );
    }

    /// TC-U13 real send path: the same fallback must apply to `deny`.
    #[test]
    fn tc_u13_deny_sends_api_scope_token_when_admin_scope_is_unset() {
        let requests = capture_decide_requests(|base_url| {
            PolicyClient::new_with_tokens(base_url, Some(TC_U13_API_SCOPE_TOKEN.into()), None)
        });
        let request = &requests[1];
        assert!(
            request.starts_with("POST /v1/approval/req-2/deny "),
            "deny must POST to the deny endpoint"
        );
        assert!(
            approval_token(request).as_deref() == Some(TC_U13_API_SCOPE_TOKEN),
            "deny must send the API-scope token when admin scope is unset"
        );
    }

    /// TC-U13 real send path through the *startup* wiring: the encoded
    /// two-token bundle decoded by `new_with_token` must still resolve to the
    /// admin scope on both POSTs, never the API scope.
    #[test]
    fn tc_u13_admin_scope_wins_on_real_send_path_via_encoded_bundle() {
        let requests = capture_decide_requests(|base_url| {
            let tokens = PolicyTokens::new(
                Some(TC_U13_API_SCOPE_TOKEN.into()),
                Some(TC_U13_ADMIN_SCOPE_TOKEN.into()),
            );
            let encoded = tokens.encode().expect("bundle encodes");
            PolicyClient::new_with_token(base_url, Some(encoded))
        });
        for request in &requests {
            assert!(
                approval_token(request).as_deref() == Some(TC_U13_ADMIN_SCOPE_TOKEN),
                "decisions must send the admin-scope token via the encoded bundle"
            );
            assert!(
                !request.contains(TC_U13_API_SCOPE_TOKEN),
                "decisions must not send the API-scope token when admin scope is set"
            );
        }
    }

    /// TC-U13 real send path: an admin-only configuration must send the admin
    /// scope on both POSTs (no API scope to fall back to).
    #[test]
    fn tc_u13_admin_scope_is_sent_when_api_scope_is_unset() {
        let requests = capture_decide_requests(|base_url| {
            PolicyClient::new_with_tokens(base_url, None, Some(TC_U13_ADMIN_SCOPE_TOKEN.into()))
        });
        for request in &requests {
            assert!(
                approval_token(request).as_deref() == Some(TC_U13_ADMIN_SCOPE_TOKEN),
                "decisions must send the admin-scope token when it is the only scope"
            );
        }
    }

    /// TC-U13 real send path: no configured scope means no approval-token
    /// header at all on either POST.
    #[test]
    fn tc_u13_no_token_header_when_no_scope_is_configured() {
        let requests =
            capture_decide_requests(|base_url| PolicyClient::new_with_tokens(base_url, None, None));
        for request in &requests {
            assert!(
                approval_token(request).is_none(),
                "no X-Jarvis-Approval-Token header may be sent when no scope is configured"
            );
        }
    }

    /// TC-U13 real send path through the *startup* wiring: an API-only
    /// configuration encoded by `PolicyTokens::encode` and decoded by
    /// `new_with_token` must fall back to the API scope on both POSTs. This
    /// exercises `decode_or_legacy -> effective_admin_token -> POST`, the same
    /// path production takes via `ApprovalsView::new`.
    #[test]
    fn tc_u13_api_scope_fallback_via_encoded_bundle_on_real_send_path() {
        let requests = capture_decide_requests(|base_url| {
            let tokens = PolicyTokens::new(Some(TC_U13_API_SCOPE_TOKEN.into()), None);
            let encoded = tokens.encode().expect("bundle encodes");
            PolicyClient::new_with_token(base_url, Some(encoded))
        });
        for request in &requests {
            assert!(
                approval_token(request).as_deref() == Some(TC_U13_API_SCOPE_TOKEN),
                "decisions must fall back to the API-scope token via the encoded bundle"
            );
            assert!(
                !request.contains(TC_U13_ADMIN_SCOPE_TOKEN),
                "decisions must not send the admin-scope token when it is unset"
            );
        }
    }

    /// TC-U13 real send path through the *startup* wiring: an admin-only
    /// configuration encoded by `PolicyTokens::encode` and decoded by
    /// `new_with_token` must send the admin scope on both POSTs.
    #[test]
    fn tc_u13_admin_only_scope_via_encoded_bundle_on_real_send_path() {
        let requests = capture_decide_requests(|base_url| {
            let tokens = PolicyTokens::new(None, Some(TC_U13_ADMIN_SCOPE_TOKEN.into()));
            let encoded = tokens.encode().expect("bundle encodes");
            PolicyClient::new_with_token(base_url, Some(encoded))
        });
        for request in &requests {
            assert!(
                approval_token(request).as_deref() == Some(TC_U13_ADMIN_SCOPE_TOKEN),
                "decisions must send the admin-scope token via the encoded bundle"
            );
            assert!(
                !request.contains(TC_U13_API_SCOPE_TOKEN),
                "decisions must not send the API-scope token when it is unset"
            );
        }
    }

    // --- I48-D: post-approval execution-stop display probe -----------------

    fn respond_json(stream: &mut TcpStream, status_line: &str, body: &str) {
        let response = format!(
            "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        respond(stream, &response);
    }

    /// I48-D: only an explicit `reason=kill_switch` on a 503 reports the
    /// emergency stop; a healthy service must not produce a false "halted"
    /// display.
    #[test]
    fn kill_switch_active_reads_degraded_health_reason() {
        let (addr, server) = spawn_test_server(
            |stream, _request| {
                respond_json(
                    stream,
                    "HTTP/1.1 503 Service Unavailable",
                    r#"{"status":"degraded","reason":"kill_switch"}"#,
                )
            },
            1,
        );
        let client = PolicyClient::new(format!("http://{addr}"));
        assert!(client.kill_switch_active());
        server.join().expect("server thread");
    }

    /// I48-D: `200 status=ok` and a different degraded reason are definitive
    /// releases and report `false`, keeping the approval status truthful. An
    /// unexpected 200 body is no longer a release; see the I60-A regression
    /// `health_probe_unexpected_200_body_fails_closed` below.
    #[test]
    fn kill_switch_active_is_false_for_ok_and_other_reasons() {
        let mut responses = [
            ("HTTP/1.1 200 OK", r#"{"status":"ok"}"#),
            (
                "HTTP/1.1 503 Service Unavailable",
                r#"{"status":"degraded","reason":"no_policy"}"#,
            ),
        ]
        .into_iter();
        let (addr, server) = spawn_test_server(
            move |stream, _request| {
                let (status, body) = responses.next().expect("queued response");
                respond_json(stream, status, body);
            },
            2,
        );
        let client = PolicyClient::new(format!("http://{addr}"));
        for _ in 0..2 {
            assert!(!client.kill_switch_active());
        }
        server.join().expect("server thread");
    }

    /// I54-A: a health probe that never completes must not be projected as
    /// "switch lifted". Failing closed keeps a confirmed halt pin alive after
    /// a kill-switch 409; the raw deadline itself is already covered by
    /// `hung_health_probe_respects_configured_deadline`.
    #[test]
    fn kill_switch_active_fails_closed_on_probe_timeout() {
        let (addr, server) = spawn_test_server(
            |stream, _request| {
                std::thread::sleep(Duration::from_secs(5));
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            },
            1,
        );
        let client = PolicyClient::with_timeouts(
            format!("http://{addr}"),
            None,
            Duration::from_millis(150),
            Duration::from_secs(1),
        );
        let started = std::time::Instant::now();
        assert!(
            client.kill_switch_active(),
            "probe timeout must fail closed, not lift the halt"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "deadline must fire long before the 5s server response"
        );
        let _ = server.join();
    }

    /// I54-A: an unreadable degraded body is also an incomplete probe and
    /// fails closed, while a later explicit `200 ok` still lifts the stop.
    /// This pins both halves of the release contract at the client boundary.
    #[test]
    fn kill_switch_active_fails_closed_on_unreadable_degraded_body_then_releases_on_ok() {
        let mut responses = [
            ("HTTP/1.1 503 Service Unavailable", "not-json"),
            ("HTTP/1.1 200 OK", r#"{"status":"ok"}"#),
        ]
        .into_iter();
        let (addr, server) = spawn_test_server(
            move |stream, _request| {
                let (status, body) = responses.next().expect("queued response");
                respond_json(stream, status, body);
            },
            2,
        );
        let client = PolicyClient::new(format!("http://{addr}"));
        assert!(
            client.kill_switch_active(),
            "unreadable 503 must fail closed, not lift the halt"
        );
        assert!(
            !client.kill_switch_active(),
            "an explicit 200 ok must still release the confirmed stop"
        );
        server.join().expect("server thread");
    }

    // --- I60-A: probe status/body classification regressions ----------------

    /// Observe both halves of the incomplete-probe contract on one client: the
    /// raw probe must return `Err`, and the public bool must fail closed.
    fn assert_health_status_fails_closed(status_line: &str, body: &str) {
        let status_line = status_line.to_string();
        let body = body.to_string();
        let label = status_line.clone();
        let (addr, server) = spawn_test_server(
            move |stream, _request| respond_json(stream, status_line.as_str(), body.as_str()),
            2,
        );
        let client = PolicyClient::new(format!("http://{addr}"));
        assert!(
            client.kill_switch_probe().is_err(),
            "raw probe must return Err for {label}"
        );
        assert!(
            client.kill_switch_active(),
            "public probe must fail closed for {label}"
        );
        server.join().expect("server thread");
    }

    /// I60-A: a reverse proxy's 500/502/504 used to be classified as "not
    /// stopped" and released the halt pin. They are incomplete probes now.
    #[test]
    fn health_probe_proxy_errors_fail_closed() {
        for status in [
            "HTTP/1.1 500 Internal Server Error",
            "HTTP/1.1 502 Bad Gateway",
            "HTTP/1.1 504 Gateway Timeout",
        ] {
            assert_health_status_fails_closed(status, r#"{"status":"ok"}"#);
        }
    }

    /// I60-A: a 3xx is not a definitive health answer either; it must fail
    /// closed even when its body looks healthy.
    #[test]
    fn health_probe_3xx_status_fails_closed() {
        for status in [
            "HTTP/1.1 301 Moved Permanently",
            "HTTP/1.1 302 Found",
            "HTTP/1.1 307 Temporary Redirect",
        ] {
            assert_health_status_fails_closed(status, r#"{"status":"ok"}"#);
        }
    }

    /// I60-A: a 200 is only a release when its body carries `status=ok`. Any
    /// other body is unexpected and must fail closed instead of lifting the
    /// halt.
    #[test]
    fn health_probe_unexpected_200_body_fails_closed() {
        for body in [
            "not-json",
            r#"{"status":"degraded"}"#,
            r#"{"status":"degraded","reason":"kill_switch"}"#,
            r#"{}"#,
            "",
        ] {
            let body = body.to_string();
            let label = body.clone();
            let (addr, server) = spawn_test_server(
                move |stream, _request| respond_json(stream, "HTTP/1.1 200 OK", body.as_str()),
                2,
            );
            let client = PolicyClient::new(format!("http://{addr}"));
            assert!(
                client.kill_switch_probe().is_err(),
                "raw probe must reject an unexpected 200 body: {label}"
            );
            assert!(
                client.kill_switch_active(),
                "an unexpected 200 body must fail closed: {label}"
            );
            server.join().expect("server thread");
        }
    }

    /// I60-A: the exact bypass from the review finding. With `max_redirects(0)`
    /// a proxy 302 is returned as-is rather than followed to a 200
    /// `{"status":"ok"}` release. The target must see no follow-up request.
    #[test]
    fn health_probe_does_not_follow_redirect() {
        use std::sync::mpsc;
        use std::time::Instant;

        let target = TcpListener::bind("127.0.0.1:0").expect("bind redirect target");
        let target_addr = target.local_addr().expect("target addr");
        let (seen_tx, seen_rx) = mpsc::channel();
        let target_thread = std::thread::spawn(move || {
            target
                .set_nonblocking(true)
                .expect("set nonblocking on target");
            let deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < deadline {
                match target.accept() {
                    Ok((mut stream, _)) => {
                        let mut buf = [0u8; 1024];
                        let _ = stream.read(&mut buf);
                        // A followed redirect would otherwise answer with an
                        // explicit healthy release and drop the halt pin.
                        let body = r#"{"status":"ok"}"#;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                        let _ = seen_tx.send(true);
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => return,
                }
            }
            let _ = seen_tx.send(false);
        });

        let (addr, server) = spawn_test_server(
            move |stream, _request| {
                let response = format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://{target_addr}/health\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                respond(stream, &response);
            },
            2,
        );

        let client = PolicyClient::new(format!("http://{addr}"));
        assert!(
            client.kill_switch_probe().is_err(),
            "an unfollowed 302 must be an incomplete probe"
        );
        assert!(
            client.kill_switch_active(),
            "an unfollowed 302 must not lift the halt"
        );
        server.join().expect("server thread");
        let followed = seen_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("redirect target probe result");
        target_thread.join().expect("redirect target thread");
        assert!(!followed, "the 302 must not be followed to the target");
    }

    // --- I50-C: winner preservation (409) and probe deadlines ---------------

    /// I50-C (b): an HTTP 409 on approve is not a failure. The authoritative
    /// winner is read from the status endpoint and reported as AlreadyDecided,
    /// even when it differs from the operator's intent (I48-C). No reason text
    /// is printed, displayed or logged on this path.
    #[test]
    fn already_decided_409_returns_authoritative_winner() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_for_thread = seen.clone();
        let mut bodies = [
            ("HTTP/1.1 409 Conflict", ALREADY_DECIDED_409_BODY),
            (
                "HTTP/1.1 200 OK",
                r#"{"decision":"Denied","reason":"competing operator"}"#,
            ),
        ]
        .into_iter();
        let (addr, handle) = spawn_test_server(
            move |stream, request| {
                seen_for_thread.lock().expect("lock").push(request);
                let (status, body) = bodies.next().expect("queued response");
                respond_json(stream, status, body);
            },
            2,
        );

        let id = "3f9a1c2e-7b4d-4a1f-9c33-0d2e5f6a7b88";
        let client = PolicyClient::new(format!("http://{addr}"));
        let outcome = client.approve_outcome(id, "koppe");
        handle.join().expect("server thread");

        let outcome = outcome.expect("409 resolves to the winner decision");
        let is_winner_denied = matches!(
            outcome,
            DecisionOutcome::AlreadyDecided(Decision::Denied { .. })
        );
        assert!(
            is_winner_denied,
            "409 must resolve to the authoritative winner"
        );

        let requests = seen.lock().expect("lock");
        assert_eq!(requests.len(), 2, "POST then status GET");
        assert!(
            requests[0].starts_with(&format!("POST /v1/approval/{id}/approve ")),
            "first request must be the decision POST"
        );
        assert!(
            requests[1].starts_with(&format!("GET /v1/approval/{id} ")),
            "second request must be the status GET"
        );
    }

    /// I50-C (b): the same winner-preservation path applies to a deny POST.
    /// The operator's intent (Denied) cannot masquerade as the outcome when the
    /// winner is Approved.
    #[test]
    fn already_decided_409_on_deny_returns_approved_winner() {
        let mut bodies = [
            ("HTTP/1.1 409 Conflict", ALREADY_DECIDED_409_BODY),
            (
                "HTTP/1.1 200 OK",
                r#"{"decision":"Approved","approved_by":"other","approved_at":"2026-11-15T08:00:00Z"}"#,
            ),
        ]
        .into_iter();
        let (addr, handle) = spawn_test_server(
            move |stream, _request| {
                let (status, body) = bodies.next().expect("queued response");
                respond_json(stream, status, body);
            },
            2,
        );

        let client = PolicyClient::new(format!("http://{addr}"));
        let outcome = client.deny_outcome("req-1", "because");
        handle.join().expect("server thread");

        let outcome = outcome.expect("409 resolves to the winner decision");
        let is_winner_approved = matches!(
            outcome,
            DecisionOutcome::AlreadyDecided(Decision::Approved { .. })
        );
        assert!(
            is_winner_approved,
            "the winner must win over the deny intent"
        );
    }

    /// I50-C (b): a 409 whose winner cannot be read is an explicit error, not
    /// a silent success. The error text carries only URLs/status, never a
    /// reason body or token.
    #[test]
    fn already_decided_winner_fetch_failure_is_an_error() {
        let mut bodies = [
            ("HTTP/1.1 409 Conflict", ALREADY_DECIDED_409_BODY),
            ("HTTP/1.1 500 Internal Server Error", "unavailable"),
        ]
        .into_iter();
        let (addr, handle) = spawn_test_server(
            move |stream, _request| {
                let (status, body) = bodies.next().expect("queued response");
                respond_json(stream, status, body);
            },
            2,
        );

        let client = PolicyClient::new(format!("http://{addr}"));
        let outcome = client.approve_outcome("req-1", "koppe");
        handle.join().expect("server thread");
        assert!(outcome.is_err(), "unreadable winner must fail closed");
    }

    /// I50-C (c) / I52-A (c): the health probe shares the main agent's
    /// deadlines. The server answers only after 5s and the test observes the
    /// ureq timeout directly; the old 600 ms / <3s version passed even with no
    /// timeout configured. The public `bool` projection stays truthful too.
    #[test]
    fn hung_health_probe_respects_configured_deadline() {
        let (addr, handle) = spawn_test_server(
            |stream, _request| {
                std::thread::sleep(Duration::from_secs(5));
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            },
            1,
        );
        let client = PolicyClient::with_timeouts(
            format!("http://{addr}"),
            None,
            Duration::from_millis(150),
            Duration::from_secs(1),
        );
        let started = std::time::Instant::now();
        let error = client
            .kill_switch_probe()
            .expect_err("hung probe must hit the configured deadline");
        assert!(
            is_timeout(&error),
            "probe must fail via the timeout: {error:#}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "deadline must fire long before the 5s server response"
        );
        let _ = handle.join();
    }

    // --- I52-A: 409 body classification and winner-token contract ----------

    /// I52-A (a) / I54-C: a kill-switch 409 is not an already-decided winner.
    /// No status readback is issued, no decision is fabricated, and the fixed
    /// body is never carried into the outcome.
    ///
    /// Contract pin (I54-C): the stub serves an independent fixture of the body
    /// `jarvis-policy` puts on the wire, not a value derived from the
    /// classification constant. Production compares against the shared
    /// `jarvis-core` constant, so a policy wording change (a change to that
    /// constant) fails the equality pin or the outcome assertion instead of
    /// silently resolving the 409 to a false winner.
    #[test]
    fn kill_switch_409_pins_halt_without_winner_fetch() {
        use std::sync::{Arc, Mutex};

        // External contract fixture, not a second production definition: this
        // literal is what the policy service writes to the 409 response body.
        const POLICY_KILL_SWITCH_409_WIRE_BODY: &str =
            "approval decisions are blocked by the kill switch";
        assert_eq!(
            KILL_SWITCH_409_BODY, POLICY_KILL_SWITCH_409_WIRE_BODY,
            "shared 409 body must still match the policy wire contract"
        );

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_for_thread = seen.clone();
        let (addr, handle) = spawn_test_server(
            move |stream, request| {
                seen_for_thread.lock().expect("lock").push(request);
                respond_json(
                    stream,
                    "HTTP/1.1 409 Conflict",
                    POLICY_KILL_SWITCH_409_WIRE_BODY,
                );
            },
            1,
        );

        let client = PolicyClient::new(format!("http://{addr}"));
        let outcome = client.approve_outcome("req-1", "koppe");
        handle.join().expect("server thread");

        assert_eq!(
            outcome.expect("kill-switch 409 is a classified outcome"),
            DecisionOutcome::KillSwitchBlocked
        );
        let requests = seen.lock().expect("lock");
        assert_eq!(requests.len(), 1, "no winner GET after a kill-switch 409");
        assert!(
            requests[0].starts_with("POST /v1/approval/req-1/approve "),
            "only the decision POST must be sent"
        );
    }

    /// I56-B: an empty 409 body must not be treated as "another 409" and
    /// routed to the winner readback. The old `.unwrap_or_default()` made both
    /// a read error and a literal empty body fall through to `winner_decision`,
    /// which is fail-open: an unreadable kill-switch body would be reported as
    /// a decided winner. Now the halt is pinned and no status GET is issued.
    #[test]
    fn empty_409_body_pins_halt_without_winner_fetch() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_for_thread = seen.clone();
        let (addr, handle) = spawn_test_server(
            move |stream, request| {
                seen_for_thread.lock().expect("lock").push(request);
                respond(
                    stream,
                    "HTTP/1.1 409 Conflict\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            },
            1,
        );

        let client = PolicyClient::new(format!("http://{addr}"));
        let outcome = client.approve_outcome("req-1", "koppe");
        handle.join().expect("server thread");

        assert_eq!(
            outcome.expect("empty 409 is still a classified outcome"),
            DecisionOutcome::KillSwitchBlocked
        );
        let requests = seen.lock().expect("lock");
        assert_eq!(
            requests.len(),
            1,
            "no winner GET may follow an empty 409 body"
        );
    }

    /// I58-B: a 409 whose body is neither of the two known literals must pin the
    /// halt. This is the rolling-update / relay regression: a peer on a
    /// different build (or a rewritten response) must never surface a real
    /// emergency stop as "already decided". No status GET is issued.
    #[test]
    fn unknown_409_body_pins_halt_without_winner_fetch() {
        use std::sync::{Arc, Mutex};

        const REWRITTEN_BODY: &str = "kill switch engaged; rewritten by relay";

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_for_thread = seen.clone();
        let (addr, handle) = spawn_test_server(
            move |stream, request| {
                seen_for_thread.lock().expect("lock").push(request);
                respond_json(stream, "HTTP/1.1 409 Conflict", REWRITTEN_BODY);
            },
            1,
        );

        let client = PolicyClient::new(format!("http://{addr}"));
        let outcome = client.approve_outcome("req-1", "koppe");
        handle.join().expect("server thread");

        assert_eq!(
            outcome.expect("unknown 409 is still a classified outcome"),
            DecisionOutcome::KillSwitchBlocked
        );
        assert_eq!(
            seen.lock().expect("lock").len(),
            1,
            "no winner GET may follow an unknown 409 body"
        );
    }

    /// I58-B: a 409 body that cannot be decoded as UTF-8 is unreadable and must
    /// pin the halt instead of falling through to the winner readback. This is
    /// the wire-level counterpart of the `Err` arm of the pure classifier test.
    #[test]
    fn unreadable_409_body_pins_halt_without_winner_fetch() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_for_thread = seen.clone();
        let (addr, handle) = spawn_test_server(
            move |stream, request| {
                seen_for_thread.lock().expect("lock").push(request);
                // Declared length matches the raw bytes: the read fails on
                // invalid UTF-8, not on a truncated transfer.
                stream
                    .write_all(
                        b"HTTP/1.1 409 Conflict\r\nContent-Length: 2\r\nConnection: close\r\n\r\n\xff\xfe",
                    )
                    .expect("write");
            },
            1,
        );

        let client = PolicyClient::new(format!("http://{addr}"));
        let outcome = client.approve_outcome("req-1", "koppe");
        handle.join().expect("server thread");

        assert_eq!(
            outcome.expect("unreadable 409 is still a classified outcome"),
            DecisionOutcome::KillSwitchBlocked
        );
        assert_eq!(
            seen.lock().expect("lock").len(),
            1,
            "no winner GET may follow an unreadable 409 body"
        );
    }

    /// I56-B / I58-B: pure classification of the bounded 409 body read. Only
    /// the exact already-decided literal is benign; a read error, an
    /// empty/whitespace body and any unknown body fail closed to the halt.
    #[test]
    fn kill_switch_409_body_classification_fails_closed() {
        type Body = std::result::Result<String, ()>;
        let kill_switch: Body = Ok(KILL_SWITCH_409_BODY.to_string());
        let already_decided: Body = Ok(ALREADY_DECIDED_409_BODY.to_string());
        let already_decided_padded: Body = Ok(format!(" {ALREADY_DECIDED_409_BODY} "));
        let empty: Body = Ok(String::new());
        let whitespace: Body = Ok("  \n\t ".to_string());
        let read_error: Body = Err(());
        let unknown: Body = Ok("some rewritten 409 body".to_string());
        let kill_switch_rewritten: Body = Ok(format!("{KILL_SWITCH_409_BODY} (v2)"));
        assert!(kill_switch_body_blocks(&kill_switch));
        assert!(!kill_switch_body_blocks(&already_decided));
        assert!(!kill_switch_body_blocks(&already_decided_padded));
        assert!(kill_switch_body_blocks(&empty));
        assert!(kill_switch_body_blocks(&whitespace));
        assert!(kill_switch_body_blocks(&read_error));
        assert!(kill_switch_body_blocks(&unknown));
        assert!(kill_switch_body_blocks(&kill_switch_rewritten));
    }

    /// I58-B contract pin: the stub serves an independent fixture of the body
    /// `jarvis-policy` writes for an already-decided 409. Only this literal may
    /// take the winner path; a one-sided change to the shared constant fails
    /// here instead of silently turning a kill-switch 409 into a false winner.
    #[test]
    fn already_decided_409_body_pins_winner_path_contract() {
        const POLICY_ALREADY_DECIDED_409_WIRE_BODY: &str = "approval request already decided";
        assert_eq!(
            ALREADY_DECIDED_409_BODY, POLICY_ALREADY_DECIDED_409_WIRE_BODY,
            "shared already-decided body must still match the policy wire contract"
        );
        assert_ne!(
            ALREADY_DECIDED_409_BODY, KILL_SWITCH_409_BODY,
            "the two 409 bodies must stay distinguishable"
        );
    }

    /// I52-A (a): the canonical already-decided literal still takes the legacy
    /// winner-preservation path, so the two fixed bodies are the only split.
    #[test]
    fn already_decided_canonical_409_body_still_fetches_winner() {
        let mut bodies = [
            ("HTTP/1.1 409 Conflict", ALREADY_DECIDED_409_BODY),
            ("HTTP/1.1 200 OK", r#"{"decision":"Pending"}"#),
        ]
        .into_iter();
        let (addr, handle) = spawn_test_server(
            move |stream, _request| {
                let (status, body) = bodies.next().expect("queued response");
                respond_json(stream, status, body);
            },
            2,
        );

        let client = PolicyClient::new(format!("http://{addr}"));
        let outcome = client.deny_outcome("req-1", "because");
        handle.join().expect("server thread");
        assert!(
            matches!(
                outcome.expect("already-decided 409 resolves to the winner"),
                DecisionOutcome::AlreadyDecided(_)
            ),
            "the canonical already-decided body must keep the winner path"
        );
    }

    /// Contract helper: a 409 followed by a winner GET, returning both raw
    /// requests so the header on each hop can be asserted.
    fn capture_winner_readback(client_factory: impl FnOnce(String) -> PolicyClient) -> Vec<String> {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_for_thread = seen.clone();
        let mut bodies = [
            ("HTTP/1.1 409 Conflict", ALREADY_DECIDED_409_BODY),
            ("HTTP/1.1 200 OK", r#"{"decision":"Pending"}"#),
        ]
        .into_iter();
        let (addr, handle) = spawn_test_server(
            move |stream, request| {
                seen_for_thread.lock().expect("lock").push(request);
                let (status, body) = bodies.next().expect("queued response");
                respond_json(stream, status, body);
            },
            2,
        );

        let client = client_factory(format!("http://{addr}"));
        let outcome = client.approve_outcome("req-1", "koppe");
        handle.join().expect("server thread");
        assert!(outcome.is_ok(), "409 must resolve to the winner");
        let requests = seen.lock().expect("lock").clone();
        assert_eq!(requests.len(), 2, "POST then status GET");
        requests
    }

    /// I52-A (b) contract: with both scopes configured the winner readback
    /// keeps sending the API token (no regression for existing deployments).
    #[test]
    fn winner_readback_sends_api_token_when_configured() {
        let requests = capture_winner_readback(|base_url| {
            PolicyClient::new_with_tokens(
                base_url,
                Some("tc-api-token".into()),
                Some("tc-admin-token".into()),
            )
        });
        assert_eq!(
            approval_token(&requests[0]).as_deref(),
            Some("tc-admin-token")
        );
        assert_eq!(
            approval_token(&requests[1]).as_deref(),
            Some("tc-api-token")
        );
    }

    /// I52-A (b) contract: in an admin-only configuration there is no API
    /// token, so the readback must fall back to the admin token instead of
    /// sending no header (which the status route now rejects).
    #[test]
    fn winner_readback_sends_admin_token_in_admin_only_config() {
        let requests = capture_winner_readback(|base_url| {
            PolicyClient::new_with_tokens(base_url, None, Some("tc-admin-token".into()))
        });
        assert_eq!(
            approval_token(&requests[0]).as_deref(),
            Some("tc-admin-token")
        );
        assert_eq!(
            approval_token(&requests[1]).as_deref(),
            Some("tc-admin-token")
        );
    }

    /// I60-B regression: a 410 from approve/deny is a terminal expiry with no
    /// winner. It must not surface as a generic transport error and must never
    /// issue a status GET — the old already-decided 409 path is what rendered
    /// "既に決定済み: 保留中" for an expired request.
    #[test]
    fn expired_410_reports_expired_without_winner_fetch() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_for_thread = seen.clone();
        let (addr, handle) = spawn_test_server(
            move |stream, request| {
                seen_for_thread.lock().expect("lock").push(request);
                respond_json(stream, "HTTP/1.1 410 Gone", "approval request expired");
            },
            2,
        );

        let client = PolicyClient::new(format!("http://{addr}"));
        let approved = client.approve_outcome("req-1", "koppe");
        let denied = client.deny_outcome("req-2", "because");
        handle.join().expect("server thread");

        assert_eq!(
            approved.expect("410 is a classified outcome"),
            DecisionOutcome::Decided(Decision::Expired)
        );
        assert_eq!(
            denied.expect("410 is a classified outcome"),
            DecisionOutcome::Decided(Decision::Expired)
        );
        let requests = seen.lock().expect("lock");
        assert_eq!(requests.len(), 2, "no winner GET may follow a 410");
        assert!(
            requests.iter().all(|r| r.starts_with("POST /v1/approval/")),
            "only decision POSTs may be sent: {requests:?}"
        );
    }
}
