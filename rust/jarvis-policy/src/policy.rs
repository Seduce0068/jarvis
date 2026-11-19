//! Policy evaluation: Cedar (ABAC) + Casbin (RBAC)

use anyhow::{Context as AnyhowContext, Result};
use casbin::{CoreApi, DefaultModel, Enforcer, FileAdapter, MgmtApi};
use cedar_policy::{
    Authorizer, Context, Decision, Entities, PolicySet, Request as CedarRequest, Schema,
    ValidationMode, Validator,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio::sync::RwLock;
use tracing::instrument;

/// Schema file shipped next to the Cedar policies. In the checkout this is
/// `infra/config/policy/cedar_schema.cedarschema`; the deploy scripts install
/// the same file as `/etc/jarvis/policy/cedar_schema.cedarschema`, i.e. next
/// to the configured `cedar_policy_dir`.
const CEDAR_SCHEMA_FILE: &str = "cedar_schema.cedarschema";

/// Parse a Cedar schema, attaching the source to any error.
fn parse_schema(text: &str, source: &Path) -> Result<Schema> {
    text.parse::<Schema>()
        .map_err(|err| anyhow::anyhow!("parsing Cedar schema from {}: {err}", source.display()))
}

/// Type-check `policies` against `schema` in strict mode.
///
/// This is the single validation gate used by both the production startup
/// path ([`CedarEngine::new`]) and the shipped-config regression tests, so the
/// enforced contract cannot drift from the tested one. A validation error is
/// fatal: callers propagate the error with `?` and startup fails (fail-closed).
fn validate_policies_against_schema(schema: Schema, policies: &PolicySet) -> Result<()> {
    let validator = Validator::new(schema);
    let result = validator.validate(policies, ValidationMode::Strict);
    if result.validation_passed() {
        return Ok(());
    }
    let errors: Vec<String> = result
        .validation_errors()
        .map(|err| err.to_string())
        .collect();
    anyhow::bail!(
        "Cedar policy validation failed ({} error(s)): {}",
        errors.len(),
        errors.join("; ")
    )
}

/// Locate the schema deployed alongside the configured Cedar policy source.
///
/// `None` means no schema is available, in which case startup keeps the
/// legacy "empty PolicySet / deny by default" behavior. A missing policy path
/// is tracked separately and is deliberately not turned into a schema
/// validation failure here.
fn schema_path_for(policy_path: &Path) -> Option<PathBuf> {
    let dir = if policy_path.is_dir() {
        Some(policy_path)
    } else if policy_path.is_file() {
        policy_path.parent()
    } else {
        None
    }?;
    let schema_path = dir.join(CEDAR_SCHEMA_FILE);
    schema_path.is_file().then_some(schema_path)
}

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

        // Type-check the policies that were actually loaded before the engine
        // can authorize anything. The schema is deployed next to the policy
        // source, so a missing schema means this installation has no schema
        // contract yet: keep the legacy deny-by-default startup rather than
        // failing (see the separate configuration-path finding).
        if let Some(schema_path) = schema_path_for(path) {
            tracing::info!(
                "Validating Cedar policies against schema {}",
                schema_path.display()
            );
            let schema_text = std::fs::read_to_string(&schema_path)
                .with_context(|| format!("reading Cedar schema {}", schema_path.display()))?;
            let schema = parse_schema(&schema_text, &schema_path)?;
            validate_policies_against_schema(schema, &policies)?;
        } else {
            tracing::info!(
                "Cedar schema {} not found next to {}; skipping schema validation",
                CEDAR_SCHEMA_FILE,
                path.display()
            );
        }

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
    enforcer: RwLock<Enforcer>,
}

impl CasbinEnforcer {
    pub async fn new(model_path: String, policy_path: String) -> Result<Self> {
        // FileAdapter fails to load a missing policy file; start from an empty
        // (deny-by-default) policy file instead.
        if !Path::new(&policy_path).exists() {
            std::fs::write(&policy_path, "")
                .with_context(|| format!("creating Casbin policy file {policy_path}"))?;
            tracing::info!("Created empty Casbin policy file {}", policy_path);
        }

        let model = DefaultModel::from_file(&model_path).await?;
        let adapter = FileAdapter::new(policy_path);
        let enforcer = Enforcer::new(model, adapter).await?;
        Ok(Self {
            enforcer: RwLock::new(enforcer),
        })
    }

    pub async fn enforce(&self, principal: &str, action: &str, resource: &str) -> Result<bool> {
        // Casbin 2.x uses (subject, resource, action) order
        Ok(self
            .enforcer
            .read()
            .await
            .enforce((principal, resource, action))?)
    }

    /// Add a policy rule and persist it to the policy file.
    ///
    /// Returns `false` when the rule already exists.
    pub async fn add_policy(&self, principal: &str, action: &str, resource: &str) -> Result<bool> {
        let mut enforcer = self.enforcer.write().await;
        if !enforcer
            .add_policy(casbin_rule(principal, action, resource))
            .await?
        {
            return Ok(false);
        }
        enforcer.save_policy().await?;
        Ok(true)
    }

    /// Remove a policy rule and persist the policy file.
    ///
    /// Returns `false` when the rule did not exist.
    pub async fn remove_policy(
        &self,
        principal: &str,
        action: &str,
        resource: &str,
    ) -> Result<bool> {
        let mut enforcer = self.enforcer.write().await;
        if !enforcer
            .remove_policy(casbin_rule(principal, action, resource))
            .await?
        {
            return Ok(false);
        }
        enforcer.save_policy().await?;
        Ok(true)
    }
}

/// Map the public `(principal, action, resource)` order to Casbin's
/// `(subject, object, action)` request order.
fn casbin_rule(principal: &str, action: &str, resource: &str) -> Vec<String> {
    vec![
        principal.to_string(),
        resource.to_string(),
        action.to_string(),
    ]
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

    /// A schema sitting next to the policies must be enforced on the
    /// production startup path, not only in unit tests.
    #[tokio::test]
    async fn schema_violation_fails_startup() -> Result<()> {
        let dir = temp_dir("schema-invalid");
        fs::write(
            dir.join("policies.cedar"),
            r#"permit (principal == jarvis::Unknown::"ghost", action, resource);"#,
        )?;
        fs::write(
            dir.join(CEDAR_SCHEMA_FILE),
            include_str!("../../../infra/config/policy/cedar_schema.cedarschema"),
        )?;

        let result = CedarEngine::new(&config_for(&dir)).await;
        assert!(
            result.is_err(),
            "a policy violating the schema must fail startup (fail-closed)"
        );

        fs::remove_dir_all(&dir)?;
        Ok(())
    }

    /// The same startup path must still accept policies that satisfy the
    /// schema (the shipped config is covered by the dedicated test below).
    #[tokio::test]
    async fn schema_conforming_policy_starts() -> Result<()> {
        let dir = temp_dir("schema-valid");
        fs::write(dir.join("policies.cedar"), ADMIN_POLICY)?;
        fs::write(
            dir.join(CEDAR_SCHEMA_FILE),
            include_str!("../../../infra/config/policy/cedar_schema.cedarschema"),
        )?;

        let engine = CedarEngine::new(&config_for(&dir)).await?;
        assert!(engine.is_authorized("jarvis_admin", "execute", "tool:git.commit", &json!({}))?);

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

    #[test]
    fn shipped_cedar_schema_parses() {
        let text = include_str!("../../../infra/config/policy/cedar_schema.cedarschema");
        let parsed = parse_schema(text, Path::new("shipped cedar_schema.cedarschema"));
        assert!(
            parsed.is_ok(),
            "shipped cedar_schema.cedarschema must parse: {:?}",
            parsed.err().map(|err| err.to_string())
        );
    }

    /// The shipped policies must be well-typed against the shipped schema.
    /// `CedarEngine::new` now runs this same check at startup, so this test
    /// guards the shipped configuration against contract drift.
    #[test]
    fn shipped_policies_validate_against_shipped_schema() {
        let schema_text = include_str!("../../../infra/config/policy/cedar_schema.cedarschema");
        let schema = parse_schema(schema_text, Path::new("shipped cedar_schema.cedarschema"))
            .expect("schema parses (covered by shipped_cedar_schema_parses)");
        let policies_text = include_str!("../../../infra/config/policy/policies.cedar");
        let policies = policies_text
            .parse::<PolicySet>()
            .expect("policies parse (covered by shipped_policies_parse)");

        let result = validate_policies_against_schema(schema, &policies);
        assert!(
            result.is_ok(),
            "shipped policies must validate against the shipped schema: {:?}",
            result.err().map(|err| err.to_string())
        );
    }

    fn casbin_files(label: &str) -> (PathBuf, PathBuf) {
        let dir = temp_dir(label);
        let model = dir.join("casbin_model.conf");
        fs::write(
            &model,
            include_str!("../../../infra/config/policy/casbin_model.conf"),
        )
        .expect("write casbin model");
        (model, dir.join("casbin_policy.csv"))
    }

    fn path_string(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn casbin_add_and_remove_policy_round_trip() -> Result<()> {
        let (model, policy) = casbin_files("casbin-roundtrip");
        let enforcer = CasbinEnforcer::new(path_string(&model), path_string(&policy)).await?;

        assert!(!enforcer.enforce("alice", "execute", "tool:demo").await?);
        assert!(enforcer.add_policy("alice", "execute", "tool:demo").await?);
        assert!(enforcer.enforce("alice", "execute", "tool:demo").await?);
        // Adding the same rule twice must not report success.
        assert!(!enforcer.add_policy("alice", "execute", "tool:demo").await?);

        // A fresh enforcer reading the same files proves persistence.
        let reloaded = CasbinEnforcer::new(path_string(&model), path_string(&policy)).await?;
        assert!(reloaded.enforce("alice", "execute", "tool:demo").await?);

        assert!(
            reloaded
                .remove_policy("alice", "execute", "tool:demo")
                .await?
        );
        assert!(!reloaded.enforce("alice", "execute", "tool:demo").await?);
        // Removing a missing rule must not report success.
        assert!(
            !reloaded
                .remove_policy("alice", "execute", "tool:demo")
                .await?
        );

        let third = CasbinEnforcer::new(path_string(&model), path_string(&policy)).await?;
        assert!(!third.enforce("alice", "execute", "tool:demo").await?);

        fs::remove_dir_all(model.parent().expect("temp dir"))?;
        Ok(())
    }

    #[tokio::test]
    async fn casbin_creates_missing_policy_file() -> Result<()> {
        let (model, policy) = casbin_files("casbin-create");
        assert!(!policy.exists());

        let enforcer = CasbinEnforcer::new(path_string(&model), path_string(&policy)).await?;
        assert!(policy.exists());
        assert!(!enforcer.enforce("alice", "execute", "tool:demo").await?);

        fs::remove_dir_all(model.parent().expect("temp dir"))?;
        Ok(())
    }

    fn shipped_casbin_files(label: &str) -> (PathBuf, PathBuf) {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../infra/config/policy");
        let dir = temp_dir(label);
        let model = dir.join("casbin_model.conf");
        let policy = dir.join("casbin_policy.csv");
        fs::copy(source.join("casbin_model.conf"), &model).expect("copy casbin model");
        fs::copy(source.join("casbin_policy.csv"), &policy).expect("copy casbin policy");
        (model, policy)
    }

    #[tokio::test]
    async fn shipped_casbin_policy_loads_and_enforces() -> Result<()> {
        let (model, policy) = shipped_casbin_files("casbin-shipped");
        let enforcer = CasbinEnforcer::new(path_string(&model), path_string(&policy)).await?;

        // Role assignments resolve through g(): jarvis -> agent -> exact tools.
        assert!(
            enforcer
                .enforce("jarvis", "execute", "tool:filesystem.read")
                .await?
        );
        assert!(
            enforcer
                .enforce("jarvis", "execute", "tool:web.search")
                .await?
        );
        assert!(
            enforcer
                .enforce("jarvis_user", "execute", "tool:filesystem.write")
                .await?
        );
        // The agent role must not inherit the user role's write access.
        assert!(
            !enforcer
                .enforce("jarvis", "execute", "tool:filesystem.write")
                .await?
        );
        assert!(
            !enforcer
                .enforce("unknown", "execute", "tool:filesystem.read")
                .await?
        );

        // The opencode principal carries a dedicated role mirroring the
        // `opencode` grants in mcp_allowlist.toml; the gateway exposes the
        // role to OpenCode over MCP.
        assert!(
            enforcer
                .enforce("opencode", "execute", "tool:filesystem.write")
                .await?
        );
        assert!(
            enforcer
                .enforce("opencode", "execute", "tool:shell.execute")
                .await?
        );
        assert!(
            !enforcer
                .enforce("opencode", "execute", "tool:homeassistant.call_service")
                .await?
        );
        assert!(
            !enforcer
                .enforce("opencode", "execute", "tool:android.send_notification")
                .await?
        );

        // Wildcards are effective through keyMatch: admin and system cover all.
        assert!(
            enforcer
                .enforce("jarvis_admin", "execute", "shell.execute")
                .await?
        );
        assert!(
            enforcer
                .enforce("jarvis_system", "execute", "anything:at.all")
                .await?
        );
        // The killswitch principal stays deny-by-default; the kill switch is
        // enforced out-of-band by the policy engine.
        assert!(
            !enforcer
                .enforce("jarvis_killswitch", "execute", "tool:filesystem.read")
                .await?
        );

        fs::remove_dir_all(model.parent().expect("temp dir"))?;
        Ok(())
    }
}
