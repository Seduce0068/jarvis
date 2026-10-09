//! gRPC service for Policy Engine

use tonic::{Request, Response, Status};
use crate::PolicyEngine;
use crate::policy::{ApprovalRequest, ApprovalDecision};

pub mod proto {
    tonic::include_proto!("jarvis.policy.v1");
}

use proto::policy_service_server::{PolicyService, PolicyServiceServer};
use proto::{CheckRequest, CheckResponse, ApprovalRequest as ProtoApprovalRequest, ApprovalResponse, KillSwitchStatusResponse, ActivateKillSwitchRequest, DeactivateKillSwitchRequest, Empty};

pub struct GrpcPolicyService {
    engine: Arc<PolicyEngine>,
}

#[tonic::async_trait]
impl PolicyService for GrpcPolicyService {
    async fn check(&self, request: Request<CheckRequest>) -> Result<Response<CheckResponse>, Status> {
        let req = request.into_inner();
        let context: serde_json::Value = serde_json::from_str(&req.context).unwrap_or_default();
        let allowed = self.engine.is_allowed(&req.principal, &req.action, &req.resource, &context).await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(CheckResponse { allowed }))
    }

    async fn request_approval(&self, request: Request<ProtoApprovalRequest>) -> Result<Response<ApprovalResponse>, Status> {
        let req = request.into_inner();
        let context: serde_json::Value = serde_json::from_str(&req.context).unwrap_or_default();
        let approval_req = ApprovalRequest {
            id: uuid::Uuid::parse_str(&req.id).map_err(|e| Status::invalid_argument(e.to_string()))?,
            principal: req.principal,
            action: req.action,
            resource: req.resource,
            context,
            requested_at: chrono::DateTime::from_timestamp(req.requested_at, 0).unwrap_or_default(),
            expires_at: chrono::DateTime::from_timestamp(req.expires_at, 0).unwrap_or_default(),
        };
        let decision = self.engine.request_approval(approval_req).await
            .map_err(|e| Status::internal(e.to_string()))?;
        let decision_str = match decision {
            ApprovalDecision::Approved { .. } => "approved".to_string(),
            ApprovalDecision::Denied { reason } => format!("denied: {}", reason),
            ApprovalDecision::Pending => "pending".to_string(),
            ApprovalDecision::Expired => "expired".to_string(),
        };
        Ok(Response::new(ApprovalResponse { decision: decision_str }))
    }

    async fn get_kill_switch_status(&self, _request: Request<Empty>) -> Result<Response<KillSwitchStatusResponse>, Status> {
        let status = self.engine.kill_switch_status();
        Ok(Response::new(KillSwitchStatusResponse {
            activated: status.activated,
            activated_at: status.activated_at.unwrap_or(0) as i64,
            reason: status.reason.unwrap_or_default(),
            activated_by: status.activated_by.unwrap_or_default(),
        }))
    }

    async fn activate_kill_switch(&self, request: Request<ActivateKillSwitchRequest>) -> Result<Response<Empty>, Status> {
        let req = request.into_inner();
        self.engine.activate_kill_switch(req.reason);
        Ok(Response::new(Empty {}))
    }

    async fn deactivate_kill_switch(&self, request: Request<DeactivateKillSwitchRequest>) -> Result<Response<Empty>, Status> {
        let req = request.into_inner();
        self.engine.deactivate_kill_switch(&req.admin_token)
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