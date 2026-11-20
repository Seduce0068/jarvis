//! Gateway core - orchestrates MCP tool execution.
//!
//! Tool calls are served by two backends:
//!
//! 1. **Native Rust handlers** - read-only tools (`filesystem.*`, `git.*`,
//!    `web.fetch`) implemented in the gateway itself (ADR-0002/0003).
//! 2. **Registered MCP servers** - upstream MCP servers spawned as child
//!    processes and driven by the official `rmcp` client over stdio. Tools
//!    are discovered at registration time via `tools/list`; both their names
//!    and their declared input schemas are retained so the MCP `tools/list`
//!    surface can advertise the upstream contract instead of a generic one.
//!
//! When an upstream advertises a name that also has a native handler, the
//! upstream wins both at execution time and in schema advertisement. We chose
//! to align the two orders (rather than reject such registrations) because
//! collisions are legitimate upstream deployments and both decisions now flow
//! through [`resolve_target`], which makes any future drift impossible.
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
use rmcp::model::{CallToolRequestParam, PaginatedRequestParam};
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

/// Upper bound for the MCP `initialize` handshake while registering an
/// upstream server. Tool discovery is bounded separately below.
const SERVER_REGISTRATION_TIMEOUT: Duration = Duration::from_secs(15);

/// Upper bound for the `tools/list` discovery phase of registration.
///
/// `rmcp` 0.3.2's `Peer::list_all_tools` follows `next_cursor` until the server
/// stops returning one, so a slow or adversarial upstream can otherwise block
/// registration indefinitely. Discovery gets its own deadline; expiry aborts
/// the registration (fail closed) instead of leaving a half-registered server.
const TOOL_DISCOVERY_TIMEOUT: Duration = SERVER_REGISTRATION_TIMEOUT;

/// Maximum number of `tools/list` pages followed during discovery. A server
/// that keeps returning a fresh `next_cursor` would otherwise loop forever
/// (bounded only by the wall-clock timeout) and grow the tool vec without end.
const MAX_DISCOVERY_PAGES: usize = 64;

/// Maximum number of tools a single upstream may advertise. Bounds memory and
/// the size of the routing/schema maps even when a server paginates quickly.
const MAX_DISCOVERED_TOOLS: usize = 4096;

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

    /// Best-effort synchronous kill-and-reap for the drop/replacement paths,
    /// where an async lock cannot be awaited. `start_kill()` signals the OS
    /// process and `try_wait()` reaps it if it has already exited, so a failed
    /// or replaced registration does not leave a zombie. The command is
    /// spawned with `kill_on_drop(true)`: it covers the residual window in
    /// which `try_wait()` still reports `Ok(None)` (tokio reaps the child via
    /// its orphan queue once the `Child` is dropped). If the lock is briefly
    /// contended, the final `Drop` retries the kill once the probe releases it.
    fn kill(&self) {
        if let Ok(mut child) = self.child.try_lock() {
            let _ = child.start_kill();
            let _ = child.try_wait();
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
    /// Input schemas declared by the upstream server in its `tools/list`
    /// response, keyed by tool name. Kept alongside `tools` so the MCP
    /// `tools/list` surface can advertise the upstream contract verbatim
    /// instead of a permissive `additionalProperties: true` catch-all.
    input_schemas: HashMap<String, serde_json::Map<String, Value>>,
    /// Liveness probe for the backing process (if the server was spawned as
    /// a child). Consulted before every tool call.
    liveness: ProcessLiveness,
}

/// Backend that actually serves a tool, together with the schema that must be
/// advertised for it.
///
/// Both [`Gateway::execute`] (which backend runs `tools/call`) and the MCP
/// `tools/list` surface (via [`Gateway::upstream_input_schema`]) derive their
/// answer from [`resolve_target`]. This is intentional: an upstream that owns
/// a tool name is also the backend that executes it, so the shown contract
/// can never disagree with the code path that will validate the call.
struct ToolTarget<'a> {
    server: &'a Arc<RegisteredServer>,
    input_schema: Option<&'a serde_json::Map<String, Value>>,
}

/// Single source of truth for "which backend owns `tool`".
///
/// The lookup is upstream-first: a registered server advertising `tool` wins
/// over the native handler, exactly as execution routing has always done.
/// The declared schema is read from that *same* server, so schema selection
/// and routing are decided together here instead of being computed twice in
/// different modules. A native tool therefore contributes a schema only when
/// no upstream owns its name.
///
/// Registration-time conflict detection (see [`Gateway::register_transport_inner`])
/// guarantees at most one upstream can own a given name, so this lookup is
/// deterministic regardless of `HashMap` iteration order.
fn resolve_target<'a>(
    servers: &'a HashMap<String, Arc<RegisteredServer>>,
    tool: &str,
) -> Option<ToolTarget<'a>> {
    let server = servers
        .values()
        .find(|server| server.tools.contains(tool))?;
    Some(ToolTarget {
        server,
        input_schema: server.input_schemas.get(tool),
    })
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
    /// Backend ownership is decided by [`resolve_target`], shared with schema
    /// advertisement so `tools/list` never shows a native contract for a tool
    /// this method routes to an upstream server.
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

            // Upstream-first ownership comes from `resolve_target`, the same
            // helper the MCP `tools/list` surface uses to select the schema.
            // One decision point keeps the shown contract aligned with the
            // backend that actually runs the call.
            resolve_target(&servers, tool).map(|target| Arc::clone(target.server))
        };

        if let Some(server) = server {
            return call_registered_tool(&server, tool, args, &tool_config).await;
        }

        // Fall back to the native Rust handler (runtime-agnostic, ADR-0002).
        self.sandbox.execute(tool, args, &tool_config).await
    }

    /// Input schema clients must advertise for `tool`, captured from the
    /// `tools/list` response of the upstream server that owns the name.
    ///
    /// The schema comes from [`resolve_target`] -- the very same ownership
    /// decision [`Gateway::execute`] uses for routing -- so a tool executed by
    /// an upstream is never advertised with a native schema. Only tools owned
    /// by a registered server get an entry here; native tools are served by
    /// the hardcoded branch on the MCP surface and therefore return `None`.
    pub(crate) async fn upstream_input_schema(
        &self,
        tool: &str,
    ) -> Option<serde_json::Map<String, Value>> {
        let servers = self.servers.read().await;
        resolve_target(&servers, tool)?.input_schema.cloned()
    }

    /// Read-only snapshot of the upstream registrations this gateway actually
    /// holds, with names sorted so callers get a deterministic view.
    ///
    /// This is deliberately a *registration* view, not an allowlist inference:
    /// registering a server never widens the allowlist (see the module docs), so
    /// an upstream whose tools are absent from `mcp_allowlist.toml` still counts
    /// as present. Liveness is probed inside the same read lock and a dead
    /// backing process is reported as absent, so a partial loss among several
    /// configured upstreams becomes visible instead of hiding behind an
    /// unrelated any-match. No tool runs and the routing table is not mutated.
    pub(crate) async fn registered_server_names(&self) -> Vec<String> {
        let servers = self.servers.read().await;
        let mut names = Vec::with_capacity(servers.len());
        for (name, server) in servers.iter() {
            if !server.liveness.is_dead().await {
                names.push(name.clone());
            }
        }
        names.sort();
        names
    }

    /// Count of live registrations, derived from [`Self::registered_server_names`]
    /// so the count and the names can never disagree.
    pub(crate) async fn registered_server_count(&self) -> usize {
        self.registered_server_names().await.len()
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
        // Fail-closed backstop: tokio 1.x does not kill a child when its
        // `Child` handle is dropped. If registration aborts before the child
        // is stored (missing stdio pipes, handshake/discovery failure), the
        // synchronous `ChildHandle::kill()` reaps the common case via
        // `try_wait()`; `kill_on_drop(true)` covers the residual window where
        // the child has not exited yet, so no orphan is left behind.
        cmd.kill_on_drop(true);

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
                // Fail closed: without both pipes no stdio transport can be
                // built. Inspect the kill result (never discard it) and reap
                // with `try_wait()`; if signalling itself fails, surface that
                // too so a surviving child is never silent. Dropping `child`
                // afterwards is safe because the command has
                // `kill_on_drop(true)` as the residual-race backstop.
                if let Err(err) = child.start_kill() {
                    return Err(anyhow::anyhow!(
                        "MCP server '{name}' ({program}) did not expose stdio pipes; \
                         additionally failed to kill spawned child: {err}"
                    ));
                }
                let _ = child.try_wait();
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

        // `list_all_tools()` (rmcp 0.3.2) follows `next_cursor` with no bound,
        // so a slow upstream or one that keeps changing the cursor could block
        // registration forever. Drive pagination ourselves with an explicit
        // deadline plus page/tool caps. Any failure here aborts registration:
        // `client` is dropped (cancelling its background task and transport)
        // and `liveness` is dropped, killing the upstream child (fail closed).
        let peer = client.peer();
        let tools: Vec<rmcp::model::Tool> = tokio::time::timeout(TOOL_DISCOVERY_TIMEOUT, async {
            let mut tools: Vec<rmcp::model::Tool> = Vec::new();
            let mut cursor = None;
            for page in 0..MAX_DISCOVERY_PAGES {
                let result = peer
                    .list_tools(Some(PaginatedRequestParam { cursor }))
                    .await
                    .with_context(|| format!("tools/list page {page} failed"))?;
                // Check the incoming page against the cap *before* extending
                // `tools`: a single adversarial page with 4097+ tools must be
                // rejected without first allocating/copying the excess into
                // the accumulator (fail before the memory is committed).
                if tools.len().saturating_add(result.tools.len()) > MAX_DISCOVERED_TOOLS {
                    anyhow::bail!("tools/list exceeded {MAX_DISCOVERED_TOOLS} tools");
                }
                tools.extend(result.tools);
                cursor = result.next_cursor;
                if cursor.is_none() {
                    return Ok(tools);
                }
            }
            anyhow::bail!(
                "tools/list exceeded {MAX_DISCOVERY_PAGES} pages (cursor never terminated)"
            )
        })
        .await
        .with_context(|| format!("MCP server '{name}' tool discovery timed out"))?
        .with_context(|| format!("Failed to list tools from MCP server '{name}'"))?;

        // Retain both views of the discovery result: names drive routing,
        // duplicate detection and liveness-based cleanup, while the declared
        // input schemas are what clients need to validate calls against the
        // upstream contract. Discarding the schemas here was the root cause
        // of upstream tools being advertised with a generic catch-all schema.
        let tool_names: HashSet<String> = tools.iter().map(|tool| tool.name.to_string()).collect();
        let input_schemas: HashMap<String, serde_json::Map<String, Value>> = tools
            .iter()
            .map(|tool| (tool.name.to_string(), tool.input_schema.as_ref().clone()))
            .collect();
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
                input_schemas,
                liveness,
            }),
        );

        if let Some(previous) = previous {
            // Replace semantics: kill and reap the old child (if any), then
            // drop the old client to cancel its background task before it can
            // be routed to. `ProcessLiveness::kill` delegates to
            // `ChildHandle::kill`, which signals with `start_kill()` and reaps
            // with `try_wait()` so a replaced registration leaves no orphan.
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

/// Minimal MCP server advertising one tool with a caller-supplied input
/// schema. The shared `crate::testing::run_mock_mcp_server` hardcodes
/// `{ "type": "object" }`, so tests that need to verify schema propagation
/// use this fixture instead.
#[cfg(test)]
pub(crate) async fn run_mock_mcp_server_with_schema(
    stream: tokio::io::DuplexStream,
    tool_name: &str,
    schema: serde_json::Value,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut lines = BufReader::new(read_half).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let message: Value = serde_json::from_str(&line).expect("valid JSON-RPC line");
        let method = message["method"].as_str().unwrap_or_default();
        let id = message.get("id").cloned();
        let response = match method {
            "initialize" => Some(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "serverInfo": { "name": "schema-mock", "version": "0.1.0" }
                }
            })),
            "notifications/initialized" => None,
            "tools/list" => Some(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [{
                        "name": tool_name,
                        "description": "Declared-schema tool",
                        "inputSchema": schema.clone()
                    }]
                }
            })),
            "tools/call" => Some(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": "pong" }],
                    "isError": false
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

/// Test-only MCP server that always returns a fresh `next_cursor` for
/// `tools/list`. A discovery implementation without a page cap would loop
/// forever against this fixture.
#[cfg(test)]
async fn run_endless_cursor_mock(stream: tokio::io::DuplexStream) {
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut lines = BufReader::new(read_half).lines();
    let mut page = 0u64;
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
                    "serverInfo": { "name": "endless-mock", "version": "0.1.0" }
                }
            })),
            "notifications/initialized" => None,
            "tools/list" => {
                page += 1;
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "tools": [{
                            "name": format!("tool-{page}"),
                            "inputSchema": { "type": "object" }
                        }],
                        "nextCursor": format!("cursor-{page}")
                    }
                }))
            }
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

/// Test-only MCP server that advertises a caller-chosen number of tools,
/// paginated in fixed-size pages. Used by the `MAX_DISCOVERED_TOOLS` boundary
/// regressions (exactly 4096 accepted, 4097 rejected). Paging keeps each
/// JSON-RPC line small enough for the in-memory duplex buffer.
#[cfg(test)]
async fn run_bulk_tools_mock(stream: tokio::io::DuplexStream, count: usize) {
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    const PAGE_SIZE: usize = 512;
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
                    "serverInfo": { "name": "bulk-mock", "version": "0.1.0" }
                }
            })),
            "notifications/initialized" => None,
            "tools/list" => {
                let offset: usize = message["params"]["cursor"]
                    .as_str()
                    .and_then(|cursor| cursor.parse().ok())
                    .unwrap_or(0);
                let end = offset.saturating_add(PAGE_SIZE).min(count);
                let tools: Vec<Value> = (offset..end)
                    .map(|i| {
                        json!({
                            "name": format!("bulk.tool.{i}"),
                            "inputSchema": { "type": "object" }
                        })
                    })
                    .collect();
                let mut result = json!({ "tools": tools });
                if end < count {
                    result["nextCursor"] = json!(end.to_string());
                }
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result
                }))
            }
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

    /// Regression: registration must retain the upstream-declared input
    /// schema alongside the tool name instead of discarding it. This is the
    /// data the MCP `tools/list` surface reuses when advertising upstream
    /// tools.
    #[tokio::test]
    async fn registration_retains_upstream_input_schemas() {
        let gateway = test_gateway("schema-retention").await;

        let declared = json!({
            "type": "object",
            "properties": { "query": { "type": "string", "description": "search text" } },
            "required": ["query"],
            "additionalProperties": false
        });
        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_mock_mcp_server_with_schema(
            server_side,
            "search.query",
            declared.clone(),
        ));
        gateway
            .register_transport("mock", client_side)
            .await
            .expect("registration succeeds");

        let retained = gateway
            .upstream_input_schema("search.query")
            .await
            .expect("upstream schema retained");
        assert_eq!(Value::Object(retained), declared);

        assert!(
            gateway
                .upstream_input_schema("not.advertised")
                .await
                .is_none(),
            "an unadvertised tool has no retained schema"
        );
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

    /// I101-C (a): the read-only registration snapshot used by `/health` must
    /// drop an upstream whose backing process died, so a partial loss among
    /// several registered upstreams is visible instead of being hidden by an
    /// unrelated any-match. Distinct tool names keep the duplicate-name guard
    /// out of this fixture.
    #[tokio::test]
    async fn registered_server_names_excludes_dead_upstreams() {
        let gateway = test_gateway("health-snapshot").await;

        async fn register_named(
            gateway: &Gateway,
            name: &str,
            tool: &str,
        ) -> Arc<std::sync::atomic::AtomicU8> {
            let (client_side, server_side) = tokio::io::duplex(8192);
            let owned_tool = tool.to_string();
            tokio::spawn(async move {
                run_mock_mcp_server(server_side, &owned_tool, false).await;
            });
            let state = Arc::new(std::sync::atomic::AtomicU8::new(MOCK_LIVENESS_RUNNING));
            gateway
                .register_transport_inner(
                    name,
                    client_side,
                    ProcessLiveness::Mock(Arc::clone(&state)),
                )
                .await
                .expect("registration succeeds");
            state
        }

        let alive = register_named(&gateway, "alive", "echo").await;
        let dead = register_named(&gateway, "dead", "ping").await;
        assert_eq!(
            gateway.registered_server_names().await,
            vec!["alive", "dead"]
        );
        assert_eq!(gateway.registered_server_count().await, 2);

        dead.store(MOCK_LIVENESS_EXITED, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(gateway.registered_server_names().await, vec!["alive"]);
        assert_eq!(gateway.registered_server_count().await, 1);

        alive.store(MOCK_LIVENESS_EXITED, std::sync::atomic::Ordering::SeqCst);
        assert!(gateway.registered_server_names().await.is_empty());
        assert_eq!(gateway.registered_server_count().await, 0);
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

    /// (a) Regression: an upstream whose `tools/list` never terminates its
    /// cursor must be rejected quickly instead of hanging startup forever.
    /// The page cap (not the 15s wall-clock timeout) is what makes this fast.
    #[tokio::test]
    async fn endless_tools_list_cursor_fails_registration() {
        let gateway = test_gateway("endless-cursor").await;
        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_endless_cursor_mock(server_side));

        let error = tokio::time::timeout(
            Duration::from_secs(5),
            gateway.register_transport("endless", client_side),
        )
        .await
        .expect("registration must not hang")
        .expect_err("an unterminated cursor is rejected");
        // Alternate Display renders the whole anyhow context chain, including
        // the root cause added by the bounded-pagination helper.
        let message = format!("{error:#}");
        assert!(message.contains("tools/list"), "{message}");
        assert!(message.contains("pages"), "{message}");
        assert!(
            !gateway.servers.read().await.contains_key("endless"),
            "failed registration is not inserted"
        );
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

    /// (a) Regression: the cleanup a failed registration performs on its
    /// `liveness` handle must kill AND reap the spawned child, never leave an
    /// orphan/zombie. This drives the same `ChildHandle::kill()` synchronous
    /// helper that `Drop` and the replacement path call, with a real process,
    /// and checks the Linux `/proc` entry disappears (mirroring the sandbox
    /// `timeout_kills_and_reaps_child` regression).
    #[tokio::test]
    async fn registration_failure_child_is_killed_and_reaped() {
        let mut cmd = Command::new("sleep");
        crate::subprocess::apply_minimal_env(&mut cmd);
        cmd.arg("30").kill_on_drop(true);
        let child = cmd.spawn().expect("spawn sleep");
        let pid = child.id().expect("child has a pid");
        let handle = ChildHandle::new(child);
        let proc_path = std::path::PathBuf::from(format!("/proc/{pid}"));
        assert!(
            proc_path.exists(),
            "child {pid} must be alive before cleanup"
        );

        // This is exactly what a failed registration does with `liveness`:
        // the synchronous kill+reap helper runs (from Drop / replacement).
        handle.kill();

        // `start_kill()` is asynchronous, so re-drive the same `try_wait()`
        // the helper uses (via the liveness probe) until the killed child is
        // reaped and vanishes from /proc. Bounded so an orphan fails fast
        // instead of hanging the suite.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while proc_path.exists() && std::time::Instant::now() < deadline {
            let _ = handle.is_dead().await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        drop(handle);

        assert!(
            !proc_path.exists(),
            "child {pid} survived the failed-registration cleanup and was not reaped"
        );
    }

    /// (b) Boundary regression: a single upstream advertising exactly
    /// `MAX_DISCOVERED_TOOLS` (4096) tools must still register successfully.
    #[tokio::test]
    async fn discovery_accepts_exactly_max_discovered_tools() {
        let gateway = test_gateway("tools-4096").await;
        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_bulk_tools_mock(server_side, MAX_DISCOVERED_TOOLS));

        gateway
            .register_transport("bulk", client_side)
            .await
            .expect("exactly 4096 tools is within the cap");

        let servers = gateway.servers.read().await;
        let server = servers.get("bulk").expect("server registered");
        assert_eq!(server.tools.len(), MAX_DISCOVERED_TOOLS);
    }

    /// (b) Boundary regression: 4097 tools must be rejected before they are
    /// copied into the accumulator, and the failed registration must not be
    /// inserted into the routing table.
    #[tokio::test]
    async fn discovery_rejects_more_than_max_discovered_tools() {
        let gateway = test_gateway("tools-4097").await;
        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_bulk_tools_mock(server_side, MAX_DISCOVERED_TOOLS + 1));

        let error = gateway
            .register_transport("bulk", client_side)
            .await
            .expect_err("4097 tools exceeds the cap");
        let message = format!("{error:#}");
        assert!(message.contains("tools/list"), "{message}");
        assert!(
            message.contains(&MAX_DISCOVERED_TOOLS.to_string()),
            "names the cap: {message}"
        );
        assert!(
            !gateway.servers.read().await.contains_key("bulk"),
            "failed registration is not inserted"
        );
    }
}
