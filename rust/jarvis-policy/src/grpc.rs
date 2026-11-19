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
        let context: serde_json::Value = serde_json::from_str(&req.context)
            .map_err(|e| Status::invalid_argument(format!("invalid context JSON: {}", e)))?;
        // I14-B: normalize at the same policy-side boundary as the HTTP
        // handler. Cedar rejects JSON `null`, so without this the gRPC server
        // would error on contexts the client used to sanitize before sending.
        let context = crate::client::without_nulls(&context);
        let allowed = self
            .engine
            .is_allowed(&req.principal, &req.action, &req.resource, &context)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(CheckResponse { allowed }))
    }

    async fn get_kill_switch_status(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<KillSwitchStatusResponse>, Status> {
        let status = self.engine.kill_switch_status();
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
            .map_err(|e| Status::unauthenticated(e.to_string()))?;
        Ok(Response::new(Empty {}))
    }

    async fn deactivate_kill_switch(
        &self,
        request: Request<DeactivateKillSwitchRequest>,
    ) -> Result<Response<Empty>, Status> {
        let req = request.into_inner();
        self.engine
            .deactivate_kill_switch(&req.admin_token)
            .map_err(|e| Status::unauthenticated(e.to_string()))?;
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
    use std::path::{Path, PathBuf};

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
    /// layer must surface it as `InvalidArgument` (tonic code 3).
    #[tokio::test]
    async fn malformed_context_json_is_invalid_argument() {
        let service = test_service("malformed-context").await;

        let status = service
            .check(check_request("{ not valid json"))
            .await
            .expect_err("malformed JSON context must be rejected");

        assert_eq!(status.code(), tonic::Code::InvalidArgument);
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
}
