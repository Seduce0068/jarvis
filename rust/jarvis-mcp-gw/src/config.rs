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
    /// Comma-separated host patterns allowed as `web.fetch` destinations,
    /// e.g. `example.com,*.trusted.org`. Read from TOML `egress_allowlist` or
    /// `JARVIS_MCP_GW_EGRESS_ALLOWLIST` (Figment maps both to this field).
    ///
    /// There is no "unset means unrestricted" fallback: an empty, blank or
    /// malformed value normalizes to no hosts, and the sandbox denies every
    /// fetch (fail-closed). Only validated host patterns are logged at load
    /// time; credential-bearing config values are never emitted.
    #[serde(default)]
    pub egress_allowlist: String,
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
        // R4-FIX (a): a configured-but-blank admin token would let an empty
        // header/metadata match the configured value on both listeners. Fail
        // closed at startup so the gateway never comes up with an effectively
        // unauthenticated admin gate.
        config.validate_admin_token()?;
        config.warn_if_legacy_sandbox_settings_are_ignored();
        config.log_egress_allowlist();
        Ok(config)
    }

    /// Fail-closed startup validation of the optional admin token (R4-FIX a).
    ///
    /// A token that is present but blank (empty or whitespace-only) is
    /// rejected: both the gRPC `token_matches` and the HTTP
    /// `require_admin_token` gate would otherwise compare a blank presented
    /// value against a blank configured value and let any caller through.
    /// `None` (unset) stays valid and keeps the legacy loopback behavior.
    ///
    /// The error is a fixed literal: no configured token value is ever
    /// interpolated, so secrets cannot reach startup logs.
    pub fn validate_admin_token(&self) -> Result<()> {
        if self
            .admin_token
            .as_deref()
            .is_some_and(|token| token.trim().is_empty())
        {
            anyhow::bail!(
                "admin_token is set but blank; set JARVIS_MCP_GW_ADMIN_TOKEN \
                 to a non-empty secret or unset it before starting the gateway"
            );
        }
        Ok(())
    }

    /// Emit the effective egress allowlist at startup so an operator can see
    /// which hosts `web.fetch` may reach. Only normalized host patterns are
    /// logged; tokens and other secret config values are never included.
    fn log_egress_allowlist(&self) {
        match parse_egress_allowlist(&self.egress_allowlist) {
            Some(hosts) => tracing::info!(
                egress_allowlist = %hosts.join(","),
                host_count = hosts.len(),
                "web.fetch egress allowlist (effective)"
            ),
            None => tracing::warn!(
                "web.fetch egress allowlist is empty or invalid: every fetch is denied (fail-closed)"
            ),
        }
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

/// Normalize a comma-separated egress allowlist into comparable host patterns.
///
/// Both the TOML key and `JARVIS_MCP_GW_EGRESS_ALLOWLIST` supply this value as
/// a string. Blank input and any malformed entry return `None`, which the
/// sandbox enforces as deny-all; there is deliberately no unrestricted
/// fallback for a missing or broken configuration.
pub(crate) fn parse_egress_allowlist(raw: &str) -> Option<Vec<String>> {
    let mut hosts = Vec::new();
    for entry in raw.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if !is_valid_host_pattern(entry) {
            return None;
        }
        let normalized = entry.to_ascii_lowercase();
        if !hosts.contains(&normalized) {
            hosts.push(normalized);
        }
    }
    (!hosts.is_empty()).then_some(hosts)
}

/// A bare DNS name or `*.` followed by a DNS name. Schemes, ports, paths,
/// whitespace and wildcard forms other than a leading `*.` are rejected so a
/// malformed entry cannot silently widen the allowlist.
fn is_valid_host_pattern(entry: &str) -> bool {
    let host = entry.strip_prefix("*.").unwrap_or(entry);
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
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
            // Unset means deny-all in the fetch path, never unrestricted.
            egress_allowlist: String::new(),
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

    /// Unset remains the legacy compatibility case, and a real secret is
    /// accepted. Only a present-but-blank value is a startup error.
    #[test]
    fn validate_admin_token_accepts_unset_and_non_blank() {
        assert!(McpGatewayConfig::default().validate_admin_token().is_ok());
        let config = McpGatewayConfig {
            admin_token: Some("real-secret".to_owned()),
            ..Default::default()
        };
        assert!(config.validate_admin_token().is_ok());
    }

    /// A blank (empty or whitespace-only) configured token must abort startup.
    /// Non-breaking spaces are whitespace to `trim()` but visually distinct,
    /// so an accidental interpolation into the error would be detectable.
    #[test]
    fn validate_admin_token_rejects_blank_and_never_echoes_value() {
        for token in ["", "   ", "\t\n", "\u{00a0}\u{00a0}"] {
            let config = McpGatewayConfig {
                admin_token: Some(token.to_owned()),
                ..Default::default()
            };
            let message = config
                .validate_admin_token()
                .expect_err("blank configured token must be rejected")
                .to_string();
            // All cases here are blank after trimming, so `contains(token)`
            // would be trivially true for "". The fixed message itself is
            // asserted above; it interpolates no configuration value.
            assert!(
                message.contains("admin_token is set but blank"),
                "{message}"
            );
        }
    }

    /// The real startup path (`load` -> `load_from_path`) must enforce the
    /// guard, not just an explicitly called helper.
    #[test]
    fn load_from_path_rejects_blank_admin_token() {
        let dir = std::env::temp_dir().join("jarvis-mcp-gw-config-blank-token");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("mcp_gateway.toml");
        std::fs::write(
            &path,
            format!("{}\nadmin_token = \"   \"\n", config_toml("none", "{}")),
        )
        .expect("write config");
        let error = McpGatewayConfig::load_from_path(path.to_str().expect("utf8 path"))
            .expect_err("blank configured token must abort startup");
        assert!(
            error.to_string().contains("admin_token is set but blank"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(&dir);
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

    /// The new key must be a first-class TOML field rather than an ignored
    /// unknown key, and its value must normalize to enforced host patterns.
    #[test]
    fn egress_allowlist_from_toml_is_honoured() {
        let toml = r#"
grpc_addr = "127.0.0.1:50052"
http_addr = "127.0.0.1:8082"
allowlist_path = "/etc/jarvis/mcp_allowlist.toml"
sandbox_type = "none"
sandbox_config = {}
policy_grpc_addr = "http://127.0.0.1:50051"
mcp_principal = "opencode"
egress_allowlist = "example.com, *.trusted.org"
"#;
        let config: McpGatewayConfig = toml::from_str(toml).expect("config parses");
        assert_eq!(config.egress_allowlist, "example.com, *.trusted.org");
        assert_eq!(
            parse_egress_allowlist(&config.egress_allowlist),
            Some(vec!["example.com".to_string(), "*.trusted.org".to_string()])
        );
    }

    /// Figment maps `JARVIS_MCP_GW_EGRESS_ALLOWLIST` to the same snake_case
    /// field; `Serialized` reproduces the env-style value shape without
    /// mutating process-wide environment variables.
    #[test]
    fn egress_allowlist_from_env_style_value_is_honoured() {
        let env_value = serde_json::json!({
            "grpc_addr": "127.0.0.1:50052",
            "http_addr": "127.0.0.1:8082",
            "allowlist_path": "/etc/jarvis/mcp_allowlist.toml",
            "sandbox_type": "none",
            "sandbox_config": {},
            "policy_grpc_addr": "http://127.0.0.1:50051",
            "mcp_principal": "opencode",
            "egress_allowlist": " EXAMPLE.com , *.Trusted.ORG ",
        });
        let config: McpGatewayConfig = Figment::from(Serialized::defaults(env_value))
            .extract()
            .expect("env-style value parses");
        assert_eq!(config.egress_allowlist, " EXAMPLE.com , *.Trusted.ORG ");
        assert_eq!(
            parse_egress_allowlist(&config.egress_allowlist),
            Some(vec!["example.com".to_string(), "*.trusted.org".to_string()])
        );
    }

    /// A legacy config without the key still parses, but the effective value
    /// is deny-all rather than the old silent unrestricted fallback.
    #[test]
    fn legacy_config_without_egress_allowlist_is_fail_closed() {
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
        assert_eq!(config.egress_allowlist, "");
        assert_eq!(parse_egress_allowlist(&config.egress_allowlist), None);
        assert_eq!(
            parse_egress_allowlist(&McpGatewayConfig::default().egress_allowlist),
            None
        );
    }

    #[test]
    fn egress_allowlist_empty_or_invalid_is_fail_closed() {
        for raw in ["", "   ", ",", " , , "] {
            assert_eq!(parse_egress_allowlist(raw), None, "{raw:?}");
        }
        for raw in [
            "http://example.com",
            "example.com:443",
            "foo bar",
            "*",
            "*.",
            "-bad.example",
            "bad..example",
            "*.*.example.com",
        ] {
            assert_eq!(parse_egress_allowlist(raw), None, "{raw:?}");
        }
    }

    /// Build a dotted host whose labels are all within the 63-char limit, so
    /// only the 253-char whole-host cap decides the boundary outcome.
    fn host_with_last_label(last_label_len: usize) -> String {
        let mut host = String::new();
        for len in [63, 63, 63, last_label_len] {
            if !host.is_empty() {
                host.push('.');
            }
            host.push_str(&"a".repeat(len));
        }
        host
    }

    /// Length boundaries: 253/63 accepted, 254/64 rejected. Each pair differs
    /// by exactly one character, so an off-by-one cannot hide.
    #[test]
    fn egress_allowlist_length_boundaries_flip_at_one_character() {
        let host_253 = host_with_last_label(61);
        let host_254 = host_with_last_label(62);
        assert_eq!(host_253.len(), 253);
        assert_eq!(host_254.len(), 254);
        assert_eq!(
            parse_egress_allowlist(&host_253),
            Some(vec![host_253.clone()])
        );
        assert_eq!(parse_egress_allowlist(&host_254), None);

        let label_63 = "a".repeat(63);
        let label_64 = "a".repeat(64);
        assert_eq!(
            parse_egress_allowlist(&label_63),
            Some(vec![label_63.clone()])
        );
        assert_eq!(parse_egress_allowlist(&label_64), None);
    }

    #[test]
    fn egress_allowlist_normalizes_case_and_dedupes() {
        assert_eq!(
            parse_egress_allowlist(" EXAMPLE.com, *.Trusted.ORG , example.com"),
            Some(vec!["example.com".to_string(), "*.trusted.org".to_string()])
        );
    }
}
