//! JARVIS Policy Engine
//! Cedar/Casbin-based authorization with approval gates and kill switch.

pub mod config;
pub mod error;
pub mod grpc;
pub mod http;
pub mod kill_switch;
pub mod policy;

use anyhow::Result;
use std::sync::Arc;

#[derive(Clone)]
#[allow(dead_code)]
pub struct PolicyEngine {
    config: config::PolicyConfig,
    cedar_engine: Arc<policy::CedarEngine>,
    casbin_enforcer: Arc<policy::CasbinEnforcer>,
    kill_switch: Arc<kill_switch::KillSwitch>,
}

impl PolicyEngine {
    pub async fn new(config: config::PolicyConfig) -> Result<Self> {
        let cedar_engine = Arc::new(policy::CedarEngine::new(&config).await?);
        let casbin_enforcer = Arc::new(
            policy::CasbinEnforcer::new(
                config.casbin_model_path.clone(),
                config.casbin_policy_path.clone(),
            )
            .await?,
        );
        let kill_switch = Arc::new(kill_switch::KillSwitch::new(
            &config.kill_switch_admin_token,
        ));

        Ok(Self {
            config,
            cedar_engine,
            casbin_enforcer,
            kill_switch,
        })
    }

    /// Check if action is allowed (async, with Casbin fast path)
    pub async fn is_allowed(
        &self,
        principal: &str,
        action: &str,
        resource: &str,
        context: &serde_json::Value,
    ) -> Result<bool> {
        if self.kill_switch.is_activated() {
            tracing::warn!("Kill switch activated - denying all actions");
            return Ok(false);
        }

        // Fast path: Casbin for simple RBAC
        if self.casbin_enforcer.enforce(principal, action, resource)? {
            return Ok(true);
        }

        // Deep path: Cedar for ABAC with context
        self.cedar_engine
            .is_authorized(principal, action, resource, context)
    }

    /// Request approval for sensitive action (async, may wait for user)
    pub async fn request_approval(
        &self,
        request: policy::ApprovalRequest,
    ) -> Result<policy::ApprovalDecision> {
        if self.kill_switch.is_activated() {
            return Ok(policy::ApprovalDecision::Denied {
                reason: "Kill switch activated".into(),
            });
        }
        self.cedar_engine.request_approval(request).await
    }

    /// Activate kill switch - denies all subsequent actions
    pub fn activate_kill_switch(&self, reason: String) {
        self.kill_switch.activate(reason);
    }

    /// Deactivate kill switch (requires admin)
    pub fn deactivate_kill_switch(&self, admin_token: &str) -> Result<()> {
        self.kill_switch.deactivate(admin_token)
    }

    pub fn kill_switch_status(&self) -> kill_switch::KillSwitchStatus {
        self.kill_switch.status()
    }

    pub async fn serve_grpc(&self, addr: std::net::SocketAddr) -> Result<()> {
        grpc::serve(Arc::new(self.clone()), addr).await
    }

    pub async fn serve_http(&self, addr: std::net::SocketAddr) -> Result<()> {
        http::serve(Arc::new(self.clone()), addr).await
    }
}
