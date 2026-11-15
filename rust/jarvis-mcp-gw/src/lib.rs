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
