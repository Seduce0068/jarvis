//! gRPC service for Policy Engine

use crate::PolicyEngine;
use tonic::{Request, Response, Status};

pub mod proto {
    tonic::include_proto!("jarvis.policy.v1");
}

use proto::policy_service_server::{PolicyService, PolicyServiceServer};
use proto::{
    ActivateKillSwitchRequest, CheckRequest, CheckResponse, DeactivateKillSwitchRequest, Empty,
    KillSwitchStatusResponse,
};

/// Fixed status text for the only authentication failure of the kill switch.
/// Selected from the error *variant*, never from its text, so neither a token
/// nor any other caller input can be echoed back.
const INVALID_ADMIN_TOKEN_MESSAGE: &str = "Invalid admin token";

/// Fixed status text for every non-authentication kill-switch failure.
/// `KillSwitchError::Persistence::detail` embeds the state-file path and OS
/// error text; the error is reduced to a fixed `error_kind` before any tracing
/// record is written, so that detail is never logged (R5-B / I46-D).
const KILL_SWITCH_INTERNAL_ERROR_MESSAGE: &str = "internal error";

/// Fixed status message for a malformed `context` JSON string on `Check`.
///
/// `serde_json`'s error text embeds a byte offset and an expected-token
/// description derived from caller input. Echoing it into a gRPC status made
/// this surface asymmetric with the HTTP path's empty bare 400 (I97-D): only
/// this literal is observable by the caller, while the raw error is reduced to
/// a fixed `error_kind` before any tracing record is written.
const INVALID_CONTEXT_JSON_MESSAGE: &str = "invalid context JSON";

/// Fixed-vocabulary categories for kill-switch failures (I46-D).
///
/// `KillSwitchError::Persistence` embeds a state-file path and an OS error in
/// its `detail`, and untyped `anyhow` chains can carry URLs or token fragments.
/// Only the variant token below is ever logged: no part of the error's
/// `Display`/`Debug` text is read here. This mirrors `jarvis_context::grpc`'s
/// `error_kind` so both transports expose the same log vocabulary.
fn error_kind(error: &anyhow::Error) -> &'static str {
    use crate::kill_switch::KillSwitchError;

    match error.downcast_ref::<KillSwitchError>() {
        Some(KillSwitchError::InvalidAdminToken) => "invalid_admin_token",
        Some(KillSwitchError::Persistence { .. }) => "persistence",
        None => "other",
    }
}

/// Map a kill-switch failure to a gRPC status without disclosing its detail.
///
/// The status-code contract is unchanged: `InvalidAdminToken` is the sole
/// authentication failure (`Unauthenticated`), everything else — including a
/// persistence failure after a valid token — is `Internal`. Both the message
/// and the log record use fixed vocabulary only; the raw error chain never
/// reaches the client, OTLP or Grafana (I46-D).
fn kill_switch_status(error: anyhow::Error) -> Status {
    use crate::kill_switch::KillSwitchError;

    if matches!(
        error.downcast_ref::<KillSwitchError>(),
        Some(KillSwitchError::InvalidAdminToken)
    ) {
        return Status::unauthenticated(INVALID_ADMIN_TOKEN_MESSAGE);
    }
    tracing::error!(
        operation = "kill_switch_rpc",
        error_kind = error_kind(&error),
        "kill switch RPC failed with internal error"
    );
    Status::internal(KILL_SWITCH_INTERNAL_ERROR_MESSAGE)
}

/// Map a malformed context JSON error onto the fixed gRPC contract.
///
/// The `serde_json::Error` is deliberately never read: its `Display`/`Debug`
/// text carries caller-derived fragments, a byte offset and an expected-token
/// description. It is absorbed here and only a fixed `error_kind` (shared with
/// `jarvis_core`'s `BareJson` vocabulary) reaches tracing, so neither the
/// status nor OTLP/Grafana can echo the offending input (I97-D).
fn invalid_context_json_status(_error: serde_json::Error) -> Status {
    tracing::debug!(
        error_kind = jarvis_core::INVALID_JSON_BODY_ERROR_KIND,
        field = "context",
        "invalid client input"
    );
    Status::invalid_argument(INVALID_CONTEXT_JSON_MESSAGE)
}

pub struct GrpcPolicyService {
    engine: Arc<PolicyEngine>,
}

#[tonic::async_trait]
impl PolicyService for GrpcPolicyService {
    async fn check(
        &self,
        request: Request<CheckRequest>,
    ) -> Result<Response<CheckResponse>, Status> {
        let req = request.into_inner();
        // A malformed context used to fall back to an empty object via
        // `unwrap_or_default()`, so Cedar evaluated a different request than
        // the caller sent. Reject it at the RPC boundary instead.
        let context: serde_json::Value =
            serde_json::from_str(&req.context).map_err(invalid_context_json_status)?;
        // I14-B: normalize at the same policy-side boundary as the HTTP
        // handler. Cedar rejects JSON `null`, so without this the gRPC server
        // would error on contexts the client used to sanitize before sending.
        let context = crate::client::without_nulls(&context);
        // I21-E: use the same reason-aware evaluation as the HTTP handler so
        // both transports classify a decision identically. Evaluation errors
        // are fail-closed (allowed = false) with a fixed reason instead of an
        // internal status carrying an error string that may leak policy data.
        let outcome = self
            .engine
            .check_with_reason(&req.principal, &req.action, &req.resource, &context)
            .await;
        Ok(Response::new(CheckResponse {
            allowed: outcome.allowed,
            reason: outcome.reason.to_owned(),
        }))
    }

    async fn get_kill_switch_status(
        &self,
        request: Request<Empty>,
    ) -> Result<Response<KillSwitchStatusResponse>, Status> {
        // The protobuf request is `Empty`, so the dedicated admin token rides
        // in metadata (the same key the HTTP endpoint uses). No proto/schema
        // change is required, and the token never appears in a log message.
        let token = request
            .metadata()
            .get(crate::kill_switch::KILL_SWITCH_TOKEN_HEADER)
            .and_then(|value| value.to_str().ok());
        if !token.is_some_and(|token| self.engine.verify_kill_switch_admin_token(token)) {
            return Err(Status::unauthenticated(INVALID_ADMIN_TOKEN_MESSAGE));
        }
        // The status fields are free-form strings; return the same fixed
        // vocabulary the HTTP endpoint exposes, even to an authenticated caller.
        let status = self.engine.kill_switch_status().redacted();
        Ok(Response::new(KillSwitchStatusResponse {
            activated: status.activated,
            activated_at: status.activated_at.unwrap_or(0) as i64,
            reason: status.reason.unwrap_or_default(),
            activated_by: status.activated_by.unwrap_or_default(),
        }))
    }

    async fn activate_kill_switch(
        &self,
        request: Request<ActivateKillSwitchRequest>,
    ) -> Result<Response<Empty>, Status> {
        let req = request.into_inner();
        self.engine
            .activate_kill_switch(req.reason, &req.admin_token)
            .map_err(kill_switch_status)?;
        Ok(Response::new(Empty {}))
    }

    async fn deactivate_kill_switch(
        &self,
        request: Request<DeactivateKillSwitchRequest>,
    ) -> Result<Response<Empty>, Status> {
        let req = request.into_inner();
        self.engine
            .deactivate_kill_switch(&req.admin_token)
            .map_err(kill_switch_status)?;
        Ok(Response::new(Empty {}))
    }
}

use std::sync::Arc;

pub async fn serve(engine: Arc<PolicyEngine>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let service = GrpcPolicyService { engine };
    tonic::transport::Server::builder()
        .add_service(PolicyServiceServer::new(service))
        .serve(addr)
        .await?;
    Ok(())
}

/// Serve on an already-bound listener.
///
/// Tests use this to bind an ephemeral port and learn its address before the
/// server starts.
pub async fn serve_with_listener(
    engine: Arc<PolicyEngine>,
    listener: tokio::net::TcpListener,
) -> anyhow::Result<()> {
    let incoming = futures_util::stream::unfold(listener, |listener| async move {
        match listener.accept().await {
            Ok((stream, _)) => Some((Ok::<_, std::io::Error>(stream), listener)),
            Err(error) => Some((Err(error), listener)),
        }
    });

    let service = GrpcPolicyService { engine };
    tonic::transport::Server::builder()
        .add_service(PolicyServiceServer::new(service))
        .serve_with_incoming(incoming)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PolicyEngine;
    use crate::config::PolicyConfig;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// In-memory `MakeWriter` target: no new dependency and no global
    /// subscriber, so other tests can still install their own (I46-D).
    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for LogBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut guard = self.0.lock().expect("log buffer mutex");
            guard.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jarvis-policy-grpc-{label}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Build a service backed by a real engine using the shipped Casbin files,
    /// mirroring the setup in `client::tests::start_server`.
    async fn test_service(label: &str) -> GrpcPolicyService {
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
        GrpcPolicyService { engine }
    }

    fn check_request(context: &str) -> Request<CheckRequest> {
        check_request_for("tool:filesystem.read", context)
    }

    fn check_request_for(resource: &str, context: &str) -> Request<CheckRequest> {
        Request::new(CheckRequest {
            principal: "jarvis".into(),
            action: "execute".into(),
            resource: resource.into(),
            context: context.into(),
        })
    }

    /// Engine whose allow decision can only come from Cedar: the Casbin policy
    /// is left empty (the engine creates it at startup), so the context really
    /// reaches `Context::from_json_value` instead of the RBAC fast path.
    async fn cedar_only_service(label: &str) -> GrpcPolicyService {
        let dir = std::env::temp_dir().join(format!("jarvis-policy-grpc-cedar-{label}"));
        let _ = std::fs::remove_dir_all(&dir);
        let cedar = dir.join("cedar");
        std::fs::create_dir_all(&cedar).expect("create cedar dir");
        std::fs::write(
            cedar.join("allow.cedar"),
            r#"permit (
    principal == jarvis::Principal::"jarvis",
    action == jarvis::Action::"execute",
    resource == jarvis::Resource::"tool:cedar.only"
);
"#,
        )
        .expect("write cedar policy");

        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../infra/config/policy");
        let model = dir.join("casbin_model.conf");
        std::fs::copy(source.join("casbin_model.conf"), &model).expect("copy casbin model");
        let config = PolicyConfig {
            cedar_policy_dir: cedar.to_string_lossy().into_owned(),
            casbin_model_path: model.to_string_lossy().into_owned(),
            casbin_policy_path: dir.join("casbin_policy.csv").to_string_lossy().into_owned(),
            kill_switch_admin_token: "test-admin-token".into(),
            ..Default::default()
        };
        let engine = Arc::new(
            PolicyEngine::new(config)
                .await
                .expect("policy engine starts"),
        );
        GrpcPolicyService { engine }
    }

    /// Regression: unparseable context must not silently become `{}`; the gRPC
    /// layer must surface it as `InvalidArgument` (tonic code 3). I97-D: the
    /// message is the one fixed phrase — serde's caller-derived detail must not
    /// be echoed, matching HTTP's empty bare-400 contract.
    #[tokio::test]
    async fn malformed_context_json_is_invalid_argument() {
        let service = test_service("malformed-context").await;

        const MALFORMED: &str = "{ not valid json";
        let raw_detail = serde_json::from_str::<serde_json::Value>(MALFORMED)
            .expect_err("probe is malformed")
            .to_string();
        assert!(!raw_detail.is_empty());

        let status = service
            .check(check_request(MALFORMED))
            .await
            .expect_err("malformed JSON context must be rejected");

        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert_eq!(status.message(), INVALID_CONTEXT_JSON_MESSAGE);
        assert!(
            !status.message().contains(raw_detail.as_str()),
            "serde detail leaked into the status: {}",
            status.message()
        );
    }

    /// I97-D: the fixed status message and the log error_kind are contract
    /// constants; the latter matches the shared `BareJson` invalid-input token.
    #[test]
    fn invalid_context_json_contract_constants_are_fixed() {
        assert_eq!(INVALID_CONTEXT_JSON_MESSAGE, "invalid context JSON");
        assert_eq!(jarvis_core::INVALID_JSON_BODY_ERROR_KIND, "invalid_input");
    }

    /// I97-D: the raw serde error — derived from caller input — must be
    /// absorbed before tracing. Only the fixed error_kind and message are
    /// logged, so no offending fragment or serde detail reaches OTLP/Grafana.
    #[test]
    fn malformed_context_json_never_logs_raw_error_detail() {
        const PROBE: &str = "PROBE_MALFORMED_CONTEXT_INPUT";
        let malformed = format!("{{ {PROBE}}}");
        let raw_detail = serde_json::from_str::<serde_json::Value>(&malformed)
            .expect_err("probe is malformed")
            .to_string();

        let buffer = LogBuffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_target(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || writer.clone())
            .finish();

        let status = tracing::subscriber::with_default(subscriber, || {
            let error = serde_json::from_str::<serde_json::Value>(&malformed)
                .expect_err("probe is malformed");
            invalid_context_json_status(error)
        });

        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert_eq!(status.message(), INVALID_CONTEXT_JSON_MESSAGE);

        let captured = buffer.0.lock().expect("log buffer mutex").clone();
        let logs = String::from_utf8(captured).expect("logs are utf-8");
        assert!(
            logs.contains("invalid client input"),
            "fixed log is missing"
        );
        assert!(!logs.contains(PROBE), "raw input leaked to logs: {logs}");
        assert!(
            !logs.contains(raw_detail.as_str()),
            "raw serde detail leaked: {logs}"
        );
    }

    /// Guard the normal path: well-formed JSON still reaches evaluation.
    #[tokio::test]
    async fn valid_context_json_is_accepted() {
        let service = test_service("valid-context").await;

        service
            .check(check_request(r#"{"event":{"type":"state_changed"}}"#))
            .await
            .expect("well-formed JSON context is accepted");
    }

    /// I14-B: a context containing `null` must produce the same decision over
    /// gRPC as the equivalent sanitized context. Cedar rejects JSON nulls, so
    /// without the shared normalization this request returned an error.
    #[tokio::test]
    async fn null_context_matches_sanitized_context_over_grpc() {
        let service = cedar_only_service("null-context-grpc").await;
        let sanitized = r#"{
            "event": {"type": "state_changed"},
            "recent": [{"content": "x"}]
        }"#;
        let with_nulls = r#"{
            "event": {"id": null, "type": "state_changed"},
            "recent": [null, {"content": "x"}]
        }"#;

        let clean = service
            .check(check_request_for("tool:cedar.only", sanitized))
            .await
            .expect("sanitized context is accepted")
            .into_inner()
            .allowed;
        let normalized = service
            .check(check_request_for("tool:cedar.only", with_nulls))
            .await
            .expect("null-bearing context is normalized before evaluation")
            .into_inner()
            .allowed;

        assert!(clean, "the Cedar-only permit must allow this request");
        assert_eq!(
            normalized, clean,
            "nulls in the context must not change the gRPC decision"
        );
    }

    /// I21-E: the additive `reason` field uses the same closed vocabulary as
    /// HTTP and never carries request data or internal error strings.
    #[tokio::test]
    async fn check_response_carries_fixed_reasons() {
        let service = test_service("grpc-reasons").await;

        let allowed = service
            .check(check_request_for("tool:filesystem.read", "{}"))
            .await
            .expect("check runs")
            .into_inner();
        assert!(allowed.allowed);
        assert_eq!(allowed.reason, "allowed");

        let rbac_denied = service
            .check(check_request_for("tool:no.such.tool", "{}"))
            .await
            .expect("check runs")
            .into_inner();
        assert!(!rbac_denied.allowed);
        assert_eq!(rbac_denied.reason, "rbac_denied");

        // A quoted principal cannot be parsed as a Cedar entity UID. The RPC
        // still succeeds so the caller can see the fixed `evaluation_error`
        // reason instead of an internal status carrying the parse error.
        let failed = service
            .check(Request::new(CheckRequest {
                principal: "bad\"principal".into(),
                action: "execute".into(),
                resource: "tool:filesystem.read".into(),
                context: "{}".into(),
            }))
            .await
            .expect("evaluation failure is returned in the response")
            .into_inner();
        assert!(!failed.allowed);
        assert_eq!(failed.reason, "evaluation_error");

        for reason in [&allowed.reason, &rbac_denied.reason, &failed.reason] {
            assert!(matches!(
                reason.as_str(),
                "allowed" | "kill_switch" | "rbac_denied" | "cedar_denied" | "evaluation_error"
            ));
            assert!(!reason.contains("tool:"));
        }

        service
            .engine
            .activate_kill_switch(
                "SENTINEL-KILL-REASON-DO-NOT-LEAK".into(),
                "test-admin-token",
            )
            .expect("activate kill switch");
        let killed = service
            .check(check_request_for("tool:filesystem.read", "{}"))
            .await
            .expect("check runs")
            .into_inner();
        assert!(!killed.allowed);
        assert_eq!(killed.reason, "kill_switch");
        assert!(!killed.reason.contains("SENTINEL-KILL-REASON-DO-NOT-LEAK"));
    }

    /// I21-E: a Cedar policy set that actively denies is reported as such.
    #[tokio::test]
    async fn check_response_reports_cedar_denied() {
        let service = cedar_only_service("grpc-cedar-denied").await;
        let response = service
            .check(check_request_for("tool:not.permitted", "{}"))
            .await
            .expect("check runs")
            .into_inner();
        assert!(!response.allowed);
        assert_eq!(response.reason, "cedar_denied");
    }

    /// I23-E: GetKillSwitchStatus must reject callers without the dedicated
    /// admin token and must return only the fixed status vocabulary on the
    /// success path.
    #[tokio::test]
    async fn get_kill_switch_status_requires_token_and_redacts_details() {
        let service = test_service("grpc-kill-status").await;
        let sentinel = "SENTINEL-GRPC-KILL-STATUS-DO-NOT-LEAK";
        service
            .engine
            .activate_kill_switch(sentinel.to_string(), "test-admin-token")
            .expect("activate kill switch");

        let missing = service
            .get_kill_switch_status(Request::new(Empty {}))
            .await
            .expect_err("status requires the admin token");
        assert_eq!(missing.code(), tonic::Code::Unauthenticated);

        let mut wrong = Request::new(Empty {});
        wrong.metadata_mut().insert(
            crate::kill_switch::KILL_SWITCH_TOKEN_HEADER,
            tonic::metadata::MetadataValue::from_static("wrong-token"),
        );
        let wrong = service
            .get_kill_switch_status(wrong)
            .await
            .expect_err("wrong token is rejected");
        assert_eq!(wrong.code(), tonic::Code::Unauthenticated);

        let mut authorized = Request::new(Empty {});
        authorized.metadata_mut().insert(
            crate::kill_switch::KILL_SWITCH_TOKEN_HEADER,
            tonic::metadata::MetadataValue::from_static("test-admin-token"),
        );
        let status = service
            .get_kill_switch_status(authorized)
            .await
            .expect("admin token unlocks status")
            .into_inner();
        assert!(status.activated);
        assert_ne!(status.reason, sentinel);
        assert_eq!(
            status.reason,
            crate::kill_switch::status_vocabulary::REASON_OPERATOR_DEFINED
        );
        assert_eq!(
            status.activated_by,
            crate::kill_switch::status_vocabulary::ACTOR_SYSTEM
        );
    }

    /// The typed auth-vs-internal split must stay visible on the wire: a bad
    /// token is `Unauthenticated`, never `Internal`. R5-B: the message is the
    /// fixed phrase, so the presented token cannot be echoed back either.
    #[tokio::test]
    async fn kill_switch_activation_keeps_auth_error_mapping() {
        let service = test_service("grpc-kill-errors").await;
        let bad = service
            .activate_kill_switch(Request::new(ActivateKillSwitchRequest {
                reason: "test".into(),
                admin_token: "wrong-token".into(),
            }))
            .await
            .expect_err("bad admin token is rejected");
        assert_eq!(bad.code(), tonic::Code::Unauthenticated);
        assert_eq!(bad.message(), INVALID_ADMIN_TOKEN_MESSAGE);
        assert!(!bad.message().contains("wrong-token"));
    }

    /// R5-B: the deactivate RPC shares the same fixed auth mapping.
    #[tokio::test]
    async fn kill_switch_deactivation_keeps_auth_error_mapping() {
        let service = test_service("grpc-kill-deauth-errors").await;
        let bad = service
            .deactivate_kill_switch(Request::new(DeactivateKillSwitchRequest {
                admin_token: "wrong-token".into(),
            }))
            .await
            .expect_err("bad admin token is rejected");
        assert_eq!(bad.code(), tonic::Code::Unauthenticated);
        assert_eq!(bad.message(), INVALID_ADMIN_TOKEN_MESSAGE);
        assert!(!bad.message().contains("wrong-token"));
    }

    /// R5-B regression: error detail (state-file path and OS error) and any
    /// free-form text must never be echoed into a gRPC status message. The
    /// status codes and the two fixed phrases are the whole contract.
    #[test]
    fn kill_switch_status_messages_do_not_leak_detail() {
        use crate::kill_switch::KillSwitchError;

        const PROBE_STATE_PATH: &str = "/var/lib/jarvis/PROBE-kill-switch-state.json";
        const PROBE_OS_ERROR: &str = "permission denied (os error 13)";

        let auth = kill_switch_status(KillSwitchError::InvalidAdminToken.into());
        assert_eq!(auth.code(), tonic::Code::Unauthenticated);
        assert_eq!(auth.message(), INVALID_ADMIN_TOKEN_MESSAGE);

        let detail = format!("write {PROBE_STATE_PATH}: {PROBE_OS_ERROR}");
        let err: anyhow::Error = KillSwitchError::Persistence { detail }.into();
        let persistence = kill_switch_status(err);
        assert_eq!(persistence.code(), tonic::Code::Internal);
        assert_eq!(persistence.message(), KILL_SWITCH_INTERNAL_ERROR_MESSAGE);

        // An untyped anyhow failure (e.g. an upstream context chain) falls
        // back to the same fixed internal phrase.
        let untyped = kill_switch_status(anyhow::anyhow!(
            "PROBE upstream detail at {PROBE_STATE_PATH}"
        ));
        assert_eq!(untyped.code(), tonic::Code::Internal);
        assert_eq!(untyped.message(), KILL_SWITCH_INTERNAL_ERROR_MESSAGE);

        for status in [&auth, &persistence, &untyped] {
            for probe in [PROBE_STATE_PATH, PROBE_OS_ERROR] {
                assert!(
                    !status.message().contains(probe),
                    "status leaked {probe}: {}",
                    status.message()
                );
            }
        }
    }

    /// I46-D: the fixed log vocabulary must come from the error *variant*,
    /// never from its text; untyped failures collapse to `other`, matching the
    /// context gRPC mapping.
    #[test]
    fn kill_switch_error_kind_uses_variants_only() {
        use crate::kill_switch::KillSwitchError;

        let detail = "write /var/lib/jarvis/PROBE-state.json: PROBE os error".to_owned();
        let persistence: anyhow::Error = KillSwitchError::Persistence { detail }.into();
        assert_eq!(error_kind(&persistence), "persistence");

        let auth: anyhow::Error = KillSwitchError::InvalidAdminToken.into();
        assert_eq!(error_kind(&auth), "invalid_admin_token");

        let untyped = anyhow::anyhow!("PROBE upstream detail");
        assert_eq!(error_kind(&untyped), "other");
    }

    /// I46-D regression: `Persistence::detail` (state-file path + OS error) and
    /// untyped anyhow chains (URLs / token fragments) must not reach the log
    /// stream either. Only the fixed `error_kind` and `operation` fields are
    /// emitted, so nothing sensitive is forwarded to OTLP/Grafana.
    #[test]
    fn kill_switch_status_never_logs_raw_error_detail() {
        use crate::kill_switch::KillSwitchError;

        const PROBE_STATE_PATH: &str = "/var/lib/jarvis/PROBE-kill-switch-state.json";
        const PROBE_OS_ERROR: &str = "Permission denied (os error 13)";
        const PROBE_URL: &str = "https://PROBE.internal/token/fragment";

        let buffer = LogBuffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_target(false)
            .with_writer(move || writer.clone())
            .finish();

        let statuses = tracing::subscriber::with_default(subscriber, || {
            let detail = format!("write {PROBE_STATE_PATH}: {PROBE_OS_ERROR}");
            let persistence = kill_switch_status(KillSwitchError::Persistence { detail }.into());
            let untyped = kill_switch_status(anyhow::anyhow!("upstream {PROBE_URL}"));
            [persistence, untyped]
        });

        for status in &statuses {
            assert_eq!(status.code(), tonic::Code::Internal);
            assert_eq!(status.message(), KILL_SWITCH_INTERNAL_ERROR_MESSAGE);
        }

        let captured = buffer.0.lock().expect("log buffer mutex").clone();
        let logs = String::from_utf8(captured).expect("logs are utf-8");
        for probe in [PROBE_STATE_PATH, PROBE_OS_ERROR, PROBE_URL] {
            assert!(
                !logs.contains(probe),
                "raw error detail leaked to logs: {probe}"
            );
        }
        assert!(logs.contains("kill switch RPC failed with internal error"));
        // Do not pin the textual rendering of structured fields: it depends on
        // the tracing-subscriber formatter version. The mapping itself is
        // asserted in `kill_switch_error_kind_uses_variants_only`.
    }
}
