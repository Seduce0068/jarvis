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
        Request::new(CheckRequest {
            principal: "jarvis".into(),
            action: "execute".into(),
            resource: "tool:filesystem.read".into(),
            context: context.into(),
        })
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
}
