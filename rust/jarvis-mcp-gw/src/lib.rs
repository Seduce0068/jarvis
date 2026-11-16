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

#[cfg(test)]
mod testing;

use anyhow::Result;
use std::sync::Arc;

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
        let gateway = Arc::new(
            gateway::Gateway::new(config.clone(), allowlist.clone(), sandbox.clone()).await?,
        );
        let approval = Arc::new(approval::ApprovalClient::new(
            config.policy_http_addr.clone(),
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
        grpc::serve(self.clone(), addr).await
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
        // Policy first. Gateway capabilities use the canonical resource
        // form `tool:<name>` (see infra/config/policy/casbin_policy.csv).
        let resource = format!("tool:{tool}");
        // `is_allowed` fails closed (false on transport errors).
        let allowed = self
            .policy_client
            .is_allowed(principal, "execute", &resource, &args)
            .await;
        if !allowed {
            return Err(error::McpGatewayError::PolicyDenied {
                tool: tool.to_owned(),
                principal: principal.to_owned(),
            }
            .into());
        }

        // Allowlist: the tool must be listed, the principal must be
        // admitted, and the tool entry must grant the principal. The
        // allowlist remains the single source of truth for what may run.
        self.allowlist.check(tool)?;
        let tool_config = self
            .allowlist
            .get_config(tool)
            .ok_or_else(|| error::McpGatewayError::NotAllowed(tool.into()))?;
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
            use approval::ApprovalOutcome;
            match self
                .approval
                .request_approval(principal, "execute", &resource, &args)
                .await
            {
                Ok(ApprovalOutcome::Approved { approved_by, .. }) => {
                    tracing::info!(
                        tool,
                        principal,
                        approved_by,
                        "tool call approved by operator"
                    );
                }
                Ok(ApprovalOutcome::Denied { reason }) => {
                    return Err(error::McpGatewayError::ApprovalDenied {
                        tool: tool.to_owned(),
                        reason,
                    }
                    .into());
                }
                Ok(ApprovalOutcome::Expired) => {
                    return Err(error::McpGatewayError::ApprovalDenied {
                        tool: tool.to_owned(),
                        reason: "approval request expired".to_owned(),
                    }
                    .into());
                }
                Ok(outcome @ ApprovalOutcome::Pending) => {
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
                .is_allowed(principal, "execute", &resource, &args)
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
        let dir = std::env::temp_dir().join(format!("jarvis-mcp-gw-lib-{label}"));
        let _ = std::fs::remove_dir_all(&dir);
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
        let dir = std::env::temp_dir().join("jarvis-mcp-gw-lib-argcap");
        let _ = std::fs::remove_dir_all(&dir);
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
        let id = loop {
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
        };
        http.post(format!("{policy_http}/v1/approval/{id}/approve"))
            .json(&json!({ "approved_by": "koppe" }))
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
        let id = loop {
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
        };
        // Emergency stop engages while the operator is still deciding.
        engine
            .activate_kill_switch("test".into(), "test-admin-token")
            .expect("kill switch activates");
        http.post(format!("{policy_http}/v1/approval/{id}/approve"))
            .json(&json!({ "approved_by": "koppe" }))
            .send()
            .await
            .expect("approve");

        let error = call.await.expect("call task").expect_err("must be denied");
        assert!(
            error.to_string().contains("Policy denied"),
            "unexpected: {error}"
        );
    }
}
