//! JARVIS Policy Engine
//! Cedar/Casbin-based authorization with approval gates and kill switch.

pub mod approval;
pub mod client;
pub mod config;
pub mod error;
pub mod grpc;
pub mod http;
pub mod kill_switch;
pub mod policy;

pub use client::PolicyClient;

use anyhow::Result;
use std::sync::Arc;

#[derive(Clone)]
#[allow(dead_code)]
pub struct PolicyEngine {
    config: config::PolicyConfig,
    cedar_engine: Arc<policy::CedarEngine>,
    casbin_enforcer: Arc<policy::CasbinEnforcer>,
    kill_switch: Arc<kill_switch::KillSwitch>,
    approvals: approval::ApprovalStore,
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
            config
                .kill_switch_state_path
                .as_ref()
                .map(std::path::PathBuf::from),
        ));
        if config.kill_switch_admin_token() == "changeme" {
            tracing::warn!(
                "Kill switch admin token is still the default 'changeme'; \
                 set JARVIS_POLICY_KILL_SWITCH_ADMIN_TOKEN"
            );
        }
        // Policy evaluation is always deny-by-default (Cedar/Casbin or the
        // kill switch). Reject an explicit opt-out so operators cannot be
        // misled into believing `default_deny = false` weakens the check.
        if !config.default_deny {
            tracing::warn!(
                "default_deny = false is ignored: authorization is always deny-by-default"
            );
        }
        let approvals = approval::ApprovalStore::with_max_pending(
            std::time::Duration::from_secs(config.approval_timeout_secs),
            config.approval_max_pending,
        );

        Ok(Self {
            config,
            cedar_engine,
            casbin_enforcer,
            kill_switch,
            approvals,
        })
    }

    /// Optional token required by the HTTP approval endpoints. `None` keeps
    /// the legacy unauthenticated (loopback-only) behavior.
    pub fn approval_api_token(&self) -> Option<&str> {
        self.config.approval_api_token.as_deref()
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
        if self
            .casbin_enforcer
            .enforce(principal, action, resource)
            .await?
        {
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
        Ok(self.approvals.request(request).await)
    }

    /// Approve a pending request (human-in-the-loop decision).
    pub async fn approve(
        &self,
        id: uuid::Uuid,
        approved_by: &str,
    ) -> std::result::Result<policy::ApprovalDecision, approval::ApprovalError> {
        // P0-4: the emergency stop must also block human decisions, not just
        // tool execution. Otherwise an operator could approve work that is
        // immediately denied, while the UI reports a successful decision.
        if self.kill_switch.is_activated() {
            return Err(approval::ApprovalError::KillSwitchActivated);
        }
        let decision = policy::ApprovalDecision::Approved {
            approved_by: approved_by.into(),
            approved_at: chrono::Utc::now(),
        };
        self.approvals.decide(id, decision.clone()).await?;
        Ok(decision)
    }

    /// Deny a pending request.
    pub async fn deny(
        &self,
        id: uuid::Uuid,
        reason: &str,
    ) -> std::result::Result<policy::ApprovalDecision, approval::ApprovalError> {
        if self.kill_switch.is_activated() {
            return Err(approval::ApprovalError::KillSwitchActivated);
        }
        let decision = policy::ApprovalDecision::Denied {
            reason: reason.into(),
        };
        self.approvals.decide(id, decision.clone()).await?;
        Ok(decision)
    }

    /// Current status of an approval request (`Pending` while undecided).
    pub async fn approval_status(
        &self,
        id: uuid::Uuid,
    ) -> std::result::Result<policy::ApprovalDecision, approval::ApprovalError> {
        self.approvals.status(id).await
    }

    /// All requests still awaiting a decision.
    pub async fn pending_approvals(&self) -> Vec<policy::ApprovalRequest> {
        self.approvals.list_pending().await
    }

    /// Activate the kill switch - denies all subsequent actions.
    ///
    /// Requires the admin token; an unauthenticated caller cannot stop the
    /// system.
    pub fn activate_kill_switch(&self, reason: String, admin_token: &str) -> Result<()> {
        self.kill_switch.activate_with_token(reason, admin_token)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jarvis-policy-lib-{label}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn shipped_casbin(label: &str) -> (String, String) {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../infra/config/policy");
        let dir = temp_dir(label);
        let model = dir.join("casbin_model.conf");
        let policy = dir.join("casbin_policy.csv");
        std::fs::copy(source.join("casbin_model.conf"), &model).expect("copy casbin model");
        std::fs::copy(source.join("casbin_policy.csv"), &policy).expect("copy casbin policy");
        (
            model.to_string_lossy().into_owned(),
            policy.to_string_lossy().into_owned(),
        )
    }

    async fn test_engine(label: &str) -> PolicyEngine {
        let (casbin_model_path, casbin_policy_path) = shipped_casbin(label);
        let config = config::PolicyConfig {
            cedar_policy_dir: temp_dir(label).join("cedar").to_string_lossy().into_owned(),
            casbin_model_path,
            casbin_policy_path,
            kill_switch_admin_token: "test-admin-token".into(),
            ..Default::default()
        };
        PolicyEngine::new(config)
            .await
            .expect("policy engine starts")
    }

    fn sample_request() -> policy::ApprovalRequest {
        let now = chrono::Utc::now();
        policy::ApprovalRequest {
            id: uuid::Uuid::new_v4(),
            principal: "jarvis".into(),
            action: "execute".into(),
            resource: "tool:filesystem.write".into(),
            context: serde_json::json!({}),
            requested_at: now,
            expires_at: now + chrono::Duration::seconds(60),
        }
    }

    /// Activating the kill switch must deny every action, including
    /// actions the shipped policy otherwise grants, and must deny
    /// approval requests without queueing them up. Deactivation requires
    /// the admin token.
    #[tokio::test]
    async fn kill_switch_denies_all_actions_until_deactivated() {
        let engine = test_engine("killswitch").await;
        let context = serde_json::json!({});

        // Baseline: the shipped policy grants this tool to the jarvis agent.
        assert!(
            engine
                .is_allowed("jarvis", "execute", "tool:filesystem.read", &context)
                .await
                .expect("baseline check runs")
        );

        // Unauthenticated activation must be rejected.
        assert!(
            engine
                .activate_kill_switch("test activation".into(), "wrong-token")
                .is_err()
        );
        assert!(!engine.kill_switch_status().activated);

        engine
            .activate_kill_switch("test activation".into(), "test-admin-token")
            .expect("admin token activates the switch");
        assert!(engine.kill_switch_status().activated);

        assert!(
            !engine
                .is_allowed("jarvis", "execute", "tool:filesystem.read", &context)
                .await
                .expect("check while activated runs")
        );

        let decision = engine
            .request_approval(sample_request())
            .await
            .expect("approval request runs");
        assert!(
            matches!(decision, policy::ApprovalDecision::Denied { .. }),
            "approvals are denied while the kill switch is active"
        );

        // Deactivation requires the configured admin token.
        assert!(engine.deactivate_kill_switch("wrong-token").is_err());
        assert!(engine.kill_switch_status().activated);
        engine
            .deactivate_kill_switch("test-admin-token")
            .expect("admin token deactivates the switch");
        assert!(
            engine
                .is_allowed("jarvis", "execute", "tool:filesystem.read", &context)
                .await
                .expect("check after deactivation runs")
        );

        std::fs::remove_dir_all(temp_dir("killswitch")).expect("clean temp dir");
    }

    #[tokio::test]
    async fn kill_switch_blocks_approval_decisions() {
        let engine = test_engine("killswitch-approval").await;
        let request = sample_request();
        let id = request.id;
        let engine_for_waiter = engine.clone();
        let waiter = tokio::spawn(async move { engine_for_waiter.request_approval(request).await });
        // Wait until the request is registered as pending.
        loop {
            if matches!(
                engine.approval_status(id).await,
                Ok(policy::ApprovalDecision::Pending)
            ) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        engine
            .activate_kill_switch("test".into(), "test-admin-token")
            .expect("activate");

        assert!(matches!(
            engine.approve(id, "koppe").await,
            Err(crate::approval::ApprovalError::KillSwitchActivated)
        ));
        assert!(matches!(
            engine.deny(id, "no").await,
            Err(crate::approval::ApprovalError::KillSwitchActivated)
        ));

        // The request is still pending and can be decided after deactivation.
        engine
            .deactivate_kill_switch("test-admin-token")
            .expect("deactivate");
        assert!(engine.approve(id, "koppe").await.is_ok());
        let _ = waiter.await.expect("waiter task");
    }
}
