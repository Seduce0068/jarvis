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

/// Fixed status message returned to callers for gateway-side failures.
///
/// Internal causes (paths, tokens, upstream text) are written to the server
/// log only; a gRPC status is observable by any caller and must not carry
/// them. Mirrors the transport-neutral classification used by HTTP.
const INTERNAL_ERROR_MESSAGE: &str = "internal gateway error";

/// Log an internal failure server-side and return a sanitized `Status`.
fn sanitized_internal_status(detail: impl std::fmt::Display) -> Status {
    tracing::error!(error = %detail, "gRPC request failed with internal gateway error");
    Status::internal(INTERNAL_ERROR_MESSAGE)
}

/// Map a gateway error to a gRPC status by its concrete type.
///
/// This mirrors `http::http_status_for`: the status is derived from
/// `McpGatewayError::kind()`, never from the human-readable message. Unknown
/// error types fall back to `Internal` and are sanitized like any other
/// gateway-side failure.
fn grpc_status_for(error: &anyhow::Error) -> Status {
    use crate::error::ErrorKind;

    let kind = error
        .downcast_ref::<crate::error::McpGatewayError>()
        .map(crate::error::McpGatewayError::kind)
        .unwrap_or(ErrorKind::Internal);
    match kind {
        ErrorKind::ClientError => Status::invalid_argument(error.to_string()),
        ErrorKind::Forbidden => Status::permission_denied(error.to_string()),
        ErrorKind::Internal => sanitized_internal_status(error),
    }
}

pub struct GrpcMcpGatewayService {
    gateway: McpGateway,
    admin_token: Option<String>,
}

/// Constant-time token comparison that folds a length difference into the
/// accumulator instead of short-circuiting on it.
///
/// Mirrors `jarvis_policy::kill_switch::constant_time_eq`: the loop covers
/// `max(a.len(), b.len())` bytes and pads the shorter input with zero bytes,
/// so neither a content mismatch nor a length mismatch exits early. Seeding
/// `diff` with `a.len() ^ b.len()` makes a length difference contribute even
/// though the XOR loop only runs `max` iterations. No `unsafe` code and no
/// extra dependency are involved.
///
/// A missing presented token is rejected without comparison. The unconfigured
/// token and fail-closed decisions live in [`authorize_grpc`] and are
/// unchanged; neither token value is ever logged.
fn token_matches(provided: Option<&str>, expected: &str) -> bool {
    provided.is_some_and(|p| {
        let a = p.as_bytes();
        let b = expected.as_bytes();
        let max = a.len().max(b.len());
        let mut diff = a.len() ^ b.len();
        for (x, y) in a
            .iter()
            .map(Some)
            .chain(std::iter::repeat(None))
            .zip(b.iter().map(Some).chain(std::iter::repeat(None)))
            .take(max)
        {
            diff |= (x.copied().unwrap_or(0) ^ y.copied().unwrap_or(0)) as usize;
        }
        diff == 0
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
            .map_err(|e| grpc_status_for(&e))?;
        let result = serde_json::to_string(&result).map_err(sanitized_internal_status)?;
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
            .map_err(|e| grpc_status_for(&e))?;
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

    /// `token_matches` must fold a length difference into the accumulator
    /// instead of short-circuiting, matching `kill_switch::constant_time_eq`.
    /// A shorter, a longer, and a same-length wrong token must all be
    /// rejected, while exact matches (including empty/empty) still pass.
    #[test]
    fn token_matches_rejects_length_and_content_mismatches() {
        let expected = "correct-token";

        assert!(token_matches(Some(expected), expected));
        assert!(token_matches(Some(""), ""));

        // Same-length content mismatch.
        assert!(!token_matches(Some("Correct-token"), expected));

        // Length mismatches in both directions; these were rejected before as
        // well, but via a timing-observable `p.len() == expected.len()` exit.
        assert!(!token_matches(Some("correct"), expected));
        assert!(!token_matches(Some("correct-token-extra"), expected));

        // A prefix-only match must not be accepted by the padded XOR loop.
        assert!(!token_matches(Some("correct-to"), expected));

        // Missing token stays rejected (unchanged fail-closed contract).
        assert!(!token_matches(None, expected));
    }

    /// The call site rejects a length-mismatched header token with the same
    /// status as a same-length wrong one, and never echoes token values.
    #[test]
    fn authorize_grpc_rejects_length_mismatched_header_token() {
        for provided in [
            Some("correct"),
            Some("correct-token-extra"),
            Some("Correct-token"),
        ] {
            let request = request_with_token((), provided);
            let status = authorize_grpc(&request, Some("correct-token"), false)
                .expect_err("length/content mismatch must be rejected");
            assert_eq!(status.code(), Code::Unauthenticated);
            assert!(!status.message().contains("correct-token"));
            assert!(!status.message().contains(provided.unwrap_or("")));
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
        // An empty command is a typed `ClientError` (gateway.rs `client_error`),
        // so the shared contract maps it to InvalidArgument, not Internal.
        assert_eq!(status.code(), Code::InvalidArgument);
        assert!(status.message().contains("must not be empty"));
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

    /// Every `ErrorKind` must map to its agreed gRPC code, and internal
    /// failures must never echo their detail into the caller-visible status.
    #[test]
    fn grpc_status_for_maps_error_kinds_and_hides_internal_detail() {
        use crate::error::{ErrorKind, McpGatewayError};

        let cases: [(anyhow::Error, ErrorKind, Code); 9] = [
            (
                McpGatewayError::ArgsTooLarge {
                    tool: "echo".into(),
                    max: 4,
                    actual: 8,
                }
                .into(),
                ErrorKind::ClientError,
                Code::InvalidArgument,
            ),
            (
                McpGatewayError::ClientError("missing url".into()).into(),
                ErrorKind::ClientError,
                Code::InvalidArgument,
            ),
            (
                McpGatewayError::NotAllowed("echo".into()).into(),
                ErrorKind::Forbidden,
                Code::PermissionDenied,
            ),
            (
                McpGatewayError::PolicyDenied {
                    tool: "echo".into(),
                    principal: "test".into(),
                }
                .into(),
                ErrorKind::Forbidden,
                Code::PermissionDenied,
            ),
            (
                McpGatewayError::PrincipalNotAllowed {
                    principal: "test".into(),
                    tool: "echo".into(),
                }
                .into(),
                ErrorKind::Forbidden,
                Code::PermissionDenied,
            ),
            (
                McpGatewayError::ApprovalRequired {
                    tool: "echo".into(),
                }
                .into(),
                ErrorKind::Forbidden,
                Code::PermissionDenied,
            ),
            (
                McpGatewayError::ApprovalDenied {
                    tool: "echo".into(),
                    reason: "no".into(),
                }
                .into(),
                ErrorKind::Forbidden,
                Code::PermissionDenied,
            ),
            (
                McpGatewayError::Sandbox("secret path /tmp/x".into()).into(),
                ErrorKind::Internal,
                Code::Internal,
            ),
            // Untyped errors (including bare `anyhow!` values) fall back to
            // Internal, exactly as they do on the HTTP adapter.
            (
                anyhow::anyhow!("untyped secret detail"),
                ErrorKind::Internal,
                Code::Internal,
            ),
        ];

        for (error, expected_kind, expected_code) in cases {
            let actual_kind = error
                .downcast_ref::<McpGatewayError>()
                .map(McpGatewayError::kind)
                .unwrap_or(ErrorKind::Internal);
            assert_eq!(actual_kind, expected_kind);

            let status = grpc_status_for(&error);
            assert_eq!(status.code(), expected_code, "{error}");
            if expected_code == Code::Internal {
                assert_eq!(status.message(), INTERNAL_ERROR_MESSAGE);
                assert!(!status.message().contains("secret"));
            } else {
                assert!(status.message().contains(error.to_string().as_str()));
            }
        }
    }

    /// HTTP/gRPC joint status-contract regression (I25-B): the same typed
    /// gateway failure must surface as the agreed HTTP status and gRPC code
    /// on both adapters, and internal detail must stay out of gRPC statuses.
    #[tokio::test]
    async fn http_and_grpc_status_contracts_agree() {
        let dir = std::env::temp_dir().join("jarvis-mcp-gw-grpc-joint-status");
        let _ = std::fs::remove_dir_all(&dir);
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("create temp dir");
        let allowlist = dir.join("mcp_allowlist.toml");
        tokio::fs::write(
            &allowlist,
            r#"
principals = ["opencode"]

[[tools]]
name = "shell.execute"
description = "Echo tool"
allowed_principals = ["opencode"]
max_args_size = 8
timeout_ms = 5000
requires_approval = false

[[tools]]
name = "filesystem.read"
description = "Read tool"
allowed_principals = ["opencode"]
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
        let policy = crate::testing::shipped_policy_client("joint-status").await;
        let gateway = crate::McpGateway::new(config, Arc::new(policy))
            .await
            .expect("gateway starts");

        // A mock upstream returning an MCP error result: `gateway.execute`
        // turns that into an untyped internal failure (anyhow::bail!).
        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(crate::testing::run_mock_mcp_server(
            server_side,
            "filesystem.read",
            true,
        ));
        gateway
            .register_transport("mock", client_side)
            .await
            .expect("mock registers");

        // Real HTTP router on an ephemeral port, backed by the same gateway.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind http test server");
        let base = format!("http://{}", listener.local_addr().expect("http addr"));
        let app = crate::http::router(Arc::new(gateway.clone()));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("http server runs");
        });
        let http = reqwest::Client::new();

        let service = GrpcMcpGatewayService {
            gateway,
            admin_token: None,
        };

        // ClientError: oversized arguments -> 400 / InvalidArgument.
        let big_args = serde_json::json!({ "cmd": "a".repeat(64) });
        let http_status = http
            .post(format!("{base}/v1/execute"))
            .json(&serde_json::json!({
                "tool": "shell.execute",
                "args": big_args.clone(),
                "principal": "opencode",
            }))
            .send()
            .await
            .expect("http execute runs")
            .status();
        let grpc_status = service
            .execute_tool(Request::new(ExecuteToolRequest {
                tool: "shell.execute".into(),
                args: big_args.to_string(),
                principal: "opencode".into(),
            }))
            .await
            .expect_err("oversized args rejected by grpc");
        assert_eq!(http_status, reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(grpc_status.code(), Code::InvalidArgument);
        assert!(grpc_status.message().contains("exceed"));

        // Forbidden: principal denied by policy -> 403 / PermissionDenied.
        let http_status = http
            .post(format!("{base}/v1/execute"))
            .json(&serde_json::json!({
                "tool": "filesystem.read",
                "args": { "path": "/tmp/example" },
                "principal": "test",
            }))
            .send()
            .await
            .expect("http execute runs")
            .status();
        let grpc_status = service
            .execute_tool(Request::new(ExecuteToolRequest {
                tool: "filesystem.read".into(),
                args: r#"{"path":"/tmp/example"}"#.into(),
                principal: "test".into(),
            }))
            .await
            .expect_err("policy-denied principal rejected by grpc");
        assert_eq!(http_status, reqwest::StatusCode::FORBIDDEN);
        assert_eq!(grpc_status.code(), Code::PermissionDenied);
        assert!(grpc_status.message().contains("Policy denied"));

        // Internal: upstream MCP error result -> 500 / Internal, with the
        // detailed cause logged server-side and replaced by a fixed message.
        let http_status = http
            .post(format!("{base}/v1/execute"))
            .json(&serde_json::json!({
                "tool": "filesystem.read",
                "args": { "path": "/tmp/example" },
                "principal": "opencode",
            }))
            .send()
            .await
            .expect("http execute runs")
            .status();
        let grpc_status = service
            .execute_tool(Request::new(ExecuteToolRequest {
                tool: "filesystem.read".into(),
                args: r#"{"path":"/tmp/example"}"#.into(),
                principal: "opencode".into(),
            }))
            .await
            .expect_err("upstream MCP error rejected by grpc");
        assert_eq!(http_status, reqwest::StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(grpc_status.code(), Code::Internal);
        assert_eq!(grpc_status.message(), INTERNAL_ERROR_MESSAGE);
        assert!(!grpc_status.message().contains("returned an error"));
    }
}
