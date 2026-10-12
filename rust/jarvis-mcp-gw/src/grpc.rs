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
        let result = self
            .gateway
            .execute_tool(
                &req.tool,
                serde_json::from_str(&req.args).unwrap(),
                &req.principal,
            )
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(ExecuteToolResponse {
            result: serde_json::to_string(&result).unwrap(),
        }))
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
