//! MCP server surface for the gateway (Streamable HTTP).
//!
//! Exposes the gateway over the MCP Streamable HTTP transport so MCP
//! clients — OpenCode V2 configured as a `remote` server with
//! `oauth: false` — can list and call tools. Every call goes through
//! [`McpGateway::execute_tool`], so policy and allowlist checks are
//! identical to the REST and gRPC paths.
//!
//! The principal is fixed server-side (`JARVIS_MCP_GW_MCP_PRINCIPAL`,
//! default `opencode`); clients do not choose it. The endpoint shares the
//! localhost-only HTTP listener of the gateway.
//!
//! Tool schemas on `tools/list` come from three sources, in priority order:
//! tools served by a registered upstream MCP server reuse the `inputSchema`
//! that server declared at registration time, native tools keep their
//! hardcoded argument contract, and everything else gets a permissive object
//! schema.
//!
//! This order mirrors execution routing, where an upstream server always wins
//! over the native handler. We adopted aligned ordering instead of rejecting
//! colliding registrations: the single ownership decision lives in
//! `gateway::resolve_target`, so the advertised contract always belongs to
//! the backend that will actually run the call.

use crate::McpGateway;
use rmcp::model::{
    CallToolRequestParam, CallToolResult, Content, Implementation, ListToolsResult,
    PaginatedRequestParam, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::Value;
use std::sync::Arc;

/// Adapter that serves gateway capabilities over MCP.
#[derive(Clone)]
pub struct JarvisMcpServer {
    gateway: Arc<McpGateway>,
    principal: String,
}

impl JarvisMcpServer {
    /// Serve `gateway` with all calls attributed to `principal`.
    pub fn new(gateway: Arc<McpGateway>, principal: String) -> Self {
        Self { gateway, principal }
    }

    /// Tools the endpoint principal may call, in stable order.
    ///
    /// Schema selection uses the same upstream-first order as execution
    /// routing (`gateway::resolve_target`): a tool owned by a registered
    /// upstream server reuses the `inputSchema` that server declared at
    /// registration time, because that server is also what `tools/call` will
    /// execute. Native tools keep their hardcoded schema only when no
    /// upstream owns the name; tools with neither source fall back to the
    /// permissive generic schema.
    async fn tools(&self) -> Vec<Tool> {
        let mut tools = Vec::new();
        for tool in self.gateway.tools_for_principal(&self.principal) {
            let schema = match self.gateway.gateway.upstream_input_schema(&tool.name).await {
                Some(schema) => schema,
                None => native_input_schema(&tool.name).unwrap_or_else(generic_input_schema),
            };
            tools.push(Tool::new(
                tool.name.clone(),
                tool.description,
                Arc::new(schema),
            ));
        }
        tools
    }
}

/// JSON Schema for a **native** tool's arguments.
///
/// Native handlers have well-known contracts that clients should see so
/// they can validate calls before asking for an approval. `None` means
/// `name` is not a native tool: the caller must then consult the upstream
/// schema captured at registration time (or fall back to the generic one).
fn native_input_schema(name: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    // `Sandbox::resolve_path` maps a missing `path` to "." for every native
    // handler, so the filesystem tools must not advertise it as required:
    // clients would otherwise reject calls the gateway is able to accept.
    let path = serde_json::json!({
        "type": "string",
        "description": "Path relative to the configured base directory (optional; defaults to \".\" when omitted)"
    });
    // git.* keep their original contract: `path` stays required there.
    let git_path = serde_json::json!({
        "type": "string",
        "description": "Path relative to the configured base directory"
    });
    // `web.fetch` honours `max_bytes` through `web_max_bytes`, which clamps to
    // 1024..=1048576 (default 262144). `filesystem.read` ignores the argument
    // entirely (`native_read` enforces a fixed 1 MiB cap), so it is no longer
    // advertised there rather than advertising a value we do not respect.
    let web_max_bytes = serde_json::json!({
        "type": "integer",
        "minimum": 1024,
        "maximum": 1048576,
        "description": "Maximum bytes returned (clamped to 1024..=1048576; defaults to 262144)"
    });
    let schema = match name {
        "filesystem.read" => Some(serde_json::json!({
            "type": "object",
            "properties": { "path": path },
            "additionalProperties": true
        })),
        "filesystem.list" => Some(serde_json::json!({
            "type": "object",
            "properties": { "path": path },
            "additionalProperties": true
        })),
        "git.status" | "git.diff" => Some(serde_json::json!({
            "type": "object",
            "properties": { "path": git_path },
            "required": ["path"],
            "additionalProperties": true
        })),
        "web.fetch" => Some(serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "Public HTTP(S) URL (private addresses are refused)"
                },
                "max_bytes": web_max_bytes
            },
            "required": ["url"],
            "additionalProperties": true
        })),
        _ => None,
    };
    schema.and_then(|value| value.as_object().cloned())
}

/// Permissive fallback for tools with neither a native contract nor an
/// upstream-declared schema. Kept intentionally generic: it must not claim
/// required fields the gateway cannot guarantee.
fn generic_input_schema() -> serde_json::Map<String, serde_json::Value> {
    serde_json::json!({
        "type": "object",
        "additionalProperties": true
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

/// JSON Schema for a tool's arguments using only the name-based sources.
///
/// Upstream-declared schemas are layered on by `JarvisMcpServer::tools`,
/// which has access to the registered-server inventory; this helper stays
/// name-only so the native-schema regression tests can call it directly.
#[cfg(test)]
fn tool_input_schema(name: &str) -> serde_json::Map<String, serde_json::Value> {
    native_input_schema(name).unwrap_or_else(generic_input_schema)
}

/// Only phrase an MCP client ever sees for a gateway-side failure.
///
/// Mirrors `grpc::INTERNAL_ERROR_MESSAGE`: the full error chain (sandbox
/// paths, upstream bodies, rmcp internals) stays in the server-side
/// `tracing::error!` log and is never serialized into a tool result.
const INTERNAL_ERROR_MESSAGE: &str = "internal gateway error";

/// Phrase used if a backend unexpectedly hands us an `is_error` result
/// instead of routing it through the safe-summary error path.
const UPSTREAM_ERROR_MESSAGE: &str = "upstream tool returned an error";

/// Reduce a caller-input failure to a small, fixed vocabulary.
///
/// Several `client_error()` messages interpolate absolute sandbox paths, git
/// stderr or raw URLs (e.g. `stat /home/...`). Echoing such text to an MCP
/// client would open the same disclosure channel as internal errors, so only
/// known-static messages are passed through verbatim; everything else is
/// replaced by one fixed phrase.
fn safe_client_error_text(message: &str) -> &'static str {
    match message {
        "missing 'url' argument" => "missing 'url' argument",
        "url has no host" => "url has no host",
        "credentials in URL are not allowed" => "credentials in URL are not allowed",
        "redirect response has no Location header" => "redirect response has no Location header",
        "redirect response has an empty Location header" => {
            "redirect response has an empty Location header"
        }
        "web.fetch timed out" => "web.fetch timed out",
        "web.fetch body timed out" => "web.fetch body timed out",
        "MCP server name must not be empty" => "MCP server name must not be empty",
        "MCP server command must not be empty" => "MCP server command must not be empty",
        _ if message.starts_with("too many redirects (") => "too many redirects",
        _ if message.starts_with("file too large to read (") => "file too large to read",
        _ if message.starts_with("response too large (") => "response too large",
        _ if message.starts_with("git output too large (") => "git output too large",
        _ if message.starts_with("unsupported scheme '") => "unsupported scheme",
        _ if message.starts_with("unsupported content type:") => "unsupported content type",
        _ if message.starts_with("host '") => "host is not allowed by the egress policy",
        _ if message.starts_with("refusing to fetch non-public address") => {
            "refusing to fetch non-public address"
        }
        _ if message.contains("dns lookup") => "dns lookup failed",
        _ if message.starts_with("no native handler for tool '") => "no native handler for tool",
        _ if message.starts_with("MCP server '") => "upstream MCP server is unavailable",
        _ => "invalid tool arguments",
    }
}

/// Turn an execution failure into the only text an MCP client may see.
///
/// Classification uses `McpGatewayError::kind()` (as the HTTP and gRPC
/// adapters do), never the human-readable message. Untyped failures —
/// including upstream `bail!` results that were summarized by
/// `gateway::safe_error_summary` and anyhow context chains — fall back to
/// `Internal` and are sanitized.
fn client_facing_tool_error(error: &anyhow::Error) -> String {
    use crate::error::ErrorKind;

    let kind = error
        .downcast_ref::<crate::error::McpGatewayError>()
        .map(crate::error::McpGatewayError::kind)
        .unwrap_or(ErrorKind::Internal);
    match kind {
        // Fixed policy wording plus tool/principal names the caller already
        // knows; no sandbox paths or upstream bodies are interpolated here.
        ErrorKind::Forbidden => error.to_string(),
        // Caller-input errors are rounded to a fixed vocabulary as well.
        ErrorKind::ClientError => safe_client_error_text(&error.to_string()).to_string(),
        ErrorKind::Internal => {
            let detail = format!("{error:#}");
            tracing::error!(error = %detail, "MCP tools/call failed with internal gateway error");
            INTERNAL_ERROR_MESSAGE.to_string()
        }
    }
}

impl ServerHandler for JarvisMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "JARVIS tool gateway. Every tool call is policy-checked and \
                 allowlist-gated before it runs."
                    .to_string(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "jarvis-mcp-gw".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            ..Default::default()
        }
    }

    /// Lists every tool available to the endpoint principal.
    ///
    /// # Pagination contract
    ///
    /// `PaginatedRequestParam.cursor` is intentionally ignored: the complete
    /// catalogue (bounded by the allowlist) is returned in a single response
    /// and `nextCursor` is never emitted. Clients must not expect a cursor or
    /// a second page. Keeping this contract avoids an unnecessary pagination
    /// implementation for a small, config-bounded tool list.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.tools().await))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let tool = request.name.to_string();
        let args = request.arguments.map(Value::Object).unwrap_or(Value::Null);
        match self
            .gateway
            .execute_tool(&tool, args, &self.principal)
            .await
        {
            // Results from registered MCP servers already carry the MCP
            // result shape; forward them natively so clients see the
            // original content blocks instead of an escaped JSON string.
            Ok(result) => match serde_json::from_value::<CallToolResult>(result.clone()) {
                // `gateway::call_registered_tool` already turns upstream
                // `is_error` results into a failure summarized by
                // `safe_error_summary`, so this arm is only a defensive
                // backstop: never forward an error result's body, even if a
                // future backend returns one as a success value.
                Ok(mcp_result) if mcp_result.is_error.unwrap_or(false) => {
                    tracing::error!(tool = %tool, "backend returned an is_error result");
                    Ok(CallToolResult::error(vec![Content::text(
                        UPSTREAM_ERROR_MESSAGE.to_string(),
                    )]))
                }
                Ok(mcp_result) => Ok(mcp_result),
                Err(_) => Ok(CallToolResult::success(vec![Content::text(
                    serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()),
                )])),
            },
            // Execution failures (policy, allowlist, transport) are tool
            // errors per the MCP spec, not protocol errors. Classify by type
            // so internal detail stays server-side and callers only see a
            // fixed phrase.
            Err(error) => Ok(CallToolResult::error(vec![Content::text(
                client_facing_tool_error(&error),
            )])),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpGatewayConfig;
    use crate::testing;
    use rmcp::ServiceExt;
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    use serde_json::json;

    const ENDPOINT_ALLOWLIST: &str = r#"
principals = ["opencode", "jarvis"]

[[tools]]
name = "filesystem.read"
description = "Read a file"
allowed_principals = ["opencode"]
max_args_size = 1048576
timeout_ms = 5000
requires_approval = false

[[tools]]
name = "git.commit"
description = "Commit staged changes"
allowed_principals = ["jarvis"]
max_args_size = 1048576
timeout_ms = 5000
requires_approval = true

[[tools]]
name = "shell.execute"
description = "Execute a shell command"
allowed_principals = ["opencode"]
max_args_size = 1048576
timeout_ms = 5000
requires_approval = true
"#;

    /// Gateway over a throwaway allowlist with the shipped policy files.
    async fn endpoint_gateway(name: &str) -> Arc<McpGateway> {
        let dir = std::env::temp_dir().join(format!("jarvis-mcp-gw-endpoint-{name}"));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let allowlist_path = dir.join("mcp_allowlist.toml");
        tokio::fs::write(&allowlist_path, ENDPOINT_ALLOWLIST)
            .await
            .unwrap();

        let config = McpGatewayConfig {
            allowlist_path: allowlist_path.to_string_lossy().into_owned(),
            // Unreachable by construction: tests must not reach a policy
            // service that happens to listen on the default address.
            policy_http_addr: "http://127.0.0.1:1".into(),
            ..Default::default()
        };
        let policy = Arc::new(testing::shipped_policy_client(name).await);
        Arc::new(
            McpGateway::new(config, policy)
                .await
                .expect("gateway starts"),
        )
    }

    /// Serve the gateway's HTTP surface on an ephemeral localhost port.
    async fn start_endpoint(gateway: Arc<McpGateway>) -> std::net::SocketAddr {
        let app = crate::http::router(gateway);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("http server runs");
        });
        addr
    }

    /// Connect an MCP client to the endpoint, exactly like OpenCode does.
    async fn connect(
        addr: std::net::SocketAddr,
    ) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
        let transport = StreamableHttpClientTransport::with_client(
            reqwest::Client::new(),
            StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp")),
        );
        ().serve(transport).await.expect("MCP client connects")
    }

    fn first_text(result: &CallToolResult) -> String {
        serde_json::to_value(&result.content).expect("content serializes")[0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// `path` is optional for the filesystem tools: `Sandbox::resolve_path`
    /// resolves a missing `path` to ".", so the native handlers accept calls
    /// that omit it. Advertising it as `required` made clients reject those
    /// calls before they ever reached the gateway. `web.fetch` keeps `url`
    /// required because `native_web_fetch` has no default for it, and now
    /// advertises exactly the `max_bytes` range the implementation honours;
    /// `filesystem.read` drops `max_bytes` because `native_read` ignores it.
    #[test]
    fn native_tools_advertise_required_arguments() {
        use super::tool_input_schema;

        let read = tool_input_schema("filesystem.read");
        assert!(
            read.get("required")
                .and_then(|value| value.as_array())
                .is_none_or(|fields| !fields.iter().any(|field| field.as_str() == Some("path"))),
            "path defaults to \".\" natively and must not be required"
        );
        assert!(read["properties"]["path"].is_object());
        assert!(
            read["properties"]["path"]["description"]
                .as_str()
                .unwrap_or_default()
                .contains("defaults to \".\""),
            "path description must document the native \".\" default"
        );
        assert!(
            read["properties"].get("max_bytes").is_none(),
            "native_read ignores max_bytes; it must not be advertised"
        );

        let list = tool_input_schema("filesystem.list");
        assert!(
            list.get("required")
                .and_then(|value| value.as_array())
                .is_none_or(|fields| !fields.iter().any(|field| field.as_str() == Some("path"))),
            "path defaults to \".\" natively and must not be required"
        );

        let fetch = tool_input_schema("web.fetch");
        assert_eq!(fetch["required"], serde_json::json!(["url"]));
        assert_eq!(fetch["properties"]["max_bytes"]["minimum"], 1024);
        assert_eq!(fetch["properties"]["max_bytes"]["maximum"], 1048576);

        let unknown = tool_input_schema("some.upstream.tool");
        assert_eq!(unknown["type"], "object");
        assert_eq!(unknown["additionalProperties"], true);
    }

    /// Allowlist entry for a tool served by a registered upstream server.
    fn allow_upstream_tool(gateway: &McpGateway, name: &str) {
        gateway.allowlist.add_tool(crate::allowlist::AllowedTool {
            name: name.to_string(),
            description: format!("{name} upstream tool"),
            allowed_principals: vec!["opencode".to_string()],
            max_args_size: 1048576,
            timeout_ms: 5000,
            requires_approval: false,
        });
    }

    /// Regression: a tool served by a registered upstream MCP server must be
    /// advertised with the `inputSchema` that server declared at
    /// registration time, not the permissive catch-all schema. A tool with
    /// neither a native contract nor an upstream declaration still gets the
    /// generic fallback.
    #[tokio::test]
    async fn upstream_tools_advertise_their_registered_input_schema() {
        let gateway = endpoint_gateway("upstream-schema").await;
        allow_upstream_tool(&gateway, "search.query");

        let declared = json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "search text" },
                "limit": { "type": "integer", "minimum": 1 }
            },
            "required": ["query"],
            "additionalProperties": false
        });
        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(crate::gateway::run_mock_mcp_server_with_schema(
            server_side,
            "search.query",
            declared.clone(),
        ));
        gateway
            .register_transport("mock", client_side)
            .await
            .expect("upstream registers");

        let server = JarvisMcpServer::new(gateway, "opencode".into());
        let tools = server.tools().await;

        let advertised = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "search.query")
            .expect("upstream tool listed");
        assert_eq!(
            serde_json::Value::Object((*advertised.input_schema).clone()),
            declared
        );

        // `shell.execute` is allowlisted but not advertised by the upstream
        // and has no native schema, so it must keep the generic fallback.
        let fallback = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "shell.execute")
            .expect("non-upstream tool still listed");
        assert_eq!(
            serde_json::Value::Object((*fallback.input_schema).clone()),
            serde_json::Value::Object(generic_input_schema())
        );
    }

    /// Regression: the advertised schema follows the same upstream-first
    /// order as execution routing. An upstream that advertises `web.fetch`
    /// also executes it (`Gateway::execute` prefers registered servers over
    /// the native handler), so `tools/list` must show that upstream's declared
    /// schema rather than the native `web.fetch` contract. The historical
    /// test name is kept; only the expected value changed to mirror routing.
    #[tokio::test]
    async fn native_tool_schema_takes_precedence_over_upstream_schema() {
        let gateway = endpoint_gateway("native-precedence").await;
        allow_upstream_tool(&gateway, "web.fetch");

        let declared = json!({
            "type": "object",
            "properties": { "from_upstream": { "type": "boolean" } }
        });
        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(crate::gateway::run_mock_mcp_server_with_schema(
            server_side,
            "web.fetch",
            declared.clone(),
        ));
        gateway
            .register_transport("mock", client_side)
            .await
            .expect("upstream registers");

        let server = JarvisMcpServer::new(gateway, "opencode".into());
        let tools = server.tools().await;
        let advertised = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "web.fetch")
            .expect("tool listed");
        let shown = serde_json::Value::Object((*advertised.input_schema).clone());
        // The upstream owns the name and therefore runs the call: advertise
        // its contract, never the native hard-coded `web.fetch` schema.
        assert_eq!(shown, declared);
        assert_ne!(
            shown,
            serde_json::Value::Object(tool_input_schema("web.fetch")),
            "a native schema must not be shown for an upstream-owned tool"
        );
    }

    #[tokio::test]
    async fn endpoint_lists_the_principal_tools_and_routes_calls() {
        let gateway = endpoint_gateway("routes").await;

        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(testing::run_mock_mcp_server(
            server_side,
            "filesystem.read",
            false,
        ));
        gateway
            .register_transport("mock", client_side)
            .await
            .expect("mock server registers");

        let addr = start_endpoint(gateway).await;
        let client = connect(addr).await;

        let tools = client
            .peer()
            .list_all_tools()
            .await
            .expect("tools/list succeeds");
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
        assert_eq!(
            names,
            vec!["filesystem.read", "shell.execute"],
            "approval-required tools are listed and routed through human approval"
        );

        let result = client
            .peer()
            .call_tool(CallToolRequestParam {
                name: "filesystem.read".into(),
                arguments: Some(serde_json::Map::from_iter([(
                    "path".into(),
                    json!("/tmp/example"),
                )])),
            })
            .await
            .expect("tools/call returns");

        assert_eq!(result.is_error, Some(false));
        assert_eq!(first_text(&result), "pong");
    }

    #[tokio::test]
    async fn endpoint_reports_denials_as_tool_errors() {
        let gateway = endpoint_gateway("denials").await;
        let addr = start_endpoint(gateway).await;
        let client = connect(addr).await;

        // The shipped policy grants `tool:git.commit` to the endpoint
        // principal; the allowlist does not. The allowlist layer must deny.
        let result = client
            .peer()
            .call_tool(CallToolRequestParam {
                name: "git.commit".into(),
                arguments: Some(serde_json::Map::new()),
            })
            .await
            .expect("denial is reported as a tool result");
        assert_eq!(result.is_error, Some(true));
        let text = first_text(&result);
        assert!(
            text.contains("not allowed to execute"),
            "unexpected: {text}"
        );

        // Unknown tools are denied by the policy before the allowlist.
        let result = client
            .peer()
            .call_tool(CallToolRequestParam {
                name: "unknown.tool".into(),
                arguments: Some(serde_json::Map::new()),
            })
            .await
            .expect("denial is reported as a tool result");
        assert_eq!(result.is_error, Some(true));
        let text = first_text(&result);
        assert!(text.contains("Policy denied"), "unexpected: {text}");

        // The allowlist grants this tool to OpenCode and marks it as
        // approval-required. With no policy HTTP server reachable in this
        // test, the approval request fails and the call is denied
        // (fail-closed).
        let result = client
            .peer()
            .call_tool(CallToolRequestParam {
                name: "shell.execute".into(),
                arguments: Some(serde_json::Map::new()),
            })
            .await
            .expect("approval requirement is reported as a tool result");
        assert_eq!(result.is_error, Some(true));
        let text = first_text(&result);
        assert!(
            text.contains("requires human approval"),
            "unexpected: {text}"
        );
    }

    /// Regression: internal failures must never put their detail in the tool
    /// result body. Classification is by `McpGatewayError::kind()`, so
    /// sandbox paths, upstream text and untyped anyhow chains all collapse to
    /// the single fixed phrase; caller-input errors use a fixed vocabulary.
    #[test]
    fn tool_errors_do_not_leak_internal_details() {
        use crate::error::McpGatewayError;

        let sandbox: anyhow::Error = McpGatewayError::Sandbox(
            "stat /home/koppe/.secret/token: permission denied".to_string(),
        )
        .into();
        let text = client_facing_tool_error(&sandbox);
        assert_eq!(text, INTERNAL_ERROR_MESSAGE);
        assert!(!text.contains("/home/koppe"));

        let upstream: anyhow::Error =
            anyhow::anyhow!("MCP tool 'search.query' returned an error: upstream secret body");
        let text = client_facing_tool_error(&upstream);
        assert_eq!(text, INTERNAL_ERROR_MESSAGE);
        assert!(!text.contains("upstream secret body"));

        // A `client_error()` that interpolates a resolved absolute path is
        // rounded to the generic phrase, not echoed back to the client.
        let path_error: anyhow::Error = McpGatewayError::ClientError(
            "stat /home/koppe/.secret/token: permission denied".to_string(),
        )
        .into();
        let text = client_facing_tool_error(&path_error);
        assert_eq!(text, "invalid tool arguments");
        assert!(!text.contains("/home/koppe"));

        // Static, safe client wording is preserved.
        let missing_url: anyhow::Error =
            McpGatewayError::ClientError("missing 'url' argument".to_string()).into();
        assert_eq!(
            client_facing_tool_error(&missing_url),
            "missing 'url' argument"
        );

        // Forbidden wording is unchanged: existing denial contracts rely on
        // it and it contains no sandbox paths or upstream bodies.
        let denied: anyhow::Error = McpGatewayError::PrincipalNotAllowed {
            principal: "opencode".into(),
            tool: "git.commit".into(),
        }
        .into();
        assert_eq!(
            client_facing_tool_error(&denied),
            "Principal 'opencode' is not allowed to execute tool 'git.commit'"
        );
    }

    /// Minimal upstream that answers `tools/call` with an `is_error` result
    /// carrying a secret in its content body.
    async fn run_error_result_mcp_server(
        stream: tokio::io::DuplexStream,
        tool_name: &str,
        secret: String,
    ) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut lines = BufReader::new(read_half).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let message: serde_json::Value =
                serde_json::from_str(&line).expect("valid JSON-RPC line");
            let method = message["method"].as_str().unwrap_or_default();
            let id = message.get("id").cloned();
            let response = match method {
                "initialize" => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2025-03-26",
                        "capabilities": {},
                        "serverInfo": { "name": "error-mock", "version": "0.1.0" }
                    }
                })),
                "notifications/initialized" => None,
                "tools/list" => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": [{
                            "name": tool_name,
                            "description": "Failing upstream tool",
                            "inputSchema": { "type": "object" }
                        }]
                    }
                })),
                "tools/call" => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": secret }],
                        "isError": true
                    }
                })),
                _ => None,
            };
            if let Some(response) = response {
                let mut line = serde_json::to_string(&response).expect("serializable response");
                line.push('\n');
                write_half
                    .write_all(line.as_bytes())
                    .await
                    .expect("write response");
                write_half.flush().await.expect("flush response");
            }
        }
    }

    /// End-to-end regression: an upstream `is_error` body must not reach the
    /// MCP client. `call_registered_tool` summarizes it via
    /// `safe_error_summary`, and the MCP surface returns only the fixed
    /// internal phrase.
    #[tokio::test]
    async fn upstream_error_body_is_not_returned_to_mcp_clients() {
        const PROBE: &str = "upstream-secret-token-abc123";
        let gateway = endpoint_gateway("upstream-error-body").await;

        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_error_result_mcp_server(
            server_side,
            "filesystem.read",
            PROBE.to_string(),
        ));
        gateway
            .register_transport("mock", client_side)
            .await
            .expect("mock server registers");

        let addr = start_endpoint(gateway).await;
        let client = connect(addr).await;
        let result = client
            .peer()
            .call_tool(CallToolRequestParam {
                name: "filesystem.read".into(),
                arguments: Some(serde_json::Map::new()),
            })
            .await
            .expect("tools/call returns");

        assert_eq!(result.is_error, Some(true));
        let text = first_text(&result);
        assert!(!text.contains(PROBE), "upstream body leaked: {text}");
        assert_eq!(text, INTERNAL_ERROR_MESSAGE);
    }
}
