//! JARVIS MCP Gateway - Main library

pub mod allowlist;
pub mod approval;
pub mod config;
pub mod error;
pub mod gateway;
pub mod grpc;
pub mod http;
pub mod mcp_server;
pub mod sandbox;
pub mod subprocess;

#[cfg(test)]
mod testing;

use anyhow::Result;
use std::sync::Arc;

/// Fixed 410 body emitted by `jarvis-policy` when an approval decision window
/// elapsed. Aliased from `jarvis-core` so the gateway's fail-closed denial
/// reason cannot drift from the body the policy emits and the approval UI
/// verifies before surfacing a terminal `Expired` decision.
const APPROVAL_EXPIRED_BODY: &str = jarvis_core::APPROVAL_EXPIRED_BODY;

/// Build the Cedar authorization context from server-controlled,
/// allowlist-derived attributes only.
///
/// The caller's raw tool arguments are deliberately excluded: forwarding them
/// would let a caller forge ABAC attributes (e.g. inject a `principal` or
/// `requires_approval` field) and influence the policy decision. The shipped
/// policy (`infra/config/policy/policies.cedar`) conditions only on
/// `principal`/`action`/`resource`, so no argument-derived field is required
/// and no existing policy condition is broken by this restricted context.
fn policy_context(
    tool: &str,
    principal: &str,
    resource: &str,
    requires_approval: bool,
) -> serde_json::Value {
    serde_json::json!({
        "tool": tool,
        "principal": principal,
        "resource": resource,
        "requires_approval": requires_approval,
    })
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct McpGateway {
    config: config::McpGatewayConfig,
    gateway: Arc<gateway::Gateway>,
    allowlist: Arc<allowlist::AllowList>,
    sandbox: Arc<sandbox::Sandbox>,
    policy_client: Arc<jarvis_policy::PolicyClient>,
    approval: Arc<approval::ApprovalClient>,
}

impl McpGateway {
    pub async fn new(
        config: config::McpGatewayConfig,
        policy_client: Arc<jarvis_policy::PolicyClient>,
    ) -> Result<Self> {
        let allowlist = Arc::new(allowlist::AllowList::new(&config).await?);
        let sandbox = Arc::new(sandbox::Sandbox::new(&config).await?);
        let gateway = Arc::new(gateway::Gateway::new(allowlist.clone(), sandbox.clone()).await?);
        // Forward the configured approval API token so policy's
        // require_approval_api_token check does not fail closed.
        let approval = Arc::new(approval::ApprovalClient::new_with_token(
            config.policy_http_addr.clone(),
            config.approval_api_token.clone(),
        ));

        Ok(Self {
            config,
            gateway,
            allowlist,
            sandbox,
            policy_client,
            approval,
        })
    }

    pub async fn serve_grpc(&self, addr: std::net::SocketAddr) -> Result<()> {
        grpc::serve(self.clone(), addr, self.config.admin_token.clone()).await
    }

    pub async fn serve_http(&self, addr: std::net::SocketAddr) -> Result<()> {
        http::serve(Arc::new(self.clone()), addr).await
    }

    pub async fn execute_tool(
        &self,
        tool: &str,
        args: serde_json::Value,
        principal: &str,
    ) -> Result<serde_json::Value> {
        // P0-3 staged rollout: when enabled, the caller-supplied wire
        // principal (REST/gRPC) cannot influence any decision below. The
        // configured MCP principal becomes the effective identity. Shadowing
        // here keeps every later check, approval request and log consistent.
        let principal = if self.config.require_server_side_principal {
            self.config.mcp_principal.as_str()
        } else {
            principal
        };

        // Gateway capabilities use the canonical resource form `tool:<name>`
        // (see infra/config/policy/casbin_policy.csv).
        let resource = format!("tool:{tool}");

        // Build the Cedar context from server-controlled, allowlist-derived
        // attributes only. The caller's raw `args` are deliberately excluded:
        // forwarding them would let a caller forge ABAC attributes and sway
        // the decision. Fetch the allowlist entry first so `requires_approval`
        // comes from trusted configuration rather than the wire payload.
        let tool_config = self.allowlist.get_config(tool);
        let requires_approval = tool_config
            .as_ref()
            .map(|cfg| cfg.requires_approval)
            .unwrap_or(false);
        let context = policy_context(tool, principal, &resource, requires_approval);

        // `is_allowed` fails closed (false on transport errors).
        let allowed = self
            .policy_client
            .is_allowed(principal, "execute", &resource, &context)
            .await;
        if !allowed {
            return Err(error::McpGatewayError::PolicyDenied {
                tool: tool.to_owned(),
                principal: principal.to_owned(),
            }
            .into());
        }

        // Allowlist: the tool must be listed, the principal must be
        // admitted, and the tool entry must grant the principal. Effective
        // access is the intersection (AND) of the Cedar/policy decision above
        // and this allowlist grant -- the allowlist is a second independent
        // deny layer, not the single source of truth. The shipped
        // `mcp_allowlist.toml` and `casbin_policy.csv` intentionally carry
        // overlapping-but-not-identical grants (V-146); a grant present in
        // only one of them never authorizes execution.
        self.allowlist.check(tool)?;
        let tool_config =
            tool_config.ok_or_else(|| error::McpGatewayError::NotAllowed(tool.into()))?;
        if !self.allowlist.is_principal_allowed(principal)
            || !tool_config
                .allowed_principals
                .iter()
                .any(|allowed| allowed == principal)
        {
            return Err(error::McpGatewayError::PrincipalNotAllowed {
                principal: principal.to_owned(),
                tool: tool.to_owned(),
            }
            .into());
        }
        // Enforce the per-tool argument cap declared in the allowlist.
        // Without this, `max_args_size` was dead config and a caller could
        // push an arbitrarily large payload into a tool handler or an
        // approval request.
        let args_size = serde_json::to_string(&args)
            .map(|text| text.len())
            .unwrap_or(usize::MAX);
        if args_size > tool_config.max_args_size {
            return Err(error::McpGatewayError::ArgsTooLarge {
                tool: tool.to_owned(),
                max: tool_config.max_args_size,
                actual: args_size,
            }
            .into());
        }

        if tool_config.requires_approval {
            // Human-in-the-loop (MCP-1): the call blocks on the policy
            // approval workflow until an operator decides or the approval
            // window elapses. Any failure is a denial (fail-closed).
            use jarvis_core::approval::ApprovalDecision;
            match self
                .approval
                .request_approval(principal, "execute", &resource, &args)
                .await
            {
                Ok(ApprovalDecision::Approved { approved_by, .. }) => {
                    tracing::info!(
                        tool,
                        principal,
                        approved_by,
                        "tool call approved by operator"
                    );
                }
                Ok(ApprovalDecision::Denied { reason }) => {
                    return Err(error::McpGatewayError::ApprovalDenied {
                        tool: tool.to_owned(),
                        reason,
                    }
                    .into());
                }
                Ok(ApprovalDecision::Expired) => {
                    return Err(error::McpGatewayError::ApprovalDenied {
                        tool: tool.to_owned(),
                        reason: APPROVAL_EXPIRED_BODY.to_owned(),
                    }
                    .into());
                }
                Ok(outcome @ ApprovalDecision::Pending) => {
                    tracing::error!(tool, principal, ?outcome, "unexpected pending approval");
                    return Err(error::McpGatewayError::ApprovalRequired {
                        tool: tool.to_owned(),
                    }
                    .into());
                }
                Err(error) => {
                    tracing::error!(tool, principal, %error, "approval request failed; denying (fail-closed)");
                    return Err(error::McpGatewayError::ApprovalRequired {
                        tool: tool.to_owned(),
                    }
                    .into());
                }
            }
        }

        // Re-check policy after the human decision. The kill switch may have
        // been activated while the operator was deciding, and an approval
        // must never override the emergency stop (fail-closed).
        if tool_config.requires_approval
            && !self
                .policy_client
                .is_allowed(principal, "execute", &resource, &context)
                .await
        {
            return Err(error::McpGatewayError::PolicyDenied {
                tool: tool.to_owned(),
                principal: principal.to_owned(),
            }
            .into());
        }

        // Route to a registered MCP server or the sandbox fallback.
        self.gateway.execute(tool, args).await
    }

    pub async fn register_mcp_server(&self, name: &str, command: Vec<String>) -> Result<()> {
        self.gateway.register_mcp_server(name, command).await
    }

    /// Tools the given principal may call, as declared by the allowlist.
    pub fn tools_for_principal(&self, principal: &str) -> Vec<allowlist::AllowedTool> {
        self.allowlist.tools_for_principal(principal)
    }

    /// Principal identity used for calls that arrive over the MCP endpoint.
    pub fn mcp_principal(&self) -> &str {
        &self.config.mcp_principal
    }

    /// Test helper: connect an already-built transport as `name`, mirroring
    /// [`gateway::Gateway::register_transport`] (which the child-process
    /// path drives through `register_mcp_server`).
    #[cfg(test)]
    pub(crate) async fn register_transport<T, E, A>(&self, name: &str, transport: T) -> Result<()>
    where
        T: rmcp::transport::IntoTransport<rmcp::RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        self.gateway.register_transport(name, transport).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpGatewayConfig;
    use crate::testing::{
        run_mock_mcp_server, shipped_policy_client_with_engine, start_policy_http,
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// pid separates concurrent cargo test processes; the counter separates
    /// repeated calls in one process. Every helper gets a fresh directory, so
    /// setup can never remove a directory another process is using.
    fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
        let n = TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "jarvis-mcp-gw-lib-{tag}-{}-{n}",
            std::process::id()
        ))
    }

    #[test]
    fn policy_context_uses_only_server_derived_attributes() {
        let context = policy_context("shell.execute", "opencode", "tool:shell.execute", true);
        assert_eq!(
            context,
            json!({
                "tool": "shell.execute",
                "principal": "opencode",
                "resource": "tool:shell.execute",
                "requires_approval": true,
            })
        );
    }

    /// Cross-crate pin: the gateway's expired-approval denial reason stays an
    /// alias of the shared `jarvis-core` body, never a gateway-local literal.
    /// The independent wire fixture catches a one-sided wording change in core,
    /// policy, UI or the gateway instead of silently producing a reason no
    /// other crate recognizes. The 409 bodies must stay distinct so an expiry
    /// can never be confused with an already-decided or kill-switch outcome.
    #[test]
    fn approval_expired_reason_pins_shared_wire_body() {
        const EXPIRED_WIRE_BODY: &str = "approval request expired";

        assert_eq!(APPROVAL_EXPIRED_BODY, jarvis_core::APPROVAL_EXPIRED_BODY);
        assert_eq!(APPROVAL_EXPIRED_BODY, EXPIRED_WIRE_BODY);
        assert_ne!(APPROVAL_EXPIRED_BODY, jarvis_core::ALREADY_DECIDED_409_BODY);
        assert_ne!(APPROVAL_EXPIRED_BODY, jarvis_core::KILL_SWITCH_409_BODY);
    }

    const APPROVAL_ALLOWLIST: &str = r#"
principals = ["opencode"]

[[tools]]
name = "shell.execute"
description = "Execute a shell command"
allowed_principals = ["opencode"]
max_args_size = 1048576
timeout_ms = 5000
requires_approval = true
"#;

    async fn gateway_with(
        label: &str,
    ) -> (Arc<McpGateway>, Arc<jarvis_policy::PolicyEngine>, String) {
        let dir = unique_temp_dir(label);
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("create temp dir");
        let allowlist = dir.join("mcp_allowlist.toml");
        tokio::fs::write(&allowlist, APPROVAL_ALLOWLIST)
            .await
            .expect("write allowlist");
        let policy_http = start_policy_http(label).await;
        let (policy, engine) = shipped_policy_client_with_engine(label).await;
        let config = McpGatewayConfig {
            allowlist_path: allowlist.to_string_lossy().into_owned(),
            policy_http_addr: policy_http.clone(),
            ..Default::default()
        };
        let gateway = Arc::new(
            McpGateway::new(config, Arc::new(policy))
                .await
                .expect("gateway starts"),
        );
        (gateway, engine, policy_http)
    }

    #[tokio::test]
    async fn oversized_arguments_are_rejected_before_execution() {
        let dir = unique_temp_dir("argcap");
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("create temp dir");
        let allowlist = dir.join("mcp_allowlist.toml");
        // `shell.execute` is granted by the shipped policy to `opencode`,
        // so the size gate is the layer under test here.
        tokio::fs::write(
            &allowlist,
            r#"
principals = ["opencode"]
[[tools]]
name = "shell.execute"
description = "Execute a shell command"
allowed_principals = ["opencode"]
max_args_size = 16
timeout_ms = 5000
requires_approval = false
"#,
        )
        .await
        .expect("write allowlist");
        let (policy, _engine) = shipped_policy_client_with_engine("argcap").await;
        let gateway = McpGateway::new(
            McpGatewayConfig {
                allowlist_path: allowlist.to_string_lossy().into_owned(),
                ..Default::default()
            },
            Arc::new(policy),
        )
        .await
        .expect("gateway starts");

        let error = gateway
            .execute_tool(
                "shell.execute",
                json!({ "cmd": "a".repeat(100) }),
                "opencode",
            )
            .await
            .expect_err("oversized args must be rejected");
        assert!(error.to_string().contains("exceed"), "unexpected: {error}");
    }

    /// Gateway with a 16-byte per-tool argument cap and no approval step, so
    /// the size gate is the only layer under test.
    async fn gateway_with_arg_cap(label: &str) -> Arc<McpGateway> {
        let dir = unique_temp_dir(label);
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
description = "Execute a shell command"
allowed_principals = ["opencode"]
max_args_size = 16
timeout_ms = 5000
requires_approval = false
"#,
        )
        .await
        .expect("write allowlist");
        let (policy, _engine) = shipped_policy_client_with_engine(label).await;
        Arc::new(
            McpGateway::new(
                McpGatewayConfig {
                    allowlist_path: allowlist.to_string_lossy().into_owned(),
                    ..Default::default()
                },
                Arc::new(policy),
            )
            .await
            .expect("gateway starts"),
        )
    }

    #[tokio::test]
    async fn tc_s1_max_args_size_boundary() {
        let gateway = gateway_with_arg_cap("argcap-boundary").await;
        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_mock_mcp_server(server_side, "shell.execute", false));
        gateway
            .register_transport("mock", client_side)
            .await
            .expect("mock registers");

        // `{"a":""}` is 8 bytes; 7/8/9 padding chars give 15/16/17 bytes.
        for (padding, expected_len, allowed) in [(7, 15, true), (8, 16, true), (9, 17, false)] {
            let args = json!({ "a": "x".repeat(padding) });
            assert_eq!(
                serde_json::to_string(&args).expect("serialize").len(),
                expected_len,
                "fixture must be exactly {expected_len} bytes"
            );

            let result = gateway
                .execute_tool("shell.execute", args, "opencode")
                .await;
            if allowed {
                result.unwrap_or_else(|error| panic!("{expected_len} bytes must pass: {error}"));
            } else {
                let error = result.expect_err("17 bytes must be rejected");
                assert!(error.to_string().contains("exceed"), "unexpected: {error}");
            }
        }
    }

    #[tokio::test]
    async fn approved_tool_still_executes_when_policy_allows() {
        let (gateway, _engine, policy_http) = gateway_with("approved-ok").await;
        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_mock_mcp_server(server_side, "shell.execute", false));
        gateway
            .register_transport("mock", client_side)
            .await
            .expect("mock registers");

        let http = reqwest::Client::new();
        let call = tokio::spawn({
            let gateway = gateway.clone();
            async move {
                gateway
                    .execute_tool("shell.execute", json!({}), "opencode")
                    .await
            }
        });
        // Poll with a deadline: a policy endpoint that never reports Pending
        // must fail the test instead of hanging the whole test binary.
        let id = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let pending: serde_json::Value = http
                    .get(format!("{policy_http}/v1/approvals"))
                    .send()
                    .await
                    .expect("list")
                    .json()
                    .await
                    .expect("json");
                if let Some(entry) = pending["pending"].as_array().and_then(|v| v.first()) {
                    break entry["id"].as_str().expect("id").to_owned();
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("approval must become pending within 5s");
        http.post(format!("{policy_http}/v1/approval/{id}/approve"))
            .json(&json!({ "approved_by": "testuser" }))
            .send()
            .await
            .expect("approve");

        let result = call.await.expect("call task").expect("tool executes");
        assert_eq!(result["content"][0]["text"], "pong");
    }

    #[tokio::test]
    async fn kill_switch_after_approval_denies_execution() {
        let (gateway, engine, policy_http) = gateway_with("kill-after-approval").await;
        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_mock_mcp_server(server_side, "shell.execute", false));
        gateway
            .register_transport("mock", client_side)
            .await
            .expect("mock registers");

        let http = reqwest::Client::new();
        let call = tokio::spawn({
            let gateway = gateway.clone();
            async move {
                gateway
                    .execute_tool("shell.execute", json!({}), "opencode")
                    .await
            }
        });
        // Poll with a deadline: a policy endpoint that never reports Pending
        // must fail the test instead of hanging the whole test binary.
        let id = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let pending: serde_json::Value = http
                    .get(format!("{policy_http}/v1/approvals"))
                    .send()
                    .await
                    .expect("list")
                    .json()
                    .await
                    .expect("json");
                if let Some(entry) = pending["pending"].as_array().and_then(|v| v.first()) {
                    break entry["id"].as_str().expect("id").to_owned();
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("approval must become pending within 5s");
        // Emergency stop engages while the operator is still deciding.
        engine
            .activate_kill_switch("test".into(), "test-admin-token")
            .expect("kill switch activates");
        http.post(format!("{policy_http}/v1/approval/{id}/approve"))
            .json(&json!({ "approved_by": "testuser" }))
            .send()
            .await
            .expect("approve");

        let error = call.await.expect("call task").expect_err("must be denied");
        assert!(
            error.to_string().contains("Policy denied"),
            "unexpected: {error}"
        );
    }

    const GUARD_ALLOWLIST: &str = r#"
principals = ["opencode", "jarvis"]

[[tools]]
name = "filesystem.read"
description = "Read a file"
allowed_principals = ["opencode"]
max_args_size = 1048576
timeout_ms = 5000
requires_approval = false
"#;

    async fn gateway_with_principal_guard(label: &str, require: bool) -> Arc<McpGateway> {
        let dir = unique_temp_dir(label);
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("create temp dir");
        let allowlist = dir.join("mcp_allowlist.toml");
        tokio::fs::write(&allowlist, GUARD_ALLOWLIST)
            .await
            .expect("write allowlist");
        let (policy, _engine) = shipped_policy_client_with_engine(label).await;
        Arc::new(
            McpGateway::new(
                McpGatewayConfig {
                    allowlist_path: allowlist.to_string_lossy().into_owned(),
                    require_server_side_principal: require,
                    ..Default::default()
                },
                Arc::new(policy),
            )
            .await
            .expect("gateway starts"),
        )
    }

    async fn register_read_mock(gateway: &Arc<McpGateway>) {
        let (client_side, server_side) = tokio::io::duplex(8192);
        tokio::spawn(run_mock_mcp_server(server_side, "filesystem.read", false));
        gateway
            .register_transport("mock", client_side)
            .await
            .expect("mock registers");
    }

    #[tokio::test]
    async fn caller_principal_is_honoured_when_guard_is_off() {
        let gateway = gateway_with_principal_guard("guard-off", false).await;
        register_read_mock(&gateway).await;

        let error = gateway
            .execute_tool(
                "filesystem.read",
                json!({ "path": "/tmp/example" }),
                "jarvis",
            )
            .await
            .expect_err("the caller principal is still used when the guard is off");
        assert!(
            error.to_string().contains("Principal 'jarvis'"),
            "unexpected: {error}"
        );
    }

    #[tokio::test]
    async fn server_side_principal_guard_overrides_caller_principal() {
        let gateway = gateway_with_principal_guard("guard-on", true).await;
        register_read_mock(&gateway).await;

        // `jarvis` is in the allowlist's principals but is not granted this
        // tool; the guard must use the configured `opencode` identity instead.
        let result = gateway
            .execute_tool(
                "filesystem.read",
                json!({ "path": "/tmp/example" }),
                "jarvis",
            )
            .await
            .expect("configured MCP principal is enforced");
        assert_eq!(result["content"][0]["text"], "pong");
    }

    /// `/mcp` must keep attributing calls to the configured principal; the
    /// guard only overrides the wire-level field on execute_tool.
    #[tokio::test]
    async fn mcp_principal_stays_fixed_when_guard_is_enabled() {
        let gateway = gateway_with_principal_guard("guard-mcp", true).await;
        assert_eq!(gateway.mcp_principal(), "opencode");

        let app = crate::http::router(gateway);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("test server address");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("http server runs");
        });

        use rmcp::ServiceExt;
        use rmcp::transport::StreamableHttpClientTransport;
        use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
        let transport = StreamableHttpClientTransport::with_client(
            reqwest::Client::new(),
            StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp")),
        );
        let client = ().serve(transport).await.expect("MCP client connects");
        let tools = client
            .peer()
            .list_all_tools()
            .await
            .expect("tools/list succeeds");
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
        assert_eq!(
            names,
            vec!["filesystem.read"],
            "/mcp keeps using the configured principal even with the guard on"
        );
    }
}
