//! Gateway core - orchestrates MCP tool execution.
//!
//! Tool calls can be served by two backends:
//!
//! 1. **Registered MCP servers** - upstream MCP servers spawned as child
//!    processes and driven by the official `rmcp` client over stdio. Tools
//!    are discovered at registration time via `tools/list`.
//! 2. **Sandbox fallback** - shelling out to the `opencode` CLI inside
//!    firejail/bwrap/docker/none.
//!
//! Both backends are only reachable for tools that pass the allowlist and
//! policy checks enforced by [`crate::McpGateway`]. Registering a server
//! never widens the allowlist: the allowlist file remains the single
//! source of truth for which tools may run at all.

use anyhow::{Context, Result};
use rmcp::model::CallToolRequestParam;
use rmcp::service::RunningService;
use rmcp::transport::{IntoTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::info;

/// Upper bound for the MCP `initialize` handshake plus `tools/list`
/// discovery while registering an upstream server.
const SERVER_REGISTRATION_TIMEOUT: Duration = Duration::from_secs(15);

/// An upstream MCP server registered with the gateway.
struct RegisteredServer {
    /// Live rmcp client. Dropping it cancels the background task, which
    /// drops the transport and kills the child process.
    client: RunningService<RoleClient, ()>,
    /// Tool names advertised by the server at registration time.
    tools: HashSet<String>,
}

#[allow(dead_code)]
pub struct Gateway {
    config: crate::config::McpGatewayConfig,
    allowlist: Arc<crate::allowlist::AllowList>,
    sandbox: Arc<crate::sandbox::Sandbox>,
    /// Upstream MCP servers keyed by registration name.
    servers: RwLock<HashMap<String, Arc<RegisteredServer>>>,
}

impl Gateway {
    pub async fn new(
        config: crate::config::McpGatewayConfig,
        allowlist: Arc<crate::allowlist::AllowList>,
        sandbox: Arc<crate::sandbox::Sandbox>,
    ) -> Result<Self> {
        Ok(Self {
            config,
            allowlist,
            sandbox,
            servers: RwLock::new(HashMap::new()),
        })
    }

    /// Execute a tool.
    ///
    /// The allowlist is consulted first: a tool without an allowlist entry
    /// cannot run even if a registered server advertises it. Registered
    /// servers take precedence over the sandbox fallback when they expose
    /// the tool name.
    pub async fn execute(&self, tool: &str, args: Value) -> Result<Value> {
        let tool_config = self
            .allowlist
            .get_config(tool)
            .ok_or_else(|| anyhow::anyhow!("Tool config not found: {}", tool))?;

        let server = {
            let servers = self.servers.read().await;
            servers
                .values()
                .find(|server| server.tools.contains(tool))
                .map(Arc::clone)
        };

        if let Some(server) = server {
            return call_registered_tool(&server, tool, args, &tool_config).await;
        }

        // Execute in the sandbox (opencode CLI fallback)
        self.sandbox.execute(tool, args, &tool_config).await
    }

    /// Spawn an upstream MCP server and register it with the gateway.
    ///
    /// The command runs as a child process and speaks MCP over stdio.
    /// Registering an existing name replaces the previous server and
    /// terminates its child process. Discovered tools are not added to the
    /// allowlist automatically: operators must list each tool in
    /// `mcp_allowlist.toml` before it can be executed.
    pub async fn register_mcp_server(&self, name: &str, command: Vec<String>) -> Result<()> {
        if name.trim().is_empty() {
            anyhow::bail!("MCP server name must not be empty");
        }
        let (program, args) = command
            .split_first()
            .context("MCP server command must not be empty")?;

        let mut cmd = Command::new(program);
        cmd.args(args);
        let transport = TokioChildProcess::new(cmd)
            .with_context(|| format!("Failed to spawn MCP server '{name}' ({program})"))?;

        self.register_transport(name, transport).await
    }

    /// Connect an already-built transport as the server `name`.
    ///
    /// Split out of [`Self::register_mcp_server`] so tests can drive the
    /// same protocol path with an in-memory duplex instead of a child
    /// process.
    async fn register_transport<T, E, A>(&self, name: &str, transport: T) -> Result<()>
    where
        T: IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let client = tokio::time::timeout(SERVER_REGISTRATION_TIMEOUT, ().serve(transport))
            .await
            .with_context(|| format!("MCP server '{name}' did not initialize in time"))?
            .with_context(|| format!("MCP server '{name}' initialization failed"))?;

        let tools = client
            .peer()
            .list_all_tools()
            .await
            .with_context(|| format!("Failed to list tools from MCP server '{name}'"))?;

        let tool_names: HashSet<String> = tools.iter().map(|tool| tool.name.to_string()).collect();
        info!(
            server = name,
            tools = tool_names.len(),
            "Registered MCP server"
        );

        let previous = self.servers.write().await.insert(
            name.to_string(),
            Arc::new(RegisteredServer {
                client,
                tools: tool_names,
            }),
        );

        if let Some(previous) = previous {
            // Replace semantics: dropping the old client cancels its
            // background task and kills the child process.
            drop(previous);
            info!(server = name, "Replaced existing MCP server registration");
        }

        Ok(())
    }
}

/// Call `tool` on a registered server, converting between the gateway's
/// JSON representation and the MCP model.
async fn call_registered_tool(
    server: &RegisteredServer,
    tool: &str,
    args: Value,
    tool_config: &crate::allowlist::AllowedTool,
) -> Result<Value> {
    let arguments = match args {
        Value::Null => None,
        Value::Object(arguments) => Some(arguments),
        other => anyhow::bail!("Tool '{tool}' arguments must be a JSON object, got: {other}"),
    };

    let timeout = Duration::from_millis(tool_config.timeout_ms);
    let result = tokio::time::timeout(
        timeout,
        server.client.peer().call_tool(CallToolRequestParam {
            name: tool.to_string().into(),
            arguments,
        }),
    )
    .await
    .with_context(|| {
        format!(
            "MCP tool '{tool}' timed out after {}ms",
            tool_config.timeout_ms
        )
    })?
    .with_context(|| format!("MCP tool '{tool}' call failed"))?;

    let value = serde_json::to_value(&result)?;
    if result.is_error.unwrap_or(false) {
        anyhow::bail!("MCP tool '{tool}' returned an error: {value}");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// Minimal MCP server speaking newline-delimited JSON-RPC over the
    /// given stream. Advertises a single `echo` tool. `fail_calls` makes
    /// `tools/call` answer with an MCP error result.
    async fn run_mock_mcp_server(stream: tokio::io::DuplexStream, fail_calls: bool) {
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut lines = BufReader::new(read_half).lines();

        while let Ok(Some(line)) = lines.next_line().await {
            let message: Value = serde_json::from_str(&line).expect("valid JSON-RPC line");
            let method = message["method"].as_str().unwrap_or_default();
            let id = message.get("id").cloned();

            let response = match method {
                "initialize" => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2025-03-26",
                        "capabilities": {},
                        "serverInfo": { "name": "mock-server", "version": "0.1.0" }
                    }
                })),
                "notifications/initialized" => None,
                "tools/list" => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": [{
                            "name": "echo",
                            "description": "Echo back the arguments",
                            "inputSchema": { "type": "object" }
                        }]
                    }
                })),
                "tools/call" => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": "pong" }],
                        "isError": fail_calls
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

    const ECHO_ALLOWLIST: &str = r#"
principals = ["test"]

[[tools]]
name = "echo"
description = "Echo tool"
allowed_principals = ["test"]
max_args_size = 1048576
timeout_ms = 5000
requires_approval = false
"#;

    /// Gateway backed by a throwaway allowlist file containing `echo`.
    async fn test_gateway(name: &str) -> Gateway {
        let dir = std::env::temp_dir().join(format!("jarvis-mcp-gw-test-{name}"));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let allowlist_path = dir.join("mcp_allowlist.toml");
        tokio::fs::write(&allowlist_path, ECHO_ALLOWLIST)
            .await
            .unwrap();

        let config = crate::config::McpGatewayConfig {
            allowlist_path: allowlist_path.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let allowlist = Arc::new(crate::allowlist::AllowList::new(&config).await.unwrap());
        let sandbox = Arc::new(crate::sandbox::Sandbox::new(&config).await.unwrap());
        Gateway::new(config, allowlist, sandbox).await.unwrap()
    }

    #[tokio::test]
    async fn registered_server_tools_are_routed_to_the_mcp_client() {
        let gateway = test_gateway("routes").await;

        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_mock_mcp_server(server_side, false));

        gateway
            .register_transport("mock", client_side)
            .await
            .expect("registration succeeds");

        {
            let servers = gateway.servers.read().await;
            let server = servers.get("mock").expect("server registered");
            assert!(server.tools.contains("echo"));
        }

        let result = gateway
            .execute("echo", json!({ "message": "hi" }))
            .await
            .expect("tool call succeeds");
        assert_eq!(result["content"][0]["text"], "pong");
        assert_eq!(result["isError"], false);
    }

    #[tokio::test]
    async fn re_registering_a_name_replaces_the_previous_server() {
        let gateway = test_gateway("replace").await;

        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_mock_mcp_server(server_side, false));
        gateway
            .register_transport("mock", client_side)
            .await
            .unwrap();

        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_mock_mcp_server(server_side, false));
        gateway
            .register_transport("mock", client_side)
            .await
            .expect("replacement registration succeeds");

        let result = gateway
            .execute("echo", json!({}))
            .await
            .expect("tool call succeeds after replacement");
        assert_eq!(result["content"][0]["text"], "pong");
    }

    #[tokio::test]
    async fn registration_rejects_an_empty_command() {
        let gateway = test_gateway("empty-command").await;

        let error = gateway
            .register_mcp_server("empty", Vec::new())
            .await
            .expect_err("empty command is rejected");
        assert!(error.to_string().contains("must not be empty"));
    }

    #[tokio::test]
    async fn error_results_and_non_object_arguments_surface_as_errors() {
        let gateway = test_gateway("errors").await;

        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_mock_mcp_server(server_side, true));
        gateway
            .register_transport("mock", client_side)
            .await
            .unwrap();

        let error = gateway
            .execute("echo", json!("not-an-object"))
            .await
            .expect_err("non-object arguments are rejected");
        assert!(error.to_string().contains("must be a JSON object"));

        let error = gateway
            .execute("echo", json!({ "message": "hi" }))
            .await
            .expect_err("MCP error results are surfaced");
        assert!(error.to_string().contains("returned an error"));
    }
}
