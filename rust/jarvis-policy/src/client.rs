//! gRPC client for the policy engine.
//!
//! Sidecars authorize through this client instead of building their own
//! [`crate::PolicyEngine`], so kill-switch state and policy decisions have a
//! single source of truth. Transport and protocol errors are fail-closed:
//! callers treat an unreachable policy service as a denial.

use std::time::Duration;

use anyhow::{Context, Result};
use tonic::transport::Channel;

use crate::grpc::proto::CheckRequest;
use crate::grpc::proto::policy_service_client::PolicyServiceClient;
use crate::http::check_reasons;

/// Deadline for a policy check.
///
/// Local gRPC calls complete in microseconds; the bound only matters when the
/// policy service is wedged. A timed-out check is denied.
const CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// Fixed fallback when a server's reason is not a token this client knows
/// (for example, an older server that leaves the additive field empty).
const UNKNOWN_REASON: &str = "unknown";

/// Allow decision plus the policy service's fixed-vocabulary reason.
///
/// This is the client-side counterpart of the HTTP/gRPC check classification:
/// `reason` is always one of the tokens from [`crate::http::check_reasons`] (or
/// `"unknown"` for an unrecognized value). Raw transport errors and caller
/// input never reach this field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckOutcome {
    /// Whether the policy engine explicitly allowed the request.
    pub allowed: bool,
    /// Fixed, non-sensitive classification supplied by the policy service.
    pub reason: &'static str,
}

/// Map an untrusted wire string onto the policy service's closed reason set.
///
/// The server is the authority for the classification; this allowlist makes
/// sure the client never carries an arbitrary (or caller-influenced) string
/// forward. Fail-closed evaluation errors are already labelled by the server
/// with a fixed reason, so no raw error text is ever observed here either.
fn classify_reason(reason: &str) -> &'static str {
    match reason {
        check_reasons::ALLOWED => check_reasons::ALLOWED,
        check_reasons::KILL_SWITCH => check_reasons::KILL_SWITCH,
        check_reasons::RBAC_DENIED => check_reasons::RBAC_DENIED,
        check_reasons::CEDAR_DENIED => check_reasons::CEDAR_DENIED,
        check_reasons::EVALUATION_ERROR => check_reasons::EVALUATION_ERROR,
        _ => UNKNOWN_REASON,
    }
}

/// Client for the `jarvis.policy.v1.PolicyService` endpoint.
#[derive(Clone)]
pub struct PolicyClient {
    channel: Channel,
}

impl PolicyClient {
    /// Build a lazily connected client.
    ///
    /// Construction never blocks and never fails on an unreachable endpoint:
    /// the channel connects on the first RPC. This lets sidecars start before
    /// `jarvis-policy` is ready.
    pub fn new(endpoint: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into();
        let channel = Channel::from_shared(endpoint.clone())
            .with_context(|| format!("invalid policy endpoint: {endpoint}"))?
            .connect_lazy();
        Ok(Self { channel })
    }

    /// Authorization check that fails closed.
    ///
    /// Returns `true` only when the policy engine explicitly allows the
    /// request. An unreachable or failing policy service yields `false` and an
    /// error log.
    pub async fn is_allowed(
        &self,
        principal: &str,
        action: &str,
        resource: &str,
        context: &serde_json::Value,
    ) -> bool {
        match self.check(principal, action, resource, context).await {
            Ok(allowed) => allowed,
            Err(_error) => {
                // The raw anyhow chain can embed the endpoint and caller input;
                // only fixed vocabulary reaches tracing (cf. `grpc::error_kind`).
                tracing::error!(
                    error_kind = "policy_check_failed",
                    "policy check failed; denying (fail-closed)"
                );
                false
            }
        }
    }

    /// Raw check that surfaces transport and protocol errors to the caller.
    pub async fn check(
        &self,
        principal: &str,
        action: &str,
        resource: &str,
        context: &serde_json::Value,
    ) -> Result<bool> {
        Ok(self
            .check_with_reason(principal, action, resource, context)
            .await?
            .allowed)
    }

    /// Raw check that also returns the server's fixed-vocabulary reason.
    ///
    /// This mirrors [`Self::check`] but keeps the additive `reason` from the
    /// proto response instead of discarding it, so callers can choose whether
    /// they need the classification. Fail-closed behavior on transport and
    /// protocol errors is unchanged: the error is still surfaced to the caller.
    pub async fn check_with_reason(
        &self,
        principal: &str,
        action: &str,
        resource: &str,
        context: &serde_json::Value,
    ) -> Result<CheckOutcome> {
        let mut client = PolicyServiceClient::new(self.channel.clone());
        // I14-B: the context is sent verbatim. Null normalization now happens
        // once at the policy-side boundary (`without_nulls`, shared with the
        // gRPC and HTTP handlers), so a caller gets the same decision whether
        // it arrives through this client or posts directly to /v1/check.
        let mut request = tonic::Request::new(CheckRequest {
            principal: principal.to_owned(),
            action: action.to_owned(),
            resource: resource.to_owned(),
            context: serde_json::to_string(context)?,
        });
        request.set_timeout(CHECK_TIMEOUT);
        let response = client
            .check(request)
            .await
            .with_context(|| "policy check RPC failed")?
            .into_inner();
        Ok(CheckOutcome {
            allowed: response.allowed,
            reason: classify_reason(&response.reason),
        })
    }
}

/// Remove `null` object values and array elements because Cedar's JSON
/// context parser rejects `null`.
///
/// I14-B: this is the single policy-side normalization boundary. The gRPC and
/// HTTP `/v1/check` handlers both call it immediately before
/// `PolicyEngine::is_allowed`, and this client no longer strips nulls itself.
/// Keeping one implementation makes every transport evaluate the same Cedar
/// context for the same caller input (the conversion is idempotent).
pub(crate) fn without_nulls(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .filter(|item| !item.is_null())
                .map(without_nulls)
                .collect(),
        ),
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key.clone(), without_nulls(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PolicyEngine;
    use crate::config::PolicyConfig;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jarvis-policy-client-{label}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Start a real policy server on an ephemeral port and return a client
    /// plus the engine handle so tests can drive state changes.
    async fn start_server(label: &str) -> (PolicyClient, Arc<PolicyEngine>, PathBuf) {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../infra/config/policy");
        let dir = temp_dir(label);
        let model = dir.join("casbin_model.conf");
        let policy = dir.join("casbin_policy.csv");
        std::fs::copy(source.join("casbin_model.conf"), &model).expect("copy casbin model");
        std::fs::copy(source.join("casbin_policy.csv"), &policy).expect("copy casbin policy");

        let config = PolicyConfig {
            cedar_policy_dir: dir.join("cedar").to_string_lossy().into_owned(),
            casbin_model_path: model.to_string_lossy().into_owned(),
            casbin_policy_path: policy.to_string_lossy().into_owned(),
            kill_switch_admin_token: "test-admin-token".into(),
            ..Default::default()
        };
        let engine = Arc::new(
            PolicyEngine::new(config)
                .await
                .expect("policy engine starts"),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind policy server");
        let addr = listener.local_addr().expect("policy server address");
        let serving = engine.clone();
        tokio::spawn(async move {
            crate::grpc::serve_with_listener(serving, listener)
                .await
                .expect("policy server runs");
        });

        (
            PolicyClient::new(format!("http://{addr}")).expect("policy client"),
            engine,
            dir,
        )
    }

    #[tokio::test]
    async fn allowed_and_denied_checks_go_through_grpc() {
        let (client, _engine, dir) = start_server("checks").await;
        let context = serde_json::json!({});

        assert!(
            client
                .check("jarvis", "execute", "tool:filesystem.read", &context)
                .await
                .expect("check runs"),
            "the shipped policy grants filesystem.read to jarvis"
        );
        assert!(
            !client
                .check("jarvis", "execute", "tool:git.commit", &context)
                .await
                .expect("check runs"),
            "the shipped policy does not grant git.commit to jarvis"
        );

        std::fs::remove_dir_all(dir).expect("clean temp dir");
    }

    /// Regression: `check_with_reason` must surface both the server's decision
    /// and its fixed-vocabulary reason, while `check` keeps its plain `bool`
    /// API. No raw error or caller input may appear in the reason.
    #[tokio::test]
    async fn check_with_reason_returns_allowed_and_fixed_reason() {
        let (client, _engine, dir) = start_server("reasons").await;
        let context = serde_json::json!({});

        let allowed = client
            .check_with_reason("jarvis", "execute", "tool:filesystem.read", &context)
            .await
            .expect("check with reason runs");
        assert!(allowed.allowed);
        assert_eq!(allowed.reason, crate::http::check_reasons::ALLOWED);

        let denied = client
            .check_with_reason("jarvis", "execute", "tool:no.such.tool", &context)
            .await
            .expect("check with reason runs");
        assert!(!denied.allowed);
        assert_eq!(denied.reason, crate::http::check_reasons::RBAC_DENIED);
        assert!(
            matches!(
                denied.reason,
                crate::http::check_reasons::ALLOWED
                    | crate::http::check_reasons::KILL_SWITCH
                    | crate::http::check_reasons::RBAC_DENIED
                    | crate::http::check_reasons::CEDAR_DENIED
                    | crate::http::check_reasons::EVALUATION_ERROR
            ),
            "reason must stay inside the server's fixed vocabulary"
        );
        assert!(!denied.reason.contains("tool:"));

        // The legacy method still returns a plain bool with the same decision.
        let legacy: bool = client
            .check("jarvis", "execute", "tool:filesystem.read", &context)
            .await
            .expect("check runs");
        assert_eq!(legacy, allowed.allowed);

        std::fs::remove_dir_all(dir).expect("clean temp dir");
    }

    /// The kill switch must reach sidecars through the client, not through
    /// process-local state.
    #[tokio::test]
    async fn kill_switch_activation_is_visible_to_remote_clients() {
        let (client, engine, dir) = start_server("killswitch").await;
        let context = serde_json::json!({});

        assert!(
            client
                .check("jarvis", "execute", "tool:filesystem.read", &context)
                .await
                .expect("baseline check runs")
        );

        engine
            .activate_kill_switch("test activation".into(), "test-admin-token")
            .expect("admin token activates the switch");

        assert!(
            !client
                .check("jarvis", "execute", "tool:filesystem.read", &context)
                .await
                .expect("check while activated runs")
        );

        std::fs::remove_dir_all(dir).expect("clean temp dir");
    }

    #[tokio::test]
    async fn unreachable_policy_fails_closed() {
        // Port 1 is not served; the lazy channel fails on the first RPC.
        let client = PolicyClient::new("http://127.0.0.1:1").expect("policy client");
        assert!(
            !client
                .is_allowed(
                    "jarvis",
                    "execute",
                    "tool:filesystem.read",
                    &serde_json::json!({})
                )
                .await,
            "an unreachable policy service must deny"
        );
    }

    /// The fail-closed error log must expose only fixed vocabulary: no endpoint,
    /// principal/action/resource, or raw error chain may reach tracing. A
    /// function-local mutex serializes the thread-local subscriber so parallel
    /// tests cannot interfere with the captured log line.
    #[test]
    fn unreachable_policy_logs_fixed_vocabulary_only() {
        // Static contract check: the fail-closed log must carry only the fixed
        // error_kind token. A dynamic thread-local capture is deliberately
        // avoided -- tonic/hyper emit their own TRACE records under the same
        // subscriber and made this assertion flaky under parallel execution.
        const SRC: &str = include_str!("client.rs");
        let at = SRC
            .find("policy check failed; denying (fail-closed)")
            .expect("fail-closed log message exists");
        let start = SRC[..at]
            .rfind("tracing::error!(")
            .expect("fail-closed log is an error event");
        let statement = &SRC[start..at];
        assert!(
            statement.contains("error_kind = \"policy_check_failed\""),
            "fail-closed log missing fixed error_kind: {statement}"
        );
        for leak in [
            "endpoint",
            "principal",
            "action",
            "resource",
            "error = ?error",
        ] {
            assert!(
                !statement.contains(leak),
                "fail-closed log must not format {leak}: {statement}"
            );
        }
        // The RPC error context must not embed the endpoint either.
        assert!(
            SRC.contains("with_context(|| \"policy check RPC failed\")"),
            "RPC context must stay fixed-vocabulary"
        );
    }

    #[test]
    fn strips_nulls_from_the_policy_context() {
        let context = serde_json::json!({
            "event": { "id": null, "type": "state_changed" },
            "recent": [{ "expires_at": null, "content": "x" }],
        });
        let sanitized = without_nulls(&context);
        assert!(sanitized["event"].get("id").is_none());
        assert!(sanitized["recent"][0].get("expires_at").is_none());
        assert_eq!(sanitized["event"]["type"], "state_changed");
    }
}
