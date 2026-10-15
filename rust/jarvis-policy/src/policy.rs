//! Policy evaluation: Cedar (ABAC) + Casbin (RBAC)

use anyhow::{Context as AnyhowContext, Result};
use casbin::{CoreApi, DefaultModel, Enforcer, FileAdapter};
use cedar_policy::{Authorizer, Context, Decision, Entities, PolicySet, Request as CedarRequest};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
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
        let path = Path::new(&config.cedar_policy_dir);
        let (policies, entities) = if path.is_dir() {
            load_from_directory(path)?
        } else if path.is_file() {
            load_from_file(path)?
        } else {
            tracing::info!(
                "Cedar policy path {} not found; starting with no policies (deny by default)",
                path.display()
            );
            (PolicySet::new(), Entities::empty())
        };

        tracing::info!("Loaded {} Cedar policies", policies.policies().count());

        Ok(Self {
            authorizer: Authorizer::new(),
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
}

/// Load every `*.cedar` file in `dir` (sorted) as a single policy set.
///
/// Entity data is read from `entities.json` in the same directory when present.
fn load_from_directory(dir: &Path) -> Result<(PolicySet, Entities)> {
    let mut cedar_files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("cedar")
        })
        .collect();
    cedar_files.sort();

    let mut combined = String::new();
    for file in &cedar_files {
        combined.push_str(
            &std::fs::read_to_string(file)
                .with_context(|| format!("reading Cedar policy file {}", file.display()))?,
        );
        combined.push('\n');
    }

    let policies = if combined.trim().is_empty() {
        tracing::warn!(
            "No *.cedar policy files found in {}; all Cedar deep-path checks will be denied",
            dir.display()
        );
        PolicySet::new()
    } else {
        combined
            .parse::<PolicySet>()
            .with_context(|| format!("parsing Cedar policies from {}", dir.display()))?
    };

    let entities_path = dir.join("entities.json");
    let entities = if entities_path.is_file() {
        Entities::from_json_file(std::fs::File::open(&entities_path)?, None)
            .with_context(|| format!("parsing Cedar entities from {}", entities_path.display()))?
    } else {
        Entities::empty()
    };

    Ok((policies, entities))
}

/// Legacy single-file loading: `.json` files are parsed as Cedar policy-set JSON,
/// any other file is parsed as Cedar policy text.
fn load_from_file(path: &Path) -> Result<(PolicySet, Entities)> {
    let policies = if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
        PolicySet::from_json_file(std::fs::File::open(path)?)
            .with_context(|| format!("parsing Cedar policy JSON from {}", path.display()))?
    } else {
        std::fs::read_to_string(path)?
            .parse::<PolicySet>()
            .with_context(|| format!("parsing Cedar policies from {}", path.display()))?
    };
    Ok((policies, Entities::empty()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jarvis-policy-test-{}-{}",
            label,
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn config_for(path: &Path) -> crate::config::PolicyConfig {
        crate::config::PolicyConfig {
            cedar_policy_dir: path.to_string_lossy().into_owned(),
            ..Default::default()
        }
    }

    const ADMIN_POLICY: &str =
        r#"permit (principal == jarvis::Principal::"jarvis_admin", action, resource);"#;

    #[tokio::test]
    async fn loads_policies_from_directory() -> Result<()> {
        let dir = temp_dir("load");
        fs::write(dir.join("policies.cedar"), ADMIN_POLICY)?;

        let engine = CedarEngine::new(&config_for(&dir)).await?;
        assert!(engine.is_authorized("jarvis_admin", "execute", "tool:git.commit", &json!({}))?);
        assert!(!engine.is_authorized("jarvis", "execute", "tool:git.commit", &json!({}))?);

        fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn loads_legacy_single_cedar_file() -> Result<()> {
        let dir = temp_dir("legacy");
        let file = dir.join("single.cedar");
        fs::write(&file, ADMIN_POLICY)?;

        let engine = CedarEngine::new(&config_for(&file)).await?;
        assert!(engine.is_authorized("jarvis_admin", "read", "resource", &json!({}))?);

        fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn entities_file_is_loaded_when_present() -> Result<()> {
        let dir = temp_dir("entities");
        fs::write(dir.join("policies.cedar"), ADMIN_POLICY)?;
        fs::write(dir.join("entities.json"), "[]")?;

        let engine = CedarEngine::new(&config_for(&dir)).await?;
        assert!(engine.is_authorized("jarvis_admin", "execute", "tool:git.commit", &json!({}))?);

        fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn missing_path_and_empty_directory_deny_all() -> Result<()> {
        let missing =
            std::env::temp_dir().join(format!("jarvis-policy-none-{}", uuid::Uuid::new_v4()));
        let engine = CedarEngine::new(&config_for(&missing)).await?;
        assert!(!engine.is_authorized("jarvis_admin", "execute", "tool:git.commit", &json!({}))?);

        let empty = temp_dir("empty");
        let engine = CedarEngine::new(&config_for(&empty)).await?;
        assert!(!engine.is_authorized("jarvis_admin", "execute", "tool:git.commit", &json!({}))?);

        fs::remove_dir_all(&empty)?;
        Ok(())
    }

    #[tokio::test]
    async fn invalid_policy_file_fails_startup() -> Result<()> {
        let dir = temp_dir("invalid");
        fs::write(dir.join("bad.cedar"), "this is not a cedar policy")?;

        let result = CedarEngine::new(&config_for(&dir)).await;
        assert!(result.is_err(), "invalid Cedar must fail loudly at startup");

        fs::remove_dir_all(&dir)?;
        Ok(())
    }

    #[test]
    fn shipped_policies_parse() {
        let text = include_str!("../../../infra/config/policy/policies.cedar");
        let parsed = text.parse::<PolicySet>();
        assert!(
            parsed.is_ok(),
            "shipped policies.cedar must parse: {:?}",
            parsed.err().map(|err| err.to_string())
        );
    }
}
