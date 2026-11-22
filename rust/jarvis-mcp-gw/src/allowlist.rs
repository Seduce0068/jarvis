//! Allowlist management for MCP tools

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use tokio::fs;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AllowListConfig {
    pub tools: Vec<AllowedTool>,
    pub principals: HashSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllowedTool {
    pub name: String,
    pub description: String,
    pub allowed_principals: Vec<String>,
    pub max_args_size: usize,
    pub timeout_ms: u64,
    pub requires_approval: bool,
}

#[derive(Debug)]
pub struct AllowList {
    tools: std::sync::RwLock<HashSet<String>>,
    tool_configs: std::sync::RwLock<std::collections::HashMap<String, AllowedTool>>,
    allowed_principals: std::sync::RwLock<HashSet<String>>,
}

impl AllowList {
    pub async fn new(config: &crate::config::McpGatewayConfig) -> Result<Self> {
        // Fail loudly on a missing or malformed allowlist: silently
        // defaulting to an empty allowlist would leave the gateway running
        // while denying every tool, and would hide configuration typos.
        let content = fs::read_to_string(&config.allowlist_path)
            .await
            .with_context(|| format!("failed to read allowlist: {}", config.allowlist_path))?;
        let allowlist_config: AllowListConfig = toml::from_str(&content)
            .with_context(|| format!("failed to parse allowlist: {}", config.allowlist_path))?;

        let mut tools = HashSet::new();
        let mut tool_configs = std::collections::HashMap::new();
        for tool in allowlist_config.tools {
            // Fail closed: an empty tool name would register `""` as a callable
            // tool (so `check("")` succeeded) and would collide with any other
            // unnamed entry in the name index. Reject the whole file at startup.
            if tool.name.is_empty() {
                bail!("allowlist contains a tool with an empty name");
            }
            // Fail closed: an empty principal grants the tool to an empty
            // identity and silently widens access. Reject the whole file rather
            // than keeping a tool that no real principal should be able to use.
            if tool
                .allowed_principals
                .iter()
                .any(|principal| principal.is_empty())
            {
                bail!(
                    "allowlist tool '{}' contains an empty allowed principal",
                    tool.name
                );
            }
            // Fail closed: a duplicate name would make `HashMap::insert`
            // silently overwrite the earlier entry (last wins), so a later
            // definition with a weaker `requires_approval` gate could replace
            // a stricter one. Reject the whole file at startup instead.
            if !tools.insert(tool.name.clone()) {
                bail!("allowlist contains a duplicate tool name: {}", tool.name);
            }
            tool_configs.insert(tool.name.clone(), tool);
        }

        Ok(Self {
            tools: std::sync::RwLock::new(tools),
            tool_configs: std::sync::RwLock::new(tool_configs),
            allowed_principals: std::sync::RwLock::new(allowlist_config.principals),
        })
    }

    pub fn check(&self, tool: &str) -> Result<()> {
        if !self.tools.read().unwrap().contains(tool) {
            return Err(crate::error::McpGatewayError::NotAllowed(tool.into()).into());
        }
        Ok(())
    }

    pub fn get_config(&self, tool: &str) -> Option<AllowedTool> {
        self.tool_configs.read().unwrap().get(tool).cloned()
    }

    pub fn is_principal_allowed(&self, principal: &str) -> bool {
        self.allowed_principals.read().unwrap().contains(principal)
    }

    /// Tools `principal` may call: the principal must be admitted, and each
    /// returned tool entry must grant the principal. Approval-required tools
    /// are listed as well — executing one routes through the human approval
    /// flow (see [`crate::approval`]). Sorted by name so tool listings are
    /// deterministic.
    pub fn tools_for_principal(&self, principal: &str) -> Vec<AllowedTool> {
        if !self.is_principal_allowed(principal) {
            return Vec::new();
        }
        let mut tools: Vec<AllowedTool> = self
            .tool_configs
            .read()
            .unwrap()
            .values()
            .filter(|tool| {
                tool.allowed_principals
                    .iter()
                    .any(|allowed| allowed == principal)
            })
            .cloned()
            .collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }

    /// Mutation helper retained for tests only. These change the live tool
    /// inventory and principal grants in memory without authentication or
    /// persistence, so they must not be reachable from production code or
    /// from downstream crates. `#[cfg(test)]` removes them from release
    /// builds entirely; keep the allowlist read-only at runtime and load
    /// changes through the config file / restart path instead.
    #[cfg(test)]
    pub fn add_tool(&self, tool: AllowedTool) {
        self.tools.write().unwrap().insert(tool.name.clone());
        self.tool_configs
            .write()
            .unwrap()
            .insert(tool.name.clone(), tool);
    }

    /// Test-only counterpart to `add_tool`. See its docs for why mutation is
    /// not part of the production API.
    #[cfg(test)]
    pub fn remove_tool(&self, tool: &str) {
        self.tools.write().unwrap().remove(tool);
        self.tool_configs.write().unwrap().remove(tool);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped allowlist must parse. A malformed file now aborts
    /// startup instead of silently yielding an empty allowlist, and TOML
    /// attaches any key after a table header to that table (which is how
    /// `principals` ended up inside the last `[[tools]]` entry before).
    #[test]
    fn shipped_allowlist_configuration_parses() {
        let shipped = include_str!("../../../infra/config/mcp_allowlist.toml");
        let config: AllowListConfig = toml::from_str(shipped).expect("shipped allowlist parses");

        assert!(
            config
                .tools
                .iter()
                .any(|tool| tool.name == "filesystem.read"),
            "expected the shipped tool entries to deserialize"
        );
        assert!(
            config.principals.contains("jarvis"),
            "expected the top-level principals list to deserialize"
        );
    }

    /// Build a throwaway allowlist TOML and a gateway config pointing at it.
    /// Production config files are never touched.
    async fn config_with_allowlist(
        label: &str,
        toml: &str,
    ) -> (std::path::PathBuf, crate::config::McpGatewayConfig) {
        let dir = std::env::temp_dir().join(format!("jarvis-mcp-gw-allowlist-{label}"));
        tokio::fs::create_dir_all(&dir)
            .await
            .expect("create temp dir");
        let path = dir.join("mcp_allowlist.toml");
        tokio::fs::write(&path, toml)
            .await
            .expect("write allowlist");
        let config = crate::config::McpGatewayConfig {
            allowlist_path: path.to_string_lossy().into_owned(),
            ..Default::default()
        };
        (dir, config)
    }

    /// Minimal single-tool allowlist with caller-controlled name/principals.
    fn tool_toml(name: &str, principals: &[&str]) -> String {
        let principals = principals
            .iter()
            .map(|principal| format!("\"{principal}\""))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            r#"
principals = ["jarvis"]

[[tools]]
name = "{name}"
description = "test tool"
allowed_principals = [{principals}]
max_args_size = 1
timeout_ms = 1
requires_approval = false
"#
        )
    }

    /// I75-C: an empty tool name used to be registered, making `check("")`
    /// succeed. Startup must now abort instead of admitting that identity.
    #[tokio::test]
    async fn new_rejects_empty_tool_name() {
        let toml = tool_toml("", &["jarvis"]);
        let (dir, config) = config_with_allowlist("empty-name", &toml).await;
        let error = AllowList::new(&config)
            .await
            .expect_err("empty tool name must abort startup");
        assert!(error.to_string().contains("empty name"), "{error}");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// I75-C: an empty string inside `allowed_principals` grants the tool to
    /// an empty identity; the whole allowlist must be rejected at startup.
    #[tokio::test]
    async fn new_rejects_empty_allowed_principal() {
        let toml = tool_toml("read.tool", &["jarvis", ""]);
        let (dir, config) = config_with_allowlist("empty-principal", &toml).await;
        let error = AllowList::new(&config)
            .await
            .expect_err("empty allowed principal must abort startup");
        assert!(
            error.to_string().contains("empty allowed principal"),
            "{error}"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// A duplicate name used to be silently overwritten by `HashMap::insert`
    /// (last wins), letting a later definition with a weaker approval gate
    /// replace a stricter one. Startup must abort instead.
    #[tokio::test]
    async fn new_rejects_duplicate_tool_name() {
        let toml = r#"
principals = ["jarvis"]

[[tools]]
name = "read.tool"
description = "first definition"
allowed_principals = ["jarvis"]
max_args_size = 1
timeout_ms = 1
requires_approval = true

[[tools]]
name = "read.tool"
description = "second definition"
allowed_principals = ["jarvis"]
max_args_size = 1
timeout_ms = 1
requires_approval = false
"#;
        let (dir, config) = config_with_allowlist("duplicate-name", toml).await;
        let error = AllowList::new(&config)
            .await
            .expect_err("duplicate tool name must abort startup");
        assert!(error.to_string().contains("duplicate tool name"), "{error}");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// I75-C: the new startup guards must not reject the shipped allowlist.
    /// Real tools stay callable while the empty-tool identity stays denied.
    #[tokio::test]
    async fn shipped_allowlist_still_loads_and_empty_tool_is_denied() {
        let shipped = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../infra/config/mcp_allowlist.toml");
        let config = crate::config::McpGatewayConfig {
            allowlist_path: shipped.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let allowlist = AllowList::new(&config)
            .await
            .expect("shipped allowlist still passes startup validation");
        assert!(allowlist.check("filesystem.read").is_ok());
        assert!(allowlist.check("").is_err());
    }

    #[test]
    fn tools_for_principal_filters_and_sorts() {
        let config: AllowListConfig = toml::from_str(
            r#"
principals = ["alice", "bob"]

[[tools]]
name = "b.tool"
description = "b"
allowed_principals = ["alice", "bob"]
max_args_size = 1
timeout_ms = 1
requires_approval = false

[[tools]]
name = "a.tool"
description = "a"
allowed_principals = ["alice"]
max_args_size = 1
timeout_ms = 1
requires_approval = false

[[tools]]
name = "c.tool"
description = "c"
allowed_principals = ["carol"]
max_args_size = 1
timeout_ms = 1
requires_approval = false
"#,
        )
        .expect("test allowlist parses");

        let mut tool_configs = std::collections::HashMap::new();
        for tool in config.tools {
            tool_configs.insert(tool.name.clone(), tool);
        }
        let allowlist = AllowList {
            tools: std::sync::RwLock::new(HashSet::new()),
            tool_configs: std::sync::RwLock::new(tool_configs),
            allowed_principals: std::sync::RwLock::new(config.principals),
        };

        let names = |principal: &str| -> Vec<String> {
            allowlist
                .tools_for_principal(principal)
                .into_iter()
                .map(|tool| tool.name)
                .collect()
        };

        assert_eq!(names("alice"), vec!["a.tool", "b.tool"]);
        assert_eq!(names("bob"), vec!["b.tool"]);
        // `carol` is granted tools but is not an admitted principal.
        assert!(names("carol").is_empty());
        assert!(names("nobody").is_empty());
    }

    /// The test-only mutation helpers must keep the name set and the config
    /// map in sync, since the read APIs consult both.
    #[test]
    fn add_and_remove_tool_update_both_indexes() {
        let allowlist = AllowList {
            tools: std::sync::RwLock::new(HashSet::new()),
            tool_configs: std::sync::RwLock::new(std::collections::HashMap::new()),
            allowed_principals: std::sync::RwLock::new(HashSet::new()),
        };
        let tool = AllowedTool {
            name: "new.tool".to_owned(),
            description: "new".to_owned(),
            allowed_principals: vec!["alice".to_owned()],
            max_args_size: 1,
            timeout_ms: 1,
            requires_approval: false,
        };

        allowlist.add_tool(tool);
        assert!(allowlist.check("new.tool").is_ok());
        assert!(allowlist.get_config("new.tool").is_some());

        allowlist.remove_tool("new.tool");
        assert!(allowlist.check("new.tool").is_err());
        assert!(allowlist.get_config("new.tool").is_none());
    }
}
