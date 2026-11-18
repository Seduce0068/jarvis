//! gRPC service for MCP Gateway

use crate::McpGateway;
use tonic::{Request, Response, Status};

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/proto/jarvis.mcp.gateway.v1.rs"));
}

use proto::mcp_gateway_service_server::{McpGatewayService, McpGatewayServiceServer};
use proto::{Empty, ExecuteToolRequest, ExecuteToolResponse, RegisterMcpRequest};

pub struct GrpcMcpGatewayService {
    gateway: McpGateway,
}

#[tonic::async_trait]
impl McpGatewayService for GrpcMcpGatewayService {
    async fn execute_tool(
        &self,
        request: Request<ExecuteToolRequest>,
    ) -> Result<Response<ExecuteToolResponse>, Status> {
        let req = request.into_inner();
        let args: serde_json::Value = serde_json::from_str(&req.args)
            .map_err(|e| Status::invalid_argument(format!("invalid args JSON: {e}")))?;
        let result = self
            .gateway
            .execute_tool(&req.tool, args, &req.principal)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        let result = serde_json::to_string(&result)
            .map_err(|e| Status::internal(format!("failed to serialize result: {e}")))?;
        Ok(Response::new(ExecuteToolResponse { result }))
    }

    async fn register_mcp_server(
        &self,
        request: Request<RegisterMcpRequest>,
    ) -> Result<Response<Empty>, Status> {
        let req = request.into_inner();
        self.gateway
            .register_mcp_server(&req.name, req.command)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(Empty {}))
    }
}

pub async fn serve(gateway: McpGateway, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let service = GrpcMcpGatewayService { gateway };
    tonic::transport::Server::builder()
        .add_service(McpGatewayServiceServer::new(service))
        .serve(addr)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tonic::Code;

    /// Build a service backed by a real `McpGateway`. The gateway itself is
    /// never reached by the invalid-JSON test (deserialization fails first),
    /// but the handler requires a fully constructed instance.
    async fn test_service(label: &str) -> GrpcMcpGatewayService {
        let dir = std::env::temp_dir().join(format!("jarvis-mcp-gw-grpc-{label}"));
        let _ = std::fs::remove_dir_all(&dir);
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("create temp dir");
        let allowlist = dir.join("mcp_allowlist.toml");
        tokio::fs::write(
            &allowlist,
            r#"
principals = ["test"]

[[tools]]
name = "echo"
description = "Echo tool"
allowed_principals = ["test"]
max_args_size = 1048576
timeout_ms = 5000
requires_approval = false
"#,
        )
        .await
        .expect("write allowlist");
        let config = crate::config::McpGatewayConfig {
            allowlist_path: allowlist.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let policy = crate::testing::shipped_policy_client(label).await;
        let gateway = crate::McpGateway::new(config, Arc::new(policy))
            .await
            .expect("gateway starts");
        GrpcMcpGatewayService { gateway }
    }

    /// Malformed `args` must be reported as `INVALID_ARGUMENT` instead of
    /// panicking (which, under `panic = "abort"`, would kill the whole gateway).
    #[tokio::test]
    async fn invalid_args_json_returns_invalid_argument_without_panicking() {
        let service = test_service("invalid-args").await;
        let request = Request::new(ExecuteToolRequest {
            tool: "echo".into(),
            args: "{ this is not valid json".into(),
            principal: "test".into(),
        });

        let status = service
            .execute_tool(request)
            .await
            .expect_err("malformed args JSON must be rejected");
        assert_eq!(status.code(), Code::InvalidArgument);
    }
}
