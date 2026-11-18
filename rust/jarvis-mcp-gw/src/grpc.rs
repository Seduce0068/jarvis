//! gRPC service for MCP Gateway

use crate::McpGateway;
use tonic::{Request, Response, Status};

pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/proto/jarvis.mcp.gateway.v1.rs"));
}

use proto::mcp_gateway_service_server::{McpGatewayService, McpGatewayServiceServer};
use proto::{Empty, ExecuteToolRequest, ExecuteToolResponse, RegisterMcpRequest};

/// Metadata header carrying the shared gRPC token.
const GRPC_TOKEN_HEADER: &str = "x-jarvis-grpc-token";

pub struct GrpcMcpGatewayService {
    gateway: McpGateway,
    admin_token: Option<String>,
}

/// Constant-time-ish comparison, mirroring the HTTP approval-token gate.
fn token_matches(provided: Option<&str>, expected: &str) -> bool {
    provided.is_some_and(|p| {
        p.len() == expected.len()
            && p.bytes()
                .zip(expected.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    })
}

/// Authorize a gRPC call before any handler work runs.
///
/// `admin_only` marks management RPCs (currently `RegisterMcpServer`): they
/// must never run without a configured token. Data-plane RPCs only require
/// the token when one is configured, preserving legacy loopback callers.
fn authorize_grpc<T>(
    request: &Request<T>,
    expected: Option<&str>,
    admin_only: bool,
) -> Result<(), Status> {
    let provided = request
        .metadata()
        .get(GRPC_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok());
    match expected {
        Some(token) if token_matches(provided, token) => Ok(()),
        Some(_) => Err(Status::unauthenticated("invalid or missing gRPC token")),
        None if admin_only => Err(Status::unauthenticated(
            "administrative gRPC RPCs require JARVIS_MCP_GW_ADMIN_TOKEN",
        )),
        None => Ok(()),
    }
}

#[tonic::async_trait]
impl McpGatewayService for GrpcMcpGatewayService {
    async fn execute_tool(
        &self,
        request: Request<ExecuteToolRequest>,
    ) -> Result<Response<ExecuteToolResponse>, Status> {
        authorize_grpc(&request, self.admin_token.as_deref(), false)?;
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
        authorize_grpc(&request, self.admin_token.as_deref(), true)?;
        let req = request.into_inner();
        self.gateway
            .register_mcp_server(&req.name, req.command)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(Empty {}))
    }
}

pub async fn serve(
    gateway: McpGateway,
    addr: std::net::SocketAddr,
    admin_token: Option<String>,
) -> anyhow::Result<()> {
    let service = GrpcMcpGatewayService {
        gateway,
        admin_token,
    };
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
    async fn test_service_with_token(
        label: &str,
        admin_token: Option<String>,
    ) -> GrpcMcpGatewayService {
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
        GrpcMcpGatewayService {
            gateway,
            admin_token,
        }
    }

    async fn test_service(label: &str) -> GrpcMcpGatewayService {
        test_service_with_token(label, None).await
    }

    fn request_with_token<T>(payload: T, token: Option<&str>) -> Request<T> {
        let mut request = Request::new(payload);
        if let Some(token) = token {
            request.metadata_mut().insert(
                GRPC_TOKEN_HEADER,
                token.parse().expect("valid metadata token"),
            );
        }
        request
    }

    #[test]
    fn authorize_grpc_enforces_configured_token() {
        let request = request_with_token((), Some("correct-token"));
        assert!(authorize_grpc(&request, Some("correct-token"), false).is_ok());

        for provided in [None, Some("wrong-token")] {
            let request = request_with_token((), provided);
            let status = authorize_grpc(&request, Some("correct-token"), false)
                .expect_err("invalid or missing token must be rejected");
            assert_eq!(status.code(), Code::Unauthenticated);
        }
    }

    #[test]
    fn authorize_grpc_keeps_admin_rpc_fail_closed_without_token() {
        let request = request_with_token((), None);
        let status = authorize_grpc(&request, None, true)
            .expect_err("admin RPC must fail closed when no token is configured");
        assert_eq!(status.code(), Code::Unauthenticated);

        // Data-plane RPCs stay usable for legacy loopback callers.
        assert!(authorize_grpc(&request, None, false).is_ok());
    }

    #[tokio::test]
    async fn register_mcp_server_requires_admin_token() {
        let service = test_service("admin-no-token").await;
        let request = Request::new(RegisterMcpRequest {
            name: "probe".into(),
            command: vec!["/bin/true".into()],
        });
        let status = service
            .register_mcp_server(request)
            .await
            .expect_err("unauthenticated admin RPC must be rejected");
        assert_eq!(status.code(), Code::Unauthenticated);
    }

    #[tokio::test]
    async fn register_mcp_server_accepts_correct_admin_token() {
        let service =
            test_service_with_token("admin-with-token", Some("admin-secret".into())).await;
        let request = request_with_token(
            RegisterMcpRequest {
                name: "probe".into(),
                // Empty command is rejected by the gateway after authorization;
                // reaching that error proves the auth gate was passed.
                command: Vec::new(),
            },
            Some("admin-secret"),
        );
        let status = service
            .register_mcp_server(request)
            .await
            .expect_err("empty command is still rejected by the gateway");
        assert_eq!(status.code(), Code::Internal);
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
