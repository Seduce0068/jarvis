//! Configuration for MCP Gateway

use anyhow::Result;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use serde::{Deserialize, Serialize};

/// Absolute default location of the gateway TOML. The former CWD-relative
/// `config/mcp_gateway.toml` was not found when started outside the repo root.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/jarvis/mcp_gateway.toml";

/// Environment variable that overrides [`DEFAULT_CONFIG_PATH`].
pub const CONFIG_PATH_ENV: &str = "JARVIS_MCP_GW_CONFIG_PATH";

const ENV_PREFIX: &str = "JARVIS_MCP_GW_";

/// Select the TOML path from an optional environment override. Kept pure so
/// both branches are testable without mutating the process environment
/// (edition 2024 makes `set_var` unsafe and parallel tests racy).
fn resolve_config_path(override_path: Option<String>) -> String {
    override_path
        .filter(|path| !path.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_string())
}

/// An upstream MCP server to spawn and register when the gateway starts.
///
/// Only the registration `name` and the child-process `command` are needed:
/// the server speaks MCP over stdio and its tools are discovered through
/// `tools/list` at registration time. The allowlist still decides which of
/// those tools may actually be executed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamConfig {
    /// Name used for registration (re-registering the same name replaces it).
    pub name: String,
    /// Program followed by its arguments, e.g. `["/usr/bin/mcp-fs", "--root", "/srv"]`.
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpGatewayConfig {
    pub grpc_addr: String,
    pub http_addr: String,
    pub allowlist_path: String,
    /// Legacy sandbox selector, parsed for backward compatibility only.
    ///
    /// It is not consulted by the execution path (ADR-0002): native handlers
    /// and registered upstream MCP servers run without a sandbox wrapper.
    /// [`McpGatewayConfig::load`] warns when a non-default value is set.
    pub sandbox_type: SandboxType,
    /// Legacy sandbox options, likewise parsed but never applied at runtime.
    /// See the `sandbox_type` field above and ADR-0002.
    pub sandbox_config: serde_json::Value,
    pub policy_grpc_addr: String,
    /// HTTP base URL of `jarvis-policy`, used for the human approval flow
    /// (`POST /v1/approval`). Approval stays HTTP-only by design so a
    /// long-running human decision never occupies a gRPC stream.
    /// Optional in config files: older deployments fall back to the default.
    #[serde(default = "default_policy_http_addr")]
    pub policy_http_addr: String,
    /// Optional token sent as X-Jarvis-Approval-Token to jarvis-policy.
    #[serde(default)]
    pub approval_api_token: Option<String>,
    /// Shared token required by the gRPC listener. When set, every RPC must
    /// send it in the `x-jarvis-grpc-token` metadata header. Administrative
    /// RPCs (`RegisterMcpServer`) stay fail-closed when it is unset; data-plane
    /// RPCs keep the legacy loopback behavior for compatibility.
    #[serde(default)]
    pub admin_token: Option<String>,
    /// Principal identity used for tool calls that arrive over the MCP
    /// server endpoint (`/mcp`). Calls still pass policy and allowlist
    /// checks; the client never chooses the principal.
    pub mcp_principal: String,
    /// Opt-in P0-3 guard: `execute_tool` ignores the caller-supplied wire
    /// principal and uses `mcp_principal` for every policy, allowlist,
    /// approval and log decision. Default false keeps the current behavior.
    #[serde(default)]
    pub require_server_side_principal: bool,
    /// Upstream MCP servers registered at startup.
    ///
    /// TOML uses `[[upstreams]]` blocks. From the environment the same value
    /// is supplied as a TOML-style array, e.g.
    /// `JARVIS_MCP_GW_UPSTREAMS='[{name="fs",command=["/usr/bin/mcp-fs"]}]'`.
    #[serde(default)]
    pub upstreams: Vec<UpstreamConfig>,
}

impl McpGatewayConfig {
    /// Load from the absolute [`DEFAULT_CONFIG_PATH`], or from the path in
    /// [`CONFIG_PATH_ENV`] when it is set and non-blank, with `JARVIS_MCP_GW_*`
    /// environment overrides applied on top.
    ///
    /// Figment parses environment values as TOML-style data, so structured
    /// fields such as `upstreams` are readable from both sources without any
    /// split of the flat snake_case keys used by the existing variables.
    pub fn load() -> Result<Self> {
        let path = resolve_config_path(std::env::var(CONFIG_PATH_ENV).ok());
        Self::load_from_path(&path)
    }

    /// Parse a config from an explicit TOML path. Split out from [`Self::load`]
    /// so path selection is testable without touching the process environment.
    fn load_from_path(path: &str) -> Result<Self> {
        let config: McpGatewayConfig = Figment::new()
            .merge(Toml::file(path))
            // Flat snake_case fields: do NOT use figment's Env split, which
            // would turn JARVIS_MCP_GW_GRPC_ADDR into the nested key `grpc.addr`.
            // The config-path variable only selects the file, so keep it out
            // of the field provider as well.
            .merge(Env::prefixed(ENV_PREFIX).ignore(&["config_path"]))
            .extract()?;
        config.warn_if_legacy_sandbox_settings_are_ignored();
        Ok(config)
    }

    /// True when legacy sandbox keys carry non-default values that an
    /// operator might mistake for an enforced sandbox boundary.
    fn has_effective_legacy_sandbox_settings(&self) -> bool {
        let type_is_set = !matches!(self.sandbox_type, SandboxType::None);
        let options_are_set = !self
            .sandbox_config
            .as_object()
            .is_some_and(|v| v.is_empty());
        type_is_set || options_are_set
    }

    /// Emit an operator-facing warning at load time: these keys are kept only
    /// for config compatibility and are not used by the execution path.
    fn warn_if_legacy_sandbox_settings_are_ignored(&self) {
        if self.has_effective_legacy_sandbox_settings() {
            tracing::warn!(
                sandbox_type = ?self.sandbox_type,
                "sandbox_type and sandbox_config are legacy keys: parsed for \
                 compatibility but not used by the execution path (ADR-0002). \
                 No sandbox boundary is enforced by these settings."
            );
        }
    }
}

fn default_policy_http_addr() -> String {
    "http://127.0.0.1:8081".to_string()
}

impl Default for McpGatewayConfig {
    fn default() -> Self {
        Self {
            grpc_addr: "127.0.0.1:50052".into(),
            http_addr: "127.0.0.1:8082".into(),
            allowlist_path: "/etc/jarvis/mcp_allowlist.toml".into(),
            sandbox_type: SandboxType::None,
            sandbox_config: serde_json::json!({}),
            policy_grpc_addr: "http://127.0.0.1:50051".into(),
            policy_http_addr: default_policy_http_addr(),
            approval_api_token: None,
            admin_token: None,
            mcp_principal: "opencode".into(),
            require_server_side_principal: false,
            upstreams: Vec::new(),
        }
    }
}

/// Sandbox type selected for the gateway.
///
/// Retained for configuration compatibility (`JARVIS_MCP_GW_SANDBOX_TYPE` /
/// `sandbox_type` in `mcp_gateway.toml` keep parsing). Execution is
/// runtime-agnostic (ADR-0002): native handlers and upstream MCP servers
/// serve all tools, and this value no longer selects an execution backend.
/// [`McpGatewayConfig::load`] warns when a non-default value is configured.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxType {
    None,
    Firejail,
    Bwrap,
    Docker,
    Gvisor,
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::providers::Serialized;
    use std::path::Path;

    /// Config files written before the MCP-1 approval flow existed do not
    /// contain `policy_http_addr`; loading them must still work (fail-open
    /// on config parsing, not on approval decisions).
    #[test]
    fn legacy_config_without_policy_http_addr_still_loads() {
        let toml = r#"
grpc_addr = "127.0.0.1:50052"
http_addr = "127.0.0.1:8082"
allowlist_path = "/etc/jarvis/mcp_allowlist.toml"
sandbox_type = "none"
sandbox_config = {}
policy_grpc_addr = "http://127.0.0.1:50051"
mcp_principal = "opencode"
"#;
        let config: McpGatewayConfig = toml::from_str(toml).expect("legacy config parses");
        assert_eq!(config.policy_http_addr, "http://127.0.0.1:8081");
    }

    /// Config files written before the approval token was introduced must
    /// still load (the token is optional, default None).
    #[test]
    fn legacy_config_without_approval_token_still_loads() {
        let toml = r#"
grpc_addr = "127.0.0.1:50052"
http_addr = "127.0.0.1:8082"
allowlist_path = "/etc/jarvis/mcp_allowlist.toml"
sandbox_type = "none"
sandbox_config = {}
policy_grpc_addr = "http://127.0.0.1:50051"
policy_http_addr = "http://127.0.0.1:8081"
mcp_principal = "opencode"
"#;
        let config: McpGatewayConfig = toml::from_str(toml).expect("legacy config parses");
        assert_eq!(config.approval_api_token, None);
    }

    /// Pre-auth configs have no admin_token; data-plane RPCs must stay usable
    /// while administrative RPCs are guarded separately in grpc.rs.
    #[test]
    fn legacy_config_without_admin_token_still_loads() {
        let toml = r#"
grpc_addr = "127.0.0.1:50052"
http_addr = "127.0.0.1:8082"
allowlist_path = "/etc/jarvis/mcp_allowlist.toml"
sandbox_type = "none"
sandbox_config = {}
policy_grpc_addr = "http://127.0.0.1:50051"
policy_http_addr = "http://127.0.0.1:8081"
mcp_principal = "opencode"
"#;
        let config: McpGatewayConfig = toml::from_str(toml).expect("legacy config parses");
        assert_eq!(config.admin_token, None);
    }

    #[test]
    fn explicit_admin_token_is_honoured() {
        let toml = r#"
grpc_addr = "127.0.0.1:50052"
http_addr = "127.0.0.1:8082"
allowlist_path = "/etc/jarvis/mcp_allowlist.toml"
sandbox_type = "none"
sandbox_config = {}
policy_grpc_addr = "http://127.0.0.1:50051"
policy_http_addr = "http://127.0.0.1:8081"
admin_token = "grpc-test-token"
mcp_principal = "opencode"
"#;
        let config: McpGatewayConfig = toml::from_str(toml).expect("config parses");
        assert_eq!(config.admin_token.as_deref(), Some("grpc-test-token"));
    }

    #[test]
    fn explicit_approval_token_is_honoured() {
        let toml = r#"
grpc_addr = "127.0.0.1:50052"
http_addr = "127.0.0.1:8082"
allowlist_path = "/etc/jarvis/mcp_allowlist.toml"
sandbox_type = "none"
sandbox_config = {}
policy_grpc_addr = "http://127.0.0.1:50051"
policy_http_addr = "http://127.0.0.1:8081"
approval_api_token = "test-token"
mcp_principal = "opencode"
"#;
        let config: McpGatewayConfig = toml::from_str(toml).expect("config parses");
        assert_eq!(config.approval_api_token.as_deref(), Some("test-token"));
    }

    #[test]
    fn explicit_policy_http_addr_is_honoured() {
        let toml = r#"
grpc_addr = "127.0.0.1:50052"
http_addr = "127.0.0.1:8082"
allowlist_path = "/etc/jarvis/mcp_allowlist.toml"
sandbox_type = "none"
sandbox_config = {}
policy_grpc_addr = "http://127.0.0.1:50051"
policy_http_addr = "http://127.0.0.1:18081"
mcp_principal = "opencode"
"#;
        let config: McpGatewayConfig = toml::from_str(toml).expect("config parses");
        assert_eq!(config.policy_http_addr, "http://127.0.0.1:18081");
    }

    /// Pre-P0-3 configs have no guard field; the legacy wire behavior must
    /// stay intact without any config change.
    #[test]
    fn server_side_principal_guard_defaults_to_false() {
        assert!(!McpGatewayConfig::default().require_server_side_principal);

        let toml = r#"
grpc_addr = "127.0.0.1:50052"
http_addr = "127.0.0.1:8082"
allowlist_path = "/etc/jarvis/mcp_allowlist.toml"
sandbox_type = "none"
sandbox_config = {}
policy_grpc_addr = "http://127.0.0.1:50051"
policy_http_addr = "http://127.0.0.1:8081"
mcp_principal = "opencode"
"#;
        let config: McpGatewayConfig = toml::from_str(toml).expect("legacy config parses");
        assert!(!config.require_server_side_principal);
    }

    #[test]
    fn explicit_server_side_principal_guard_is_honoured() {
        let toml = r#"
grpc_addr = "127.0.0.1:50052"
http_addr = "127.0.0.1:8082"
allowlist_path = "/etc/jarvis/mcp_allowlist.toml"
sandbox_type = "none"
sandbox_config = {}
policy_grpc_addr = "http://127.0.0.1:50051"
policy_http_addr = "http://127.0.0.1:8081"
mcp_principal = "opencode"
require_server_side_principal = true
"#;
        let config: McpGatewayConfig = toml::from_str(toml).expect("config parses");
        assert!(config.require_server_side_principal);
    }

    /// A legacy config (and `Default`) has no upstreams, so startup
    /// registration must be a no-op without a config migration.
    #[test]
    fn legacy_config_without_upstreams_still_loads() {
        let toml = r#"
grpc_addr = "127.0.0.1:50052"
http_addr = "127.0.0.1:8082"
allowlist_path = "/etc/jarvis/mcp_allowlist.toml"
sandbox_type = "none"
sandbox_config = {}
policy_grpc_addr = "http://127.0.0.1:50051"
policy_http_addr = "http://127.0.0.1:8081"
mcp_principal = "opencode"
"#;
        let config: McpGatewayConfig = toml::from_str(toml).expect("legacy config parses");
        assert!(config.upstreams.is_empty());
        assert!(McpGatewayConfig::default().upstreams.is_empty());
    }

    #[test]
    fn configured_upstreams_are_honoured_from_toml() {
        let toml = r#"
grpc_addr = "127.0.0.1:50052"
http_addr = "127.0.0.1:8082"
allowlist_path = "/etc/jarvis/mcp_allowlist.toml"
sandbox_type = "none"
sandbox_config = {}
policy_grpc_addr = "http://127.0.0.1:50051"
policy_http_addr = "http://127.0.0.1:8081"
mcp_principal = "opencode"

[[upstreams]]
name = "filesystem"
command = ["/usr/bin/mcp-fs", "--root", "/srv"]

[[upstreams]]
name = "git"
command = ["/usr/bin/mcp-git"]
"#;
        let config: McpGatewayConfig = toml::from_str(toml).expect("config parses");
        assert_eq!(config.upstreams.len(), 2);
        assert_eq!(config.upstreams[0].name, "filesystem");
        assert_eq!(
            config.upstreams[0].command,
            vec!["/usr/bin/mcp-fs", "--root", "/srv"]
        );
        assert_eq!(config.upstreams[1].name, "git");
        assert_eq!(config.upstreams[1].command, vec!["/usr/bin/mcp-git"]);
    }

    /// Figment's Env provider parses `JARVIS_MCP_GW_UPSTREAMS` as a TOML-style
    /// array of dicts. `Serialized` reproduces the resulting `Value` shape
    /// without mutating process-wide environment variables (racy under
    /// parallel tests, and unsafe in the 2024 edition).
    #[test]
    fn upstreams_are_deserialized_from_env_style_value() {
        let env_value = serde_json::json!({
            "grpc_addr": "127.0.0.1:50052",
            "http_addr": "127.0.0.1:8082",
            "allowlist_path": "/etc/jarvis/mcp_allowlist.toml",
            "sandbox_type": "none",
            "sandbox_config": {},
            "policy_grpc_addr": "http://127.0.0.1:50051",
            "mcp_principal": "opencode",
            "upstreams": [
                {"name": "filesystem", "command": ["/usr/bin/mcp-fs", "--root", "/srv"]},
                {"name": "git", "command": ["/usr/bin/mcp-git"]}
            ]
        });
        let config: McpGatewayConfig = Figment::from(Serialized::defaults(env_value))
            .extract()
            .expect("env-style value parses");
        assert_eq!(config.upstreams.len(), 2);
        assert_eq!(config.upstreams[0].name, "filesystem");
        assert_eq!(
            config.upstreams[0].command,
            vec!["/usr/bin/mcp-fs", "--root", "/srv"]
        );
        assert_eq!(config.upstreams[1].name, "git");
        assert_eq!(config.upstreams[1].command, vec!["/usr/bin/mcp-git"]);
    }

    /// Minimal config with configurable legacy sandbox keys, used by the
    /// compatibility tests below.
    fn config_toml(sandbox_type: &str, sandbox_config: &str) -> String {
        format!(
            r#"
grpc_addr = "127.0.0.1:50052"
http_addr = "127.0.0.1:8082"
allowlist_path = "/etc/jarvis/mcp_allowlist.toml"
sandbox_type = "{sandbox_type}"
sandbox_config = {sandbox_config}
policy_grpc_addr = "http://127.0.0.1:50051"
policy_http_addr = "http://127.0.0.1:8081"
mcp_principal = "opencode"
"#
        )
    }

    /// The service must find its config regardless of the launch directory;
    /// the former `config/mcp_gateway.toml` was CWD-relative.
    #[test]
    fn default_config_path_is_absolute() {
        assert!(Path::new(DEFAULT_CONFIG_PATH).is_absolute());
        assert_eq!(DEFAULT_CONFIG_PATH, "/etc/jarvis/mcp_gateway.toml");
    }

    #[test]
    fn config_path_override_is_honoured() {
        const OVERRIDE: &str = "/tmp/jarvis-mcp-gw-custom.toml";
        assert_eq!(resolve_config_path(Some(OVERRIDE.into())), OVERRIDE);
        assert_eq!(resolve_config_path(None), DEFAULT_CONFIG_PATH);
        assert_eq!(
            resolve_config_path(Some(String::new())),
            DEFAULT_CONFIG_PATH
        );
    }

    #[test]
    fn config_path_env_name_matches_prefix() {
        let field = CONFIG_PATH_ENV
            .strip_prefix(ENV_PREFIX)
            .expect("override variable uses the gateway env prefix")
            .to_ascii_lowercase();
        assert_eq!(field, "config_path");
    }

    /// Legacy sandbox keys must keep parsing for every previously supported
    /// value: they are inert at runtime but never rejected at config load.
    #[test]
    fn legacy_sandbox_types_still_parse() {
        for (value, is_default) in [
            ("none", true),
            ("firejail", false),
            ("bwrap", false),
            ("docker", false),
            ("gvisor", false),
        ] {
            let toml = config_toml(value, "{}");
            let config: McpGatewayConfig = toml::from_str(&toml)
                .unwrap_or_else(|err| panic!("sandbox_type '{value}' must parse: {err}"));
            assert_eq!(
                config.has_effective_legacy_sandbox_settings(),
                !is_default,
                "sandbox_type '{value}'"
            );
        }
    }

    /// Non-default legacy settings are detected so `load()` can warn that the
    /// execution path ignores them.
    #[test]
    fn explicit_sandbox_settings_are_detected_as_ignored() {
        let toml = config_toml("firejail", "{ seccomp = true }");
        let config: McpGatewayConfig = toml::from_str(&toml).expect("legacy config parses");
        assert!(matches!(config.sandbox_type, SandboxType::Firejail));
        assert!(config.has_effective_legacy_sandbox_settings());
    }
}
