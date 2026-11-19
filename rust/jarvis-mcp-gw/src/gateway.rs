//! Gateway core - orchestrates MCP tool execution.
//!
//! Tool calls are served by two backends:
//!
//! 1. **Native Rust handlers** - read-only tools (`filesystem.*`, `git.*`,
//!    `web.fetch`) implemented in the gateway itself (ADR-0002/0003).
//! 2. **Registered MCP servers** - upstream MCP servers spawned as child
//!    processes and driven by the official `rmcp` client over stdio. Tools
//!    are discovered at registration time via `tools/list`.
//!
//! There is no agent-runtime CLI fallback (ADR-0002): tools with neither a
//! native handler nor an upstream server return a clear error. [`Gateway`]
//! is a low-level execution primitive: it routes a tool to a registered
//! server or the native handler and only requires that an allowlist entry
//! exist. It does **not** itself enforce policy, principal, argument-size
//! or approval checks. Those guarantees are applied by
//! [`crate::McpGateway::execute_tool`], the policy-enforcing entry point
//! used by the REST, gRPC and MCP surfaces. Registering a server never
//! widens the allowlist: the allowlist file remains the single source of
//! truth for which tools may run at all.
//!
//! Startup registration from `config.upstreams` is driven by `main.rs` through
//! [`Gateway::register_mcp_server`]. The runtime `RegisterMcpServer` gRPC RPC
//! uses the same method, so both paths share the same discovery and routing.

use crate::error::McpGatewayError;
use anyhow::{Context, Result};
use rmcp::model::CallToolRequestParam;
use rmcp::service::RunningService;
use rmcp::transport::IntoTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock};
use tracing::info;

/// Upper bound for the MCP `initialize` handshake plus `tools/list`
/// discovery while registering an upstream server.
const SERVER_REGISTRATION_TIMEOUT: Duration = Duration::from_secs(15);

/// Caller-input failures are typed as `McpGatewayError::ClientError` so the
/// transport adapters keep mapping them to HTTP 400. `ClientError` renders as
/// its message verbatim, so existing error-text expectations stay unchanged.
fn client_error(message: impl Into<String>) -> anyhow::Error {
    McpGatewayError::ClientError(message.into()).into()
}

/// Owner of the OS process backing a registered MCP server.
///
/// `rmcp`'s `TokioChildProcess` consumes the `Command` and does not expose a
/// `try_wait()` handle, so the gateway spawns the child itself and keeps it
/// here. Dropping the handle kills the child, preserving the "a failed or
/// replaced registration never leaves an orphan" property that
/// `TokioChildProcess` used to provide via its own drop cleanup.
struct ChildHandle {
    child: Mutex<Child>,
}

impl ChildHandle {
    fn new(child: Child) -> Self {
        Self {
            child: Mutex::new(child),
        }
    }

    /// `true` means the process exited **or** its status could not be read.
    /// Both outcomes are treated as dead so callers fail closed.
    async fn is_dead(&self) -> bool {
        let mut child = self.child.lock().await;
        match child.try_wait() {
            Ok(None) => false,
            Ok(Some(_)) | Err(_) => true,
        }
    }

    /// Best-effort synchronous kill for the drop/replacement paths, where an
    /// async lock cannot be awaited. If the lock is briefly contended, the
    /// final `Drop` still performs the kill once the probe releases it.
    fn kill(&self) {
        if let Ok(mut child) = self.child.try_lock() {
            let _ = child.start_kill();
        }
    }
}

impl Drop for ChildHandle {
    fn drop(&mut self) {
        self.kill();
    }
}

/// How liveness is determined for a registered server.
// `NoChild` is used by the transport-only registration helper below, which
// is exercised by tests; keep it available without triggering dead_code.
#[allow(dead_code)]
enum ProcessLiveness {
    /// Transport-only registration (e.g. the in-memory duplex used by tests).
    /// There is no OS process, so nothing can die out from under us.
    NoChild,
    /// A real child process; checked with `try_wait()` before tool execution.
    Child(Box<ChildHandle>),
    /// Test-only probe. Holds a state code so the regression tests can
    /// simulate both "exited" and "status unknown" without spawning a real
    /// subprocess.
    #[cfg(test)]
    Mock(Arc<std::sync::atomic::AtomicU8>),
}

#[cfg(test)]
const MOCK_LIVENESS_RUNNING: u8 = 0;
#[cfg(test)]
const MOCK_LIVENESS_EXITED: u8 = 1;
#[cfg(test)]
const MOCK_LIVENESS_ERROR: u8 = 2;

impl ProcessLiveness {
    async fn is_dead(&self) -> bool {
        match self {
            Self::NoChild => false,
            Self::Child(handle) => handle.is_dead().await,
            #[cfg(test)]
            Self::Mock(state) => match state.load(std::sync::atomic::Ordering::SeqCst) {
                MOCK_LIVENESS_RUNNING => false,
                MOCK_LIVENESS_EXITED | MOCK_LIVENESS_ERROR => true,
                // Unknown probe values also fail closed.
                _ => true,
            },
        }
    }

    fn kill(&self) {
        if let Self::Child(handle) = self {
            handle.kill();
        }
    }
}

/// An upstream MCP server registered with the gateway.
struct RegisteredServer {
    /// Live rmcp client. Dropping it cancels the background task, which
    /// drops the transport and closes its side of the child's stdio.
    client: RunningService<RoleClient, ()>,
    /// Tool names advertised by the server at registration time.
    tools: HashSet<String>,
    /// Liveness probe for the backing process (if the server was spawned as
    /// a child). Consulted before every tool call.
    liveness: ProcessLiveness,
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

    /// Execute a tool through this low-level primitive.
    ///
    /// The allowlist config is consulted first: a tool without an allowlist
    /// entry cannot run even if a registered server advertises it. Registered
    /// servers take precedence over the sandbox fallback when they expose
    /// the tool name.
    ///
    /// This method does not apply policy, principal, argument-size or approval
    /// checks; callers needing those guarantees must go through
    /// [`crate::McpGateway::execute_tool`].
    pub async fn execute(&self, tool: &str, args: Value) -> Result<Value> {
        let tool_config = self
            .allowlist
            .get_config(tool)
            .ok_or_else(|| anyhow::anyhow!("Tool config not found: {}", tool))?;

        let server = {
            let mut servers = self.servers.write().await;

            // Check liveness before routing. A registration whose backing
            // process has exited must be removed, otherwise its tools keep
            // being advertised and routed to a dead server forever. A
            // `try_wait()` error is handled the same way (fail closed).
            let mut dead: Vec<(String, Arc<RegisteredServer>)> = Vec::new();
            for (name, server) in servers.iter() {
                if server.liveness.is_dead().await {
                    dead.push((name.clone(), Arc::clone(server)));
                }
            }
            for (name, _) in &dead {
                servers.remove(name);
            }

            if let Some((name, _)) = dead.iter().find(|(_, server)| server.tools.contains(tool)) {
                return Err(client_error(format!(
                    "MCP server '{name}' backing tool '{tool}' is no longer running; \
                     its registration was removed"
                )));
            }

            servers
                .values()
                .find(|server| server.tools.contains(tool))
                .map(Arc::clone)
        };

        if let Some(server) = server {
            return call_registered_tool(&server, tool, args, &tool_config).await;
        }

        // Fall back to the native Rust handler (runtime-agnostic, ADR-0002).
        self.sandbox.execute(tool, args, &tool_config).await
    }

    /// Spawn an upstream MCP server and register it with the gateway.
    ///
    /// The command runs as a child process and speaks MCP over stdio.
    /// Registering an existing name replaces the previous server and
    /// terminates its child process. Discovered tools are not added to the
    /// allowlist automatically: operators must list each tool in
    /// `mcp_allowlist.toml` before it can be executed.
    ///
    /// A registration is rejected when any advertised tool name is already
    /// advertised by a *different* server (see [`Self::register_transport`]).
    /// Re-registering the same `name` keeps the explicit replacement
    /// semantics above: that server is the one being replaced, so its tools
    /// are not treated as a conflict.
    pub async fn register_mcp_server(&self, name: &str, command: Vec<String>) -> Result<()> {
        if name.trim().is_empty() {
            return Err(client_error("MCP server name must not be empty"));
        }
        let (program, args) = command
            .split_first()
            .ok_or_else(|| client_error("MCP server command must not be empty"))?;

        let mut cmd = Command::new(program);
        cmd.args(args);
        // P0-5: never hand the service's full environment (including
        // BWS_ACCESS_TOKEN from `bws run`) to an upstream MCP child. Keep
        // only the variables required to locate/run a normal subprocess.
        crate::subprocess::apply_minimal_env(&mut cmd);

        // Spawn the child ourselves so we retain a `Child` for `try_wait()`
        // liveness checks. `rmcp` does not expose the child held by its
        // `TokioChildProcess`, so the transport is built from the child's
        // stdio instead. `ChildHandle::drop` kills the child if registration
        // fails before it is stored.
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("Failed to spawn MCP server '{name}' ({program})"))?;

        let (stdin, stdout) = match (child.stdin.take(), child.stdout.take()) {
            (Some(stdin), Some(stdout)) => (stdin, stdout),
            _ => {
                let _ = child.start_kill();
                return Err(anyhow::anyhow!(
                    "MCP server '{name}' ({program}) did not expose stdio pipes"
                ));
            }
        };

        let liveness = ProcessLiveness::Child(Box::new(ChildHandle::new(child)));
        self.register_transport_inner(name, (stdout, stdin), liveness)
            .await
    }

    /// Connect an already-built transport as the server `name`.
    ///
    /// Split out of [`Self::register_mcp_server`] so tests can drive the
    /// same protocol path with an in-memory duplex instead of a child
    /// process. Both paths therefore share the duplicate-tool-name check
    /// below: a name advertised by another registered server is rejected
    /// instead of silently making `execute` routing order-dependent.
    #[allow(dead_code)] // transport-only registration is used by tests
    pub(crate) async fn register_transport<T, E, A>(&self, name: &str, transport: T) -> Result<()>
    where
        T: IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        // In-memory transports have no OS process to monitor; tests that need
        // to exercise liveness call `register_transport_inner` directly.
        self.register_transport_inner(name, transport, ProcessLiveness::NoChild)
            .await
    }

    /// Connect `transport` as `name`, attaching `liveness` as the probe used
    /// by [`Self::execute`] to detect a dead backing process.
    async fn register_transport_inner<T, E, A>(
        &self,
        name: &str,
        transport: T,
        liveness: ProcessLiveness,
    ) -> Result<()>
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
        let tool_count = tool_names.len();

        // Hold the write lock across both the conflict check and the insert
        // so two concurrent registrations cannot both pass the check.
        let mut servers = self.servers.write().await;

        // `execute` resolves a tool with `servers.values().find(...)`. If two
        // different servers advertised the same tool name, routing would
        // depend on HashMap iteration order. Reject the newcomer so the
        // first owner stays authoritative. A re-registration under the same
        // `name` is excluded: that is an explicit replacement of a single
        // server and keeps its existing semantics.
        let mut conflicts: Vec<String> = Vec::new();
        for (existing_name, existing_server) in servers.iter() {
            if existing_name.as_str() == name {
                continue;
            }
            for tool in &tool_names {
                if existing_server.tools.contains(tool) {
                    conflicts.push(format!("'{tool}' -> '{existing_name}'"));
                }
            }
        }
        if !conflicts.is_empty() {
            conflicts.sort();
            return Err(client_error(format!(
                "MCP server '{name}' advertises tool name(s) already registered by another server: {}. \
                 Each tool name may be advertised by only one server; the first registration wins.",
                conflicts.join(", ")
            )));
        }

        let previous = servers.insert(
            name.to_string(),
            Arc::new(RegisteredServer {
                client,
                tools: tool_names,
                liveness,
            }),
        );

        if let Some(previous) = previous {
            // Replace semantics: kill the old child (if any) and drop the old
            // client to cancel its background task before it can be routed to.
            previous.liveness.kill();
            drop(previous);
            info!(server = name, "Replaced existing MCP server registration");
        }
        info!(server = name, tools = tool_count, "Registered MCP server");

        Ok(())
    }
}

/// Build a safe, non-echoing description of an upstream MCP error result.
///
/// The raw `CallToolResult` body can carry tokens, filesystem paths, URL query
/// strings or other server-supplied secrets. Embedding it verbatim in an error
/// leaks those values into API responses and logs. Return only a coarse
/// classification derived from structural metadata - never any string taken
/// from the body itself - so callers still learn that the tool failed and what
/// shape the result had, without a disclosure channel.
fn safe_error_summary(value: &Value) -> String {
    let content_blocks = value
        .get("content")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let has_structured = value.get("structuredContent").is_some();
    format!("{content_blocks} content block(s), structured payload = {has_structured}")
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
        // The payload came from the caller, so this is a 400-class mistake.
        other => {
            let message = format!("Tool '{tool}' arguments must be a JSON object, got: {other}");
            return Err(client_error(message));
        }
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
        // Never embed `value` (or any of its strings) in the error: upstream
        // bodies may contain secrets that would flow to callers and logs.
        let summary = safe_error_summary(&value);
        anyhow::bail!("MCP tool '{tool}' returned an error ({summary})");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::run_mock_mcp_server;
    use serde_json::json;

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

    /// Register `name` with a mock liveness probe instead of a real child
    /// process. The mock duplex keeps its lifetime inside this helper's
    /// caller: the server task owns `server_side`, while the rmcp client kept
    /// in `RegisteredServer` owns `client_side` through `register_transport_inner`.
    async fn register_with_mock_liveness(
        gateway: &Gateway,
        name: &str,
    ) -> Arc<std::sync::atomic::AtomicU8> {
        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_mock_mcp_server(server_side, "echo", false));
        let state = Arc::new(std::sync::atomic::AtomicU8::new(MOCK_LIVENESS_RUNNING));
        gateway
            .register_transport_inner(name, client_side, ProcessLiveness::Mock(Arc::clone(&state)))
            .await
            .expect("registration succeeds");
        state
    }

    #[tokio::test]
    async fn registered_server_tools_are_routed_to_the_mcp_client() {
        let gateway = test_gateway("routes").await;

        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_mock_mcp_server(server_side, "echo", false));

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
        tokio::spawn(run_mock_mcp_server(server_side, "echo", false));
        gateway
            .register_transport("mock", client_side)
            .await
            .unwrap();

        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_mock_mcp_server(server_side, "echo", false));
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

    /// Regression: once the process backing a registration is gone, the
    /// registration must disappear and the orphan tool must return a clear
    /// error instead of being routed to the dead server forever. The probe
    /// reports the same outcome a real `try_wait()` would; no subprocess is
    /// spawned in this test.
    #[tokio::test]
    async fn exited_child_process_registration_is_removed_and_reports_an_error() {
        let gateway = test_gateway("dead-child").await;
        let state = register_with_mock_liveness(&gateway, "mock").await;

        assert!(gateway.servers.read().await.contains_key("mock"));

        state.store(MOCK_LIVENESS_EXITED, std::sync::atomic::Ordering::SeqCst);

        let error = gateway
            .execute("echo", json!({ "message": "hi" }))
            .await
            .expect_err("a tool backed by a dead server fails");
        let message = error.to_string();
        assert!(message.contains("mock"), "names the server: {message}");
        assert!(
            message.contains("no longer running"),
            "explains the cause: {message}"
        );
        assert!(
            message.contains("registration was removed"),
            "states the cleanup: {message}"
        );
        assert!(
            !gateway.servers.read().await.contains_key("mock"),
            "orphan registration removed"
        );
    }

    /// A failed liveness probe (the `try_wait()` `Err` case) must also fail
    /// closed: treat the process as dead, drop the registration and error out
    /// rather than call a server whose state is unknown.
    #[tokio::test]
    async fn liveness_probe_error_removes_the_registration() {
        let gateway = test_gateway("liveness-error").await;
        let state = register_with_mock_liveness(&gateway, "mock").await;

        state.store(MOCK_LIVENESS_ERROR, std::sync::atomic::Ordering::SeqCst);

        let error = gateway
            .execute("echo", json!({}))
            .await
            .expect_err("an unknown process state is rejected");
        assert!(
            error.to_string().contains("no longer running"),
            "error: {error}"
        );
        assert!(
            !gateway.servers.read().await.contains_key("mock"),
            "registration removed on probe error"
        );
    }

    #[tokio::test]
    async fn registering_a_duplicate_tool_name_from_another_server_is_rejected() {
        let gateway = test_gateway("duplicate-tool").await;

        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_mock_mcp_server(server_side, "echo", false));
        gateway
            .register_transport("first", client_side)
            .await
            .expect("first registration succeeds");

        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_mock_mcp_server(server_side, "echo", false));
        let error = gateway
            .register_transport("second", client_side)
            .await
            .expect_err("duplicate tool name from another server is rejected");

        let message = error.to_string();
        assert!(message.contains("echo"), "names the tool: {message}");
        assert!(
            message.contains("first"),
            "names the owning server: {message}"
        );
        assert!(message.contains("only one server"), "uniqueness: {message}");

        {
            let servers = gateway.servers.read().await;
            assert!(servers.contains_key("first"), "first registration kept");
            assert!(!servers.contains_key("second"), "not inserted");
        }

        let result = gateway
            .execute("echo", json!({ "message": "hi" }))
            .await
            .expect("tool still routed to the first server");
        assert_eq!(result["content"][0]["text"], "pong");
    }

    #[tokio::test]
    async fn distinct_tool_names_from_different_servers_are_allowed() {
        let gateway = test_gateway("distinct-tools").await;

        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_mock_mcp_server(server_side, "echo", false));
        gateway
            .register_transport("alpha", client_side)
            .await
            .expect("first registration succeeds");

        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_mock_mcp_server(server_side, "ping", false));
        gateway
            .register_transport("beta", client_side)
            .await
            .expect("non-conflicting tool name is accepted");

        let servers = gateway.servers.read().await;
        assert!(servers.contains_key("alpha"));
        assert!(servers.contains_key("beta"));
    }

    #[tokio::test]
    async fn re_registering_a_name_conflicting_with_another_server_is_rejected() {
        let gateway = test_gateway("replace-conflict").await;

        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_mock_mcp_server(server_side, "echo", false));
        gateway
            .register_transport("owner", client_side)
            .await
            .expect("owner registration succeeds");

        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_mock_mcp_server(server_side, "ping", false));
        gateway
            .register_transport("other", client_side)
            .await
            .expect("other registration succeeds");

        // Re-registering "other" is normally an allowed replacement, but its
        // new tool name is owned by a different server, so it must be rejected.
        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_mock_mcp_server(server_side, "echo", false));
        gateway
            .register_transport("other", client_side)
            .await
            .expect_err("a replacement cannot steal another server's tool");

        let servers = gateway.servers.read().await;
        assert!(servers.contains_key("owner"));
        let other = servers.get("other").expect("original 'other' kept");
        assert!(other.tools.contains("ping"));
        assert!(!other.tools.contains("echo"));
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
        tokio::spawn(run_mock_mcp_server(server_side, "echo", true));
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

    /// A unit-level guard on the redaction helper: server-supplied strings
    /// (tokens, paths, URL queries, content types) must never appear in the
    /// summary, while the coarse classification is retained.
    #[test]
    fn mcp_error_summary_does_not_echo_server_supplied_text() {
        let probe_token = "FAKE-TOKEN-DO-NOT-USE";
        let body_text = format!(
            "auth failed token={probe_token} at /home/alice/.config/appdata https://example.invalid/cb?token={probe_token}&state=xyz"
        );
        let body = json!({
            "content": [{ "type": "private-type", "text": body_text }],
            "isError": true,
            "structuredContent": { "token": probe_token }
        });

        let summary = safe_error_summary(&body);
        assert!(!summary.contains(probe_token), "token leaked: {summary}");
        assert!(!summary.contains("/home/alice"), "path leaked: {summary}");
        assert!(!summary.contains("https://"), "URL leaked: {summary}");
        assert!(!summary.contains("private-type"), "type leaked: {summary}");
        assert!(
            summary.contains("1 content block"),
            "block count kept: {summary}"
        );
        assert!(
            summary.contains("structured payload = true"),
            "structured kept: {summary}"
        );
    }

    /// Integration guard: the error returned to callers must not carry the
    /// upstream body text or its raw JSON structure, only the redacted summary.
    #[tokio::test]
    async fn upstream_error_body_is_not_embedded_in_the_error_message() {
        let gateway = test_gateway("error-redaction").await;

        let (client_side, server_side) = tokio::io::duplex(8192);
        // `fail_calls = true` makes the mock answer with an error result whose
        // content text is "pong" (see `run_mock_mcp_server`).
        tokio::spawn(run_mock_mcp_server(server_side, "echo", true));
        gateway
            .register_transport("mock", client_side)
            .await
            .unwrap();

        let error = gateway
            .execute("echo", json!({ "message": "hi" }))
            .await
            .expect_err("MCP error results are surfaced");
        let message = error.to_string();

        assert!(
            message.contains("returned an error"),
            "error surfaced: {message}"
        );
        assert!(!message.contains("pong"), "body text leaked: {message}");
        assert!(
            !message.contains("isError"),
            "result JSON leaked: {message}"
        );
        assert!(
            !message.contains("\"content\""),
            "content key leaked: {message}"
        );
    }

    /// `Gateway::new` must not spawn configured upstreams by itself: startup
    /// registration belongs to `main.rs`, and runtime registration via RPC
    /// remains the only other way to add a server.
    #[tokio::test]
    async fn gateway_new_does_not_spawn_configured_upstreams() {
        let dir = std::env::temp_dir().join("jarvis-mcp-gw-config-upstreams");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let allowlist_path = dir.join("mcp_allowlist.toml");
        tokio::fs::write(&allowlist_path, ECHO_ALLOWLIST)
            .await
            .unwrap();

        let config = crate::config::McpGatewayConfig {
            allowlist_path: allowlist_path.to_string_lossy().into_owned(),
            upstreams: vec![crate::config::UpstreamConfig {
                name: "not-spawned".into(),
                // Deliberately invalid: if `new` spawned this command the
                // test would observe an unexpected child process.
                command: vec!["/definitely/not/a/real/mcp/binary".into()],
            }],
            ..Default::default()
        };
        let allowlist = Arc::new(crate::allowlist::AllowList::new(&config).await.unwrap());
        let sandbox = Arc::new(crate::sandbox::Sandbox::new(&config).await.unwrap());
        let gateway = Gateway::new(config, allowlist, sandbox).await.unwrap();

        assert!(gateway.servers.read().await.is_empty());
    }
}
