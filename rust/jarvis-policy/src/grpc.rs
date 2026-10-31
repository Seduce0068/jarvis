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
        let context: serde_json::Value = serde_json::from_str(&req.context).unwrap_or_default();
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
