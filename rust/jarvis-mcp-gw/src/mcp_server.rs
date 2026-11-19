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
//! native tools keep their hardcoded argument contract, tools served by a
//! registered upstream MCP server reuse the `inputSchema` that server
//! declared at registration time, and everything else gets a permissive
//! object schema.

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
    /// Native tools keep their hardcoded argument schema. For every other
    /// tool, the `inputSchema` declared by the upstream server at
    /// registration time is reused when available; tools with neither source
    /// fall back to the permissive generic schema.
    async fn tools(&self) -> Vec<Tool> {
        let mut tools = Vec::new();
        for tool in self.gateway.tools_for_principal(&self.principal) {
            let schema = match native_input_schema(&tool.name) {
                Some(schema) => schema,
                None => self
                    .gateway
                    .gateway
                    .upstream_input_schema(&tool.name)
                    .await
                    .unwrap_or_else(generic_input_schema),
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
                Ok(mcp_result) => Ok(mcp_result),
                Err(_) => Ok(CallToolResult::success(vec![Content::text(
                    serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()),
                )])),
            },
            // Execution failures (policy, allowlist, transport) are tool
            // errors per the MCP spec, not protocol errors.
            Err(error) => Ok(CallToolResult::error(vec![Content::text(format!(
                "{error:#}"
            ))])),
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

    /// Native tools keep their hardcoded schema even when an upstream server
    /// happens to advertise the same tool name.
    #[tokio::test]
    async fn native_tool_schema_takes_precedence_over_upstream_schema() {
        let gateway = endpoint_gateway("native-precedence").await;
        allow_upstream_tool(&gateway, "web.fetch");

        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(crate::gateway::run_mock_mcp_server_with_schema(
            server_side,
            "web.fetch",
            json!({
                "type": "object",
                "properties": { "from_upstream": { "type": "boolean" } }
            }),
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
            .expect("native tool listed");
        assert_eq!(
            serde_json::Value::Object((*advertised.input_schema).clone()),
            serde_json::Value::Object(tool_input_schema("web.fetch"))
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
}
