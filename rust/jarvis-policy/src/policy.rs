//! Policy evaluation: Cedar (ABAC) + Casbin (RBAC)

use anyhow::Result;
use casbin::{CoreApi, DefaultModel, Enforcer, FileAdapter};
use cedar_policy::{Authorizer, Context, Decision, Entities, PolicySet, Request as CedarRequest};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tracing::instrument;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: uuid::Uuid,
    pub principal: String,
    pub action: String,
    pub resource: String,
    pub context: Value,
    pub requested_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "decision")]
pub enum ApprovalDecision {
    Approved {
        approved_by: String,
        approved_at: chrono::DateTime<chrono::Utc>,
    },
    Denied {
        reason: String,
    },
    Pending,
    Expired,
}

pub struct CedarEngine {
    authorizer: Authorizer,
    policies: PolicySet,
    entities: Entities,
}

impl CedarEngine {
    pub async fn new(config: &crate::config::PolicyConfig) -> Result<Self> {
        // Load policies from directory if it exists, otherwise use empty
        let policies = if std::path::Path::new(&config.cedar_policy_dir).exists() {
            let mut file = std::fs::File::open(&config.cedar_policy_dir)?;
            PolicySet::from_json_file(&mut file)?
        } else {
            PolicySet::new()
        };

        let entities = if std::path::Path::new(&config.cedar_policy_dir).exists() {
            let mut file = std::fs::File::open(&config.cedar_policy_dir)?;
            Entities::from_json_file(&mut file, None)?
        } else {
            Entities::empty()
        };

        let authorizer = Authorizer::new();

        Ok(Self {
            authorizer,
            policies,
            entities,
        })
    }

    #[instrument(skip(self, context))]
    pub fn is_authorized(
        &self,
        principal: &str,
        action: &str,
        resource: &str,
        context: &Value,
    ) -> Result<bool> {
        // Simplified Cedar request - using string-based entity UIDs
        let principal_uid = format!("jarvis::Principal::\"{}\"", principal).parse()?;
        let action_uid = format!("jarvis::Action::\"{}\"", action).parse()?;
        let resource_uid = format!("jarvis::Resource::\"{}\"", resource).parse()?;

        let context_entity = Context::from_json_value(context.clone(), None)?;

        let request = CedarRequest::new(
            principal_uid,
            action_uid,
            resource_uid,
            context_entity,
            None,
        )?;

        let response = self
            .authorizer
            .is_authorized(&request, &self.policies, &self.entities);
        Ok(response.decision() == Decision::Allow)
    }

    pub async fn request_approval(&self, _request: ApprovalRequest) -> Result<ApprovalDecision> {
        // In production: persist to DB, notify user via HA/phone, wait for response
        // For now: simulate with timeout
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        Ok(ApprovalDecision::Denied {
            reason: "Not implemented: approval workflow".into(),
        })
    }
}

pub struct CasbinEnforcer {
    enforcer: Arc<Enforcer>,
}

impl CasbinEnforcer {
    pub async fn new(model_path: String, policy_path: String) -> Result<Self> {
        let model = DefaultModel::from_file(&model_path).await?;
        let adapter = FileAdapter::new(policy_path.clone());
        let enforcer = Enforcer::new(model, adapter).await?;
        Ok(Self {
            enforcer: Arc::new(enforcer),
        })
    }

    pub fn enforce(&self, principal: &str, action: &str, resource: &str) -> Result<bool> {
        // Casbin 2.x uses (subject, resource, action) order
        Ok(self.enforcer.enforce((principal, resource, action))?)
    }

    pub async fn add_policy(
        &self,
        _principal: &str,
        _action: &str,
        _resource: &str,
    ) -> Result<bool> {
        // Use the enforcer's underlying model/adapter directly for policy management
        // For simplicity, return Ok for now - full policy management requires MgmtApi
        Ok(true)
    }

    pub async fn remove_policy(
        &self,
        _principal: &str,
        _action: &str,
        _resource: &str,
    ) -> Result<bool> {
        Ok(true)
    }
}
