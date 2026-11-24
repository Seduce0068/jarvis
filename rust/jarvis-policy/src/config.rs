//! Configuration for Policy Engine

use anyhow::Result;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use serde::{Deserialize, Serialize};

/// Known placeholder kill-switch admin token. It is rejected at startup on
/// every path (blank tokens are rejected as well); it is not a usable secret
/// (see [`PolicyConfig::validate_kill_switch_admin_token`]).
pub const PLACEHOLDER_KILL_SWITCH_TOKEN: &str = "changeme";

/// Absolute default location of the policy TOML. The former CWD-relative
/// `config/policy.toml` was not found when the service was started outside
/// the repository root.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/jarvis/policy.toml";

/// Environment variable that overrides [`DEFAULT_CONFIG_PATH`].
pub const CONFIG_PATH_ENV: &str = "JARVIS_POLICY_CONFIG_PATH";

const ENV_PREFIX: &str = "JARVIS_POLICY_";

/// Select the TOML path from an optional environment override. Kept pure so
/// both branches are testable without mutating the process environment
/// (edition 2024 makes `set_var` unsafe and parallel tests racy).
fn resolve_config_path(override_path: Option<String>) -> String {
    override_path
        .filter(|path| !path.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_string())
}

/// Default human approval deadline, in seconds.
///
/// Aliased from [`jarvis_core::DEFAULT_APPROVAL_TIMEOUT_SECS`], the single
/// source of truth also used by the gateway's `DEFAULT_APPROVAL_WINDOW_SECS`.
/// The gateway derives `expires_at` from its window while this service expires
/// the request using `approval_timeout_secs`; a one-sided value change used to
/// silently drift the effective deadline. Keeping the alias here preserves the
/// existing `jarvis_policy::config::DEFAULT_APPROVAL_TIMEOUT_SECS` path while
/// making the agreement a compile-time fact instead of a comment.
pub const DEFAULT_APPROVAL_TIMEOUT_SECS: u64 = jarvis_core::DEFAULT_APPROVAL_TIMEOUT_SECS;

/// Unknown keys are rejected at load time: a one-character typo of an opt-in
/// guard such as `require_approval_api_token` must not silently leave it at
/// its default `false`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    pub grpc_addr: String,
    pub http_addr: String,
    pub cedar_policy_dir: String,
    pub casbin_model_path: String,
    pub casbin_policy_path: String,
    pub kill_switch_admin_token: String,
    /// Optional JSON file used to persist kill-switch state across restarts.
    /// Unset keeps the switch in memory (tests, ephemeral runs).
    pub kill_switch_state_path: Option<String>,
    /// Human approval deadline in seconds. Configure this to the same value
    /// as the gateway's `JARVIS_MCP_GW_APPROVAL_WINDOW_SECS`
    /// (see [`DEFAULT_APPROVAL_TIMEOUT_SECS`]); both sides describe one
    /// deadline and must be changed together.
    pub approval_timeout_secs: u64,
    /// Upper bound on simultaneously pending approval requests.
    #[serde(default = "default_approval_max_pending")]
    pub approval_max_pending: usize,
    /// Optional bearer token for the HTTP approval endpoints. When unset
    /// (the default), behavior is unchanged; set it before exposing the
    /// listener beyond loopback.
    #[serde(default)]
    pub approval_api_token: Option<String>,
    /// Optional separate token for the approve/deny routes only. When set,
    /// decisions must present this value in `X-Jarvis-Approval-Token` while
    /// request/list/status keep using `approval_api_token`. This prevents a
    /// caller holding the gateway API token from approving its own request.
    /// Unset (default) keeps the legacy single-token behavior.
    #[serde(default)]
    pub approval_admin_token: Option<String>,
    /// Opt-in P0-1 guard: when true, approval endpoints fail closed with 503
    /// while `approval_api_token` is unset. Default false keeps the legacy
    /// loopback-only, unauthenticated behavior.
    #[serde(default)]
    pub require_approval_api_token: bool,
    /// Legacy compatibility key. The placeholder kill-switch token is now
    /// rejected unconditionally, so this flag no longer gates that check; it
    /// stays parsed to avoid breaking older configs under
    /// `deny_unknown_fields`.
    #[serde(default)]
    pub require_kill_switch_admin_token: bool,
    /// Opt-in I8-E guard: when true, startup fails if
    /// `kill_switch_state_path` is unset (or blank). Without a durable state
    /// file an activated kill switch is lost on restart and automated actions
    /// resume. Default false keeps the legacy in-memory + warn behavior.
    #[serde(default)]
    pub require_kill_switch_state_path: bool,
    /// Optional PostgreSQL connection URL for durable approval persistence
    /// (I29-A, 案A). A non-blank URL makes `PolicyEngine::new` open the store
    /// via `PgApprovalPersistence::connect(pg_url).await?`; an unset/blank URL
    /// keeps the Noop/in-memory store (legacy behavior). With
    /// `require_approval_persistence` enabled, a missing URL fails startup
    /// closed through `validate_approval_persistence`. The URL itself is
    /// never logged or echoed into errors because it may embed credentials.
    #[serde(default)]
    pub pg_url: Option<String>,
    /// Opt-in guard: when true, startup fails closed if `pg_url` is unset (or
    /// blank), because approval decisions would otherwise be lost on restart.
    /// Default false keeps the legacy in-memory + warn-only behavior.
    #[serde(default)]
    pub require_approval_persistence: bool,
    /// Legacy compatibility key, parsed but intentionally inert: authorization
    /// is always deny-by-default and `PolicyEngine::new` warns if this is false.
    /// See `lib.rs` (`default_deny = false is ignored`) and the test
    /// `default_deny_is_kept_for_compatibility`. Do not read this as a switch.
    pub default_deny: bool,
}

fn default_approval_max_pending() -> usize {
    crate::approval::DEFAULT_MAX_PENDING
}

impl PolicyConfig {
    /// Load from the absolute [`DEFAULT_CONFIG_PATH`], or from the path in
    /// [`CONFIG_PATH_ENV`] when it is set and non-blank, with `JARVIS_POLICY_*`
    /// environment overrides applied on top.
    pub fn load() -> Result<Self> {
        let path = resolve_config_path(std::env::var(CONFIG_PATH_ENV).ok());
        Self::load_from_path(&path)
    }

    /// Parse a config from an explicit TOML path. Split out from [`Self::load`]
    /// so path selection is testable without touching the process environment.
    fn load_from_path(path: &str) -> Result<Self> {
        Self::load_from_path_with(path, Env::prefixed(ENV_PREFIX).ignore(&["config_path"]))
    }

    /// Same parse + startup-validation chain as [`Self::load_from_path`], with
    /// the override provider injected. Tests merge a `Serialized` value after
    /// the env provider so an ambient `JARVIS_POLICY_*` value cannot turn the
    /// fail-closed assertions into a vacuous pass, without mutating env vars.
    fn load_from_path_with<P: figment::Provider>(path: &str, overrides: P) -> Result<Self> {
        let config: PolicyConfig = Figment::new()
            .merge(Toml::file(path))
            // Flat snake_case fields: do NOT use figment's Env split, which
            // would turn JARVIS_POLICY_GRPC_ADDR into the nested key `grpc.addr`.
            // The config-path variable only selects the file, so keep it out
            // of the field provider as well.
            .merge(overrides)
            .extract()?;
        // I8-E fail-closed guard: enforce at load time so the real binary
        // aborts before any service starts when a durable state path is
        // required but missing.
        config.validate_kill_switch_state_path()?;
        config.validate_kill_switch_admin_token()?;
        // R4-A: a configured-but-blank approval token would let an empty
        // `X-Jarvis-Approval-Token` header match through `constant_time_eq`,
        // opening approve/deny to any caller. Fail closed at startup.
        config.validate_approval_tokens()?;
        config.validate_approval_persistence()?;
        if config
            .kill_switch_state_path
            .as_deref()
            .is_none_or(|path| path.trim().is_empty())
        {
            tracing::warn!(
                "kill_switch_state_path is unset; kill switch state is \
                 in-memory only and will not survive a restart"
            );
        }
        if config
            .pg_url
            .as_deref()
            .is_none_or(|url| url.trim().is_empty())
        {
            tracing::warn!(
                "pg_url is unset; approval requests and decisions are \
                 in-memory only and will not survive a restart"
            );
        }
        Ok(config)
    }
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            grpc_addr: "127.0.0.1:50051".into(),
            http_addr: "127.0.0.1:8081".into(),
            cedar_policy_dir: "/etc/jarvis/policy".into(),
            casbin_model_path: "/etc/jarvis/policy/casbin_model.conf".into(),
            casbin_policy_path: "/etc/jarvis/policy/casbin_policy.csv".into(),
            kill_switch_admin_token: PLACEHOLDER_KILL_SWITCH_TOKEN.into(),
            kill_switch_state_path: None,
            // Mirrored by jarvis-mcp-gw's APPROVAL_WINDOW_ENV default (300).
            approval_timeout_secs: DEFAULT_APPROVAL_TIMEOUT_SECS,
            approval_max_pending: default_approval_max_pending(),
            approval_api_token: None,
            approval_admin_token: None,
            require_approval_api_token: false,
            require_kill_switch_admin_token: false,
            require_kill_switch_state_path: false,
            pg_url: None,
            require_approval_persistence: false,
            default_deny: true,
        }
    }
}

impl PolicyConfig {
    pub fn kill_switch_admin_token(&self) -> &str {
        &self.kill_switch_admin_token
    }

    /// Fail-closed startup validation of the kill-switch admin token.
    ///
    /// A blank (unset or whitespace-only) token is rejected unconditionally:
    /// an empty admin token would make `KillSwitch::verify_admin_token("")`
    /// succeed, so any caller able to send an empty header could arm or
    /// disarm the emergency stop.
    ///
    /// The known [`PLACEHOLDER_KILL_SWITCH_TOKEN`] is now rejected on every
    /// path as well. Previously it was only rejected when
    /// `require_kill_switch_admin_token` was enabled, which left a default
    /// deployment controlling the emergency stop with a publicly known
    /// credential. The legacy flag is still parsed for compatibility but no
    /// longer gates this check.
    ///
    /// Both the file-based path (`load()` -> `load_from_path_with`) and the
    /// programmatic path (`PolicyEngine::new`) call this single validator.
    ///
    /// No configured secret is interpolated into either error: secrets must
    /// never reach logs. The errors name only the public
    /// [`PLACEHOLDER_KILL_SWITCH_TOKEN`] marker and the env override, not the
    /// configured value.
    pub fn validate_kill_switch_admin_token(&self) -> Result<()> {
        if self.kill_switch_admin_token.trim().is_empty() {
            anyhow::bail!(
                "kill_switch_admin_token is unset or blank; set \
                 JARVIS_POLICY_KILL_SWITCH_ADMIN_TOKEN to a non-empty secret \
                 before starting the policy service"
            );
        }
        if self.kill_switch_admin_token == PLACEHOLDER_KILL_SWITCH_TOKEN {
            anyhow::bail!(
                "kill switch admin token is still the known placeholder value \
                 '{}'; set JARVIS_POLICY_KILL_SWITCH_ADMIN_TOKEN to a real \
                 secret (require_kill_switch_admin_token is now unconditional)",
                PLACEHOLDER_KILL_SWITCH_TOKEN
            );
        }
        Ok(())
    }

    /// Fail-closed startup validation of the optional approval tokens (R4-A).
    ///
    /// A token that is present but blank (empty or whitespace-only) is
    /// rejected unconditionally: the HTTP `require_approval_token` gate would
    /// otherwise compare a blank presented header against a blank configured
    /// value and let any caller decide approval requests. `None` (unset) stays
    /// valid and keeps the legacy loopback-only behavior.
    ///
    /// Both errors are fixed literals: no configured token value is ever
    /// interpolated, so secrets cannot reach startup logs.
    pub fn validate_approval_tokens(&self) -> Result<()> {
        if self
            .approval_api_token
            .as_deref()
            .is_some_and(|token| token.trim().is_empty())
        {
            anyhow::bail!(
                "approval_api_token is set but blank; set \
                 JARVIS_POLICY_APPROVAL_API_TOKEN to a non-empty secret or \
                 unset it before starting the policy service"
            );
        }
        if self
            .approval_admin_token
            .as_deref()
            .is_some_and(|token| token.trim().is_empty())
        {
            anyhow::bail!(
                "approval_admin_token is set but blank; set \
                 JARVIS_POLICY_APPROVAL_ADMIN_TOKEN to a non-empty secret or \
                 unset it before starting the policy service"
            );
        }
        Ok(())
    }

    /// Fail-closed startup validation for the I8-E guard.
    ///
    /// When `require_kill_switch_state_path` is enabled, a missing or blank
    /// state path aborts startup: without a durable state file an activated
    /// kill switch is lost on restart and automated actions would resume.
    /// With the guard off this is a no-op, preserving the legacy in-memory
    /// behavior. Kept public so callers that build a `PolicyConfig`
    /// programmatically (rather than via [`Self::load`]) can enforce the same
    /// guard before constructing a `PolicyEngine`.
    pub fn validate_kill_switch_state_path(&self) -> Result<()> {
        let path_missing = self
            .kill_switch_state_path
            .as_deref()
            .is_none_or(|path| path.trim().is_empty());
        if self.require_kill_switch_state_path && path_missing {
            anyhow::bail!(
                "kill_switch_state_path is unset but \
                 require_kill_switch_state_path = true; set \
                 JARVIS_POLICY_KILL_SWITCH_STATE_PATH so an emergency stop \
                 survives a restart"
            );
        }
        Ok(())
    }

    /// Fail-closed startup validation for approval persistence (I29-A).
    ///
    /// When `require_approval_persistence` is enabled, a missing or blank
    /// `pg_url` aborts startup: decisions would otherwise be lost on restart
    /// while the operator believes they are durable. With the guard off this
    /// is a no-op, preserving the legacy in-memory + warn-only behavior.
    ///
    /// This only validates configuration: no connection is attempted here and
    /// the URL value is never interpolated into the error. The connection is
    /// opened later by `PolicyEngine::new` when a non-blank URL is configured.
    /// Kept public so callers that build a `PolicyConfig` programmatically can
    /// enforce the same guard before constructing a `PolicyEngine`.
    pub fn validate_approval_persistence(&self) -> Result<()> {
        let url_missing = self
            .pg_url
            .as_deref()
            .is_none_or(|url| url.trim().is_empty());
        if self.require_approval_persistence && url_missing {
            anyhow::bail!(
                "pg_url is unset but require_approval_persistence = true; set \
                 JARVIS_POLICY_PG_URL so approval decisions survive a restart"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::providers::{Format, Serialized};
    use std::path::Path;

    /// The approval timeout default is the shared value that
    /// `jarvis-mcp-gw` must mirror via `JARVIS_MCP_GW_APPROVAL_WINDOW_SECS`.
    #[test]
    fn approval_timeout_default_is_shared_300() {
        assert_eq!(DEFAULT_APPROVAL_TIMEOUT_SECS, 300);
        assert_eq!(PolicyConfig::default().approval_timeout_secs, 300);
    }

    /// Config files written before the pending-approval cap was introduced
    /// must still load (the MCP-1 approval flow depends on this).
    #[test]
    fn legacy_config_without_cap_uses_default() {
        let toml = format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
approval_timeout_secs = 300
default_deny = true
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        );
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("legacy config parses");
        assert_eq!(config.approval_max_pending, 100);
    }

    /// `default_deny` is kept for config compatibility; authorization is
    /// always deny-by-default regardless of this value.
    #[test]
    fn default_deny_is_kept_for_compatibility() {
        let toml = format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
approval_timeout_secs = 300
default_deny = false
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        );
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("parses");
        assert!(!config.default_deny); // parsed, but ignored by the engine
    }

    #[test]
    fn explicit_cap_is_honoured() {
        let toml = format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
approval_timeout_secs = 300
approval_max_pending = 7
default_deny = true
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        );
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("config parses");
        assert_eq!(config.approval_max_pending, 7);
    }

    /// The strict startup guard is opt-in: configs written before P0-10 must
    /// keep the legacy warn-only behavior without any config change.
    #[test]
    fn strict_kill_switch_guard_defaults_to_false() {
        assert!(!PolicyConfig::default().require_kill_switch_admin_token);

        let toml = format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
approval_timeout_secs = 300
default_deny = true
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        );
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("legacy config parses");
        assert!(!config.require_kill_switch_admin_token);
    }

    #[test]
    fn strict_kill_switch_guard_can_be_enabled() {
        let toml = format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
require_kill_switch_admin_token = true
approval_timeout_secs = 300
default_deny = true
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        );
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("config parses");
        assert!(config.require_kill_switch_admin_token);
    }

    /// `PolicyConfig::load()` reads the P0-10 flag via
    /// `Env::prefixed("JARVIS_POLICY_")`: figment strips the prefix and
    /// lowercases the remainder, so
    /// `JARVIS_POLICY_REQUIRE_KILL_SWITCH_ADMIN_TOKEN` becomes
    /// `require_kill_switch_admin_token`.
    ///
    /// Edition 2024 makes `std::env::set_var` unsafe, and mutating the process
    /// environment would race with other tests. The same provider chain is
    /// exercised here with the post-conversion key supplied by a `Serialize`
    /// provider, so no OS env var is read or written by this test.
    #[test]
    fn env_flag_name_maps_to_strict_guard_field() {
        const ENV_FLAG: &str = "JARVIS_POLICY_REQUIRE_KILL_SWITCH_ADMIN_TOKEN";

        let field = ENV_FLAG
            .strip_prefix(ENV_PREFIX)
            .expect("flag uses the policy env prefix")
            .to_ascii_lowercase();
        assert_eq!(field, "require_kill_switch_admin_token");

        let toml = format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
approval_timeout_secs = 300
default_deny = true
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        );
        // Mirrors `load()`: the env provider stays in the chain, while the
        // Serialized value is merged last so the assertion cannot depend on
        // any value set in the test runner's environment.
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .merge(Env::prefixed(ENV_PREFIX).only(&[field.as_str()]))
            .merge(Serialized::default(field.as_str(), true))
            .extract()
            .expect("env-style key merges into the strict guard field");
        assert!(config.require_kill_switch_admin_token);
    }

    /// The P0-1 approval-token guard is opt-in: configs written before it was
    /// introduced keep the loopback-only, unauthenticated behavior.
    #[test]
    fn approval_token_guard_defaults_to_false() {
        assert!(!PolicyConfig::default().require_approval_api_token);

        let toml = format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
approval_timeout_secs = 300
default_deny = true
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        );
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("legacy config parses");
        assert!(!config.require_approval_api_token);
    }

    #[test]
    fn approval_token_guard_can_be_enabled() {
        let toml = format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
require_approval_api_token = true
approval_timeout_secs = 300
default_deny = true
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        );
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("config parses");
        assert!(config.require_approval_api_token);
    }

    /// Legacy configs have no admin token: the field defaults to `None`, so
    /// approve/deny keep using the single `approval_api_token`.
    #[test]
    fn approval_admin_token_defaults_to_none_for_legacy_config() {
        assert!(PolicyConfig::default().approval_admin_token.is_none());

        let toml = format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
approval_timeout_secs = 300
default_deny = true
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        );
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("legacy config parses");
        assert!(config.approval_admin_token.is_none());
    }

    /// `JARVIS_POLICY_APPROVAL_ADMIN_TOKEN` becomes `approval_admin_token`
    /// through the same `Env::prefixed("JARVIS_POLICY_")` chain used by
    /// `load()`. As in the other env test, no OS env var is mutated.
    #[test]
    fn approval_admin_token_env_key_maps_to_field() {
        const ENV_KEY: &str = "JARVIS_POLICY_APPROVAL_ADMIN_TOKEN";

        let field = ENV_KEY
            .strip_prefix(ENV_PREFIX)
            .expect("key uses the policy env prefix")
            .to_ascii_lowercase();
        assert_eq!(field, "approval_admin_token");

        let toml = format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
approval_timeout_secs = 300
default_deny = true
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        );
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .merge(Env::prefixed(ENV_PREFIX).only(&[field.as_str()]))
            .merge(Serialized::default(field.as_str(), "admin-secret"))
            .extract()
            .expect("env-style key merges into the admin token field");
        assert_eq!(config.approval_admin_token.as_deref(), Some("admin-secret"));
        // The two tokens stay independent.
        assert!(config.approval_api_token.is_none());
    }

    #[test]
    fn approval_admin_token_can_be_set_in_toml() {
        let toml = format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
approval_timeout_secs = 300
approval_api_token = "gateway-api-token"
approval_admin_token = "admin-secret"
default_deny = true
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        );
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("config parses");
        assert_eq!(
            config.approval_api_token.as_deref(),
            Some("gateway-api-token")
        );
        assert_eq!(config.approval_admin_token.as_deref(), Some("admin-secret"));
    }

    /// Minimal legacy TOML shared by the I8-E state-path guard tests.
    fn base_toml() -> String {
        format!(
            r#"
grpc_addr = "127.0.0.1:50051"
http_addr = "127.0.0.1:8081"
cedar_policy_dir = "/etc/jarvis/policy"
casbin_model_path = "/etc/jarvis/policy/casbin_model.conf"
casbin_policy_path = "/etc/jarvis/policy/casbin_policy.csv"
kill_switch_admin_token = "{token}"
approval_timeout_secs = 300
default_deny = true
"#,
            token = PLACEHOLDER_KILL_SWITCH_TOKEN
        )
    }

    /// Legacy configs written before I8-E must keep the in-memory default:
    /// the new guard is opt-in and `kill_switch_state_path` stays unset.
    #[test]
    fn state_path_guard_defaults_to_false_for_legacy_config() {
        assert!(!PolicyConfig::default().require_kill_switch_state_path);
        assert!(PolicyConfig::default().kill_switch_state_path.is_none());

        let config: PolicyConfig = Figment::from(Toml::string(&base_toml()))
            .extract()
            .expect("legacy config parses");
        assert!(!config.require_kill_switch_state_path);
        assert!(config.kill_switch_state_path.is_none());
        config
            .validate_kill_switch_state_path()
            .expect("guard off is a no-op");
    }

    #[test]
    fn state_path_guard_can_be_enabled_in_toml() {
        let toml = format!("{}\nrequire_kill_switch_state_path = true\n", base_toml());
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("config parses");
        assert!(config.require_kill_switch_state_path);
    }

    /// With the guard on, a missing or blank state path must fail closed.
    #[test]
    fn state_path_guard_rejects_missing_path() {
        for path in [None, Some(String::new()), Some("   ".to_string())] {
            let config = PolicyConfig {
                kill_switch_state_path: path,
                require_kill_switch_state_path: true,
                ..Default::default()
            };
            let error = config
                .validate_kill_switch_state_path()
                .expect_err("missing state path must fail closed");
            assert!(
                error.to_string().contains("require_kill_switch_state_path"),
                "error names the opt-in flag"
            );
            assert!(
                error
                    .to_string()
                    .contains("JARVIS_POLICY_KILL_SWITCH_STATE_PATH"),
                "error names the env override"
            );
        }
    }

    /// The figment/serde path must preserve the guard so `load()`-style configs
    /// with the flag on and no state path fail closed as well.
    #[test]
    fn toml_guard_without_path_fails_closed() {
        let toml = format!("{}\nrequire_kill_switch_state_path = true\n", base_toml());
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("config parses");
        assert!(
            config.validate_kill_switch_state_path().is_err(),
            "guard=true with no path must fail closed"
        );
    }

    /// The guard passes when a non-blank state path is configured.
    #[test]
    fn state_path_guard_accepts_configured_path() {
        let toml = format!(
            "{}\nkill_switch_state_path = \"/var/lib/jarvis/kill-switch.json\"\nrequire_kill_switch_state_path = true\n",
            base_toml()
        );
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("config parses");
        assert_eq!(
            config.kill_switch_state_path.as_deref(),
            Some("/var/lib/jarvis/kill-switch.json")
        );
        config
            .validate_kill_switch_state_path()
            .expect("configured state path satisfies the guard");
    }

    /// `JARVIS_POLICY_REQUIRE_KILL_SWITCH_STATE_PATH` maps to the guard field
    /// via `Env::prefixed("JARVIS_POLICY_")`. No OS env var is mutated (edition
    /// 2024 makes `set_var` unsafe and racy); the same provider chain is driven
    /// by a `Serialized` value merged last.
    #[test]
    fn env_key_maps_to_state_path_guard_flag() {
        const ENV_KEY: &str = "JARVIS_POLICY_REQUIRE_KILL_SWITCH_STATE_PATH";

        let field = ENV_KEY
            .strip_prefix(ENV_PREFIX)
            .expect("key uses the policy env prefix")
            .to_ascii_lowercase();
        assert_eq!(field, "require_kill_switch_state_path");

        let config: PolicyConfig = Figment::from(Toml::string(&base_toml()))
            .merge(Env::prefixed(ENV_PREFIX).only(&[field.as_str()]))
            .merge(Serialized::default(field.as_str(), true))
            .extract()
            .expect("env-style key merges into the guard field");
        assert!(config.require_kill_switch_state_path);
    }

    /// `JARVIS_POLICY_KILL_SWITCH_STATE_PATH` maps to the path field through
    /// the same provider chain.
    #[test]
    fn env_key_maps_to_state_path_field() {
        const ENV_KEY: &str = "JARVIS_POLICY_KILL_SWITCH_STATE_PATH";

        let field = ENV_KEY
            .strip_prefix(ENV_PREFIX)
            .expect("key uses the policy env prefix")
            .to_ascii_lowercase();
        assert_eq!(field, "kill_switch_state_path");

        let config: PolicyConfig = Figment::from(Toml::string(&base_toml()))
            .merge(Env::prefixed(ENV_PREFIX).only(&[field.as_str()]))
            .merge(Serialized::default(
                field.as_str(),
                "/var/lib/jarvis/kill-switch.json",
            ))
            .extract()
            .expect("env-style key merges into the state path field");
        assert_eq!(
            config.kill_switch_state_path.as_deref(),
            Some("/var/lib/jarvis/kill-switch.json")
        );
    }

    /// The service must find its config regardless of the launch directory;
    /// the former `config/policy.toml` was CWD-relative.
    #[test]
    fn default_config_path_is_absolute() {
        assert!(Path::new(DEFAULT_CONFIG_PATH).is_absolute());
        assert_eq!(DEFAULT_CONFIG_PATH, "/etc/jarvis/policy.toml");
    }

    #[test]
    fn config_path_override_is_honoured() {
        const OVERRIDE: &str = "/tmp/jarvis-policy-custom.toml";
        assert_eq!(resolve_config_path(Some(OVERRIDE.into())), OVERRIDE);
        assert_eq!(resolve_config_path(None), DEFAULT_CONFIG_PATH);
        assert_eq!(resolve_config_path(Some("   ".into())), DEFAULT_CONFIG_PATH);
    }

    /// The override variable shares the config prefix, so verify it maps to
    /// the `config_path` key that `load_from_path` filters out of the provider.
    #[test]
    fn config_path_env_name_matches_prefix() {
        let field = CONFIG_PATH_ENV
            .strip_prefix(ENV_PREFIX)
            .expect("override variable uses the policy env prefix")
            .to_ascii_lowercase();
        assert_eq!(field, "config_path");
    }

    /// Run the same startup validations that `load_from_path` invokes, so
    /// tests can assert on their combined error without touching the process
    /// environment or the filesystem.
    fn validate_startup(config: &PolicyConfig) -> Result<()> {
        config.validate_kill_switch_state_path()?;
        config.validate_kill_switch_admin_token()?;
        config.validate_approval_tokens()
    }

    /// An unset/blank admin token must abort startup regardless of the opt-in
    /// flag: with an empty token the kill-switch gates would accept an empty
    /// presented token and let any caller stop the system.
    #[test]
    fn blank_admin_token_fails_closed_without_the_guard() {
        for token in ["", "   ", "\t\n"] {
            let config = PolicyConfig {
                kill_switch_admin_token: token.into(),
                require_kill_switch_admin_token: false,
                ..Default::default()
            };
            let error = config
                .validate_kill_switch_admin_token()
                .expect_err("blank token must fail closed");
            let message = error.to_string();
            assert!(
                message.contains("unset or blank"),
                "error explains the blank token: {message}"
            );
            assert!(
                message.contains("JARVIS_POLICY_KILL_SWITCH_ADMIN_TOKEN"),
                "error names the env override: {message}"
            );
        }
    }

    /// The known placeholder is rejected regardless of the legacy flag.
    #[test]
    fn placeholder_admin_token_is_rejected_with_or_without_guard() {
        for guard in [false, true] {
            let config = PolicyConfig {
                kill_switch_admin_token: PLACEHOLDER_KILL_SWITCH_TOKEN.into(),
                require_kill_switch_admin_token: guard,
                ..Default::default()
            };
            let error = config
                .validate_kill_switch_admin_token()
                .expect_err("the placeholder is never a usable admin token");
            assert!(
                error.to_string().contains(PLACEHOLDER_KILL_SWITCH_TOKEN),
                "error names the placeholder"
            );
            assert!(
                error.to_string().contains("require_kill_switch_admin_token"),
                "error names the legacy flag"
            );
        }
    }

    /// A real secret satisfies the guard, and startup errors must never echo
    /// the configured token value (a sentinel stands in for a real secret).
    #[test]
    fn real_admin_token_passes_and_errors_never_echo_it() {
        const SENTINEL: &str = "SENTINEL-DO-NOT-LOG-9f3a4b";
        let config = PolicyConfig {
            kill_switch_admin_token: SENTINEL.into(),
            require_kill_switch_admin_token: true,
            ..Default::default()
        };
        config
            .validate_kill_switch_admin_token()
            .expect("a real token satisfies the guard");

        let missing_path = PolicyConfig {
            require_kill_switch_state_path: true,
            kill_switch_state_path: None,
            ..config
        };
        let error = validate_startup(&missing_path).expect_err("state path guard fails closed");
        assert!(
            !error.to_string().contains(SENTINEL),
            "startup error must not contain the token value"
        );
    }

    /// The figment extraction used by `load()` must still parse a blank token
    /// so the new validation is the component that fails closed, not serde.
    #[test]
    fn blank_token_from_config_provider_is_rejected_by_validation() {
        let toml = base_toml().replace(PLACEHOLDER_KILL_SWITCH_TOKEN, "");
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("blank token parses as an empty string");
        assert_eq!(config.kill_switch_admin_token(), "");
        assert!(config.validate_kill_switch_admin_token().is_err());
    }

    /// R2-01: the placeholder guard must fire on both startup routes. The
    /// load path aborts inside `load_from_path_with`; the programmatic path
    /// aborts inside `PolicyEngine::new` before any Cedar/Casbin I/O. Both
    /// errors originate from the single shared validator, so their text is
    /// identical. No log capture is needed to prove the guard is shared.
    #[tokio::test]
    async fn placeholder_guard_is_shared_by_load_and_programmatic_paths() {
        let toml = format!("{}\nrequire_kill_switch_admin_token = true\n", base_toml());
        let name = format!(
            "jarvis-policy-placeholder-shared-{}.toml",
            std::process::id()
        );
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, &toml).expect("write temp config");

        let load_error = PolicyConfig::load_from_path_with(
            path.to_str().expect("utf8 temp path"),
            Figment::new(),
        )
        .expect_err("load path rejects a guarded placeholder token");

        let config = PolicyConfig {
            kill_switch_admin_token: PLACEHOLDER_KILL_SWITCH_TOKEN.into(),
            require_kill_switch_admin_token: true,
            ..Default::default()
        };
        // PolicyEngine is not Debug, so avoid expect_err's Display bound by
        // destructuring the Result directly.
        let engine_error = match crate::PolicyEngine::new(config).await {
            Ok(_) => panic!("programmatic path must reject the guarded placeholder"),
            Err(error) => error,
        };

        let message = engine_error.to_string();
        assert_eq!(
            load_error.to_string(),
            message,
            "both routes surface the same shared guard"
        );
        assert!(
            message.contains("require_kill_switch_admin_token"),
            "error names the opt-in flag: {message}"
        );
        assert!(
            message.contains(PLACEHOLDER_KILL_SWITCH_TOKEN),
            "error names the placeholder: {message}"
        );

        std::fs::remove_file(&path).expect("clean temp config");
    }

    // ---- I29-A approval persistence config seam --------------------------

    /// New fields are additive: legacy configs and `Default` leave them at
    /// `None` / `false`, and validation stays a no-op.
    #[test]
    fn approval_persistence_defaults_are_backward_compatible() {
        assert!(PolicyConfig::default().pg_url.is_none());
        assert!(!PolicyConfig::default().require_approval_persistence);

        let config: PolicyConfig = Figment::from(Toml::string(&base_toml()))
            .extract()
            .expect("legacy config parses");
        assert!(config.pg_url.is_none());
        assert!(!config.require_approval_persistence);
        config
            .validate_approval_persistence()
            .expect("guard off is a no-op");
    }

    /// With the guard on, a missing or blank `pg_url` must fail closed.
    #[test]
    fn approval_persistence_guard_rejects_missing_pg_url() {
        for url in [None, Some(String::new()), Some("   ".to_string())] {
            let config = PolicyConfig {
                pg_url: url,
                require_approval_persistence: true,
                ..Default::default()
            };
            let error = config
                .validate_approval_persistence()
                .expect_err("missing pg_url must fail closed");
            let message = error.to_string();
            assert!(
                message.contains("require_approval_persistence"),
                "error names the opt-in flag: {message}"
            );
            assert!(
                message.contains("JARVIS_POLICY_PG_URL"),
                "error names the env override: {message}"
            );
        }
    }

    /// A figment-parsed config with the flag on and no URL fails closed too.
    #[test]
    fn toml_persistence_guard_without_url_fails_closed() {
        let toml = format!("{}\nrequire_approval_persistence = true\n", base_toml());
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("config parses");
        assert!(config.require_approval_persistence);
        assert!(
            config.validate_approval_persistence().is_err(),
            "guard=true with no pg_url must fail closed"
        );
    }

    /// A configured URL satisfies the guard; no connection is attempted.
    #[test]
    fn approval_persistence_guard_accepts_configured_pg_url() {
        let toml = format!(
            "{}\npg_url = \"postgres://127.0.0.1:5432/jarvis\"\nrequire_approval_persistence = true\n",
            base_toml()
        );
        let config: PolicyConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("config parses");
        assert_eq!(
            config.pg_url.as_deref(),
            Some("postgres://127.0.0.1:5432/jarvis")
        );
        assert!(config.require_approval_persistence);
        config
            .validate_approval_persistence()
            .expect("configured pg_url satisfies the guard");
    }

    /// `JARVIS_POLICY_PG_URL` maps to `pg_url` via the same env chain as
    /// `load()`. No OS env var is mutated; a `Serialized` value is merged last.
    #[test]
    fn env_key_maps_to_pg_url_field() {
        const ENV_KEY: &str = "JARVIS_POLICY_PG_URL";

        let field = ENV_KEY
            .strip_prefix(ENV_PREFIX)
            .expect("key uses the policy env prefix")
            .to_ascii_lowercase();
        assert_eq!(field, "pg_url");

        let config: PolicyConfig = Figment::from(Toml::string(&base_toml()))
            .merge(Env::prefixed(ENV_PREFIX).only(&[field.as_str()]))
            .merge(Serialized::default(
                field.as_str(),
                "postgres://127.0.0.1:5432/jarvis",
            ))
            .extract()
            .expect("env-style key merges into the pg_url field");
        assert_eq!(
            config.pg_url.as_deref(),
            Some("postgres://127.0.0.1:5432/jarvis")
        );
    }

    // ---- R4-A: configured-but-blank approval tokens ----------------------

    /// A token that is set but empty/whitespace-only must fail closed for both
    /// the API and the admin field. A blank presented header would otherwise
    /// equal a blank configured value in `constant_time_eq`.
    #[test]
    fn blank_approval_tokens_fail_closed() {
        for blank in ["", "   ", "\t\n"] {
            let api = PolicyConfig {
                approval_api_token: Some(blank.into()),
                ..Default::default()
            };
            let message = api
                .validate_approval_tokens()
                .expect_err("blank api token must fail closed")
                .to_string();
            assert!(message.contains("JARVIS_POLICY_APPROVAL_API_TOKEN"));
            assert!(message.contains("blank"));
            validate_startup(&api).expect_err("startup chain also fails closed");

            let admin = PolicyConfig {
                approval_admin_token: Some(blank.into()),
                ..Default::default()
            };
            let message = admin
                .validate_approval_tokens()
                .expect_err("blank admin token must fail closed")
                .to_string();
            assert!(message.contains("JARVIS_POLICY_APPROVAL_ADMIN_TOKEN"));
            assert!(message.contains("blank"));
            validate_startup(&admin).expect_err("startup chain also fails closed");
        }
    }

    /// The errors are fixed literals: no configured token value is ever echoed
    /// into startup logs. `\t\n` is non-empty but blank after `trim`.
    #[test]
    fn blank_approval_token_errors_never_echo_the_value() {
        let blank = "\t\n";
        let api = PolicyConfig {
            approval_api_token: Some(blank.into()),
            ..Default::default()
        };
        let api_result = api.validate_approval_tokens();
        assert_eq!(
            api_result.expect_err("blank").to_string(),
            "approval_api_token is set but blank; set \
             JARVIS_POLICY_APPROVAL_API_TOKEN to a non-empty secret or \
             unset it before starting the policy service"
        );

        let admin = PolicyConfig {
            approval_admin_token: Some(blank.into()),
            ..Default::default()
        };
        let admin_result = admin.validate_approval_tokens();
        assert_eq!(
            admin_result.expect_err("blank").to_string(),
            "approval_admin_token is set but blank; set \
             JARVIS_POLICY_APPROVAL_ADMIN_TOKEN to a non-empty secret or \
             unset it before starting the policy service"
        );
    }

    /// Unset tokens keep the legacy behavior; real tokens still pass.
    #[test]
    fn unset_and_nonblank_approval_tokens_pass_validation() {
        let legacy = PolicyConfig {
            // Default() carries the rejected placeholder; a real token is
            // required to exercise the approval-token behavior under test.
            kill_switch_admin_token: "test-admin-token".into(),
            ..Default::default()
        };
        assert!(legacy.approval_api_token.is_none());
        assert!(legacy.approval_admin_token.is_none());
        legacy
            .validate_approval_tokens()
            .expect("unset tokens keep the legacy behavior");
        validate_startup(&legacy).expect("startup chain is unchanged for legacy configs");

        let configured = PolicyConfig {
            kill_switch_admin_token: "test-admin-token".into(),
            approval_api_token: Some("api-secret".into()),
            approval_admin_token: Some("admin-secret".into()),
            ..Default::default()
        };
        configured
            .validate_approval_tokens()
            .expect("non-blank tokens pass");
        validate_startup(&configured).expect("startup chain passes");
    }

    /// TOML-authority check: no env provider is injected, so the blank token
    /// can only come from `Toml::file(path)`. If that merge disappears from
    /// `load_from_path_with`, the guard no longer fires and this test fails
    /// instead of passing vacuously. Ambient `JARVIS_POLICY_*` vars are
    /// irrelevant here because no env provider participates in the parse.
    #[test]
    fn load_from_path_rejects_blank_approval_token_from_toml() {
        // Use a real (test-only) token so this test exercises the blank
        // approval-token guard rather than the placeholder-token guard.
        let safe_toml = base_toml().replace(PLACEHOLDER_KILL_SWITCH_TOKEN, "test-admin-token");
        for (key, env_key) in [
            ("approval_api_token", "JARVIS_POLICY_APPROVAL_API_TOKEN"),
            ("approval_admin_token", "JARVIS_POLICY_APPROVAL_ADMIN_TOKEN"),
        ] {
            let toml = format!("{safe_toml}\n{key} = \"\"\n");
            let file_name = format!("jarvis-policy-r4a-{key}-toml.toml");
            let path = std::env::temp_dir().join(&file_name);
            std::fs::write(&path, &toml).expect("write temp config");
            // Empty provider: the blank value under test is read from the file.
            let overrides = Figment::new();
            let loaded = PolicyConfig::load_from_path_with(
                path.to_str().expect("utf8 temp path"),
                overrides,
            );
            let error = loaded.expect_err("blank TOML token must abort startup");
            assert!(
                error.to_string().contains(env_key),
                "error names the env override: {error}"
            );
            std::fs::remove_file(&path).expect("clean temp config");
        }
    }

    /// Env-override path, kept separate from the TOML-authority test above: a
    /// valid file value is replaced by a blank override, and startup still
    /// fails closed. This proves the injected provider is merged (no early
    /// return on env presence) and validation is not skipped. `Serialized` is
    /// merged after the env provider so ambient CI vars cannot decide the
    /// outcome; the file value stays non-blank so the error can only come from
    /// the override.
    #[test]
    fn load_from_path_env_override_rejects_blank_token_without_skipping_validation() {
        // Use a real (test-only) token so the only rejection can be the blank
        // override under test.
        let safe_toml = base_toml().replace(PLACEHOLDER_KILL_SWITCH_TOKEN, "test-admin-token");
        for (key, env_key) in [
            ("approval_api_token", "JARVIS_POLICY_APPROVAL_API_TOKEN"),
            ("approval_admin_token", "JARVIS_POLICY_APPROVAL_ADMIN_TOKEN"),
        ] {
            let toml = format!("{safe_toml}\n{key} = \"from-toml\"\n");
            let file_name = format!("jarvis-policy-r4a-{key}-env.toml");
            let path = std::env::temp_dir().join(&file_name);
            std::fs::write(&path, &toml).expect("write temp config");
            let overrides = Figment::from(Env::prefixed(ENV_PREFIX).ignore(&["config_path"]))
                .merge(Serialized::default(key, ""));
            let loaded = PolicyConfig::load_from_path_with(
                path.to_str().expect("utf8 temp path"),
                overrides,
            );
            let error = loaded.expect_err("blank env override must abort startup");
            assert!(
                error.to_string().contains(env_key),
                "error names the env override: {error}"
            );
            std::fs::remove_file(&path).expect("clean temp config");
        }
    }

    /// I105-B: a one-character typo of an opt-in guard key must abort startup
    /// instead of being silently ignored (leaving the guard at `false`). The
    /// mistyped key comes from the real TOML load path; no OS env var is
    /// mutated and every other key is valid.
    #[test]
    fn typo_guard_key_is_rejected_at_startup() {
        let name = format!("jarvis-policy-typo-guard-{}.toml", std::process::id());
        let path = std::env::temp_dir().join(name);
        // `require_aproval_api_token` is a one-character typo of the real
        // opt-in `require_approval_api_token` guard.
        let toml = format!("{}\nrequire_aproval_api_token = true\n", base_toml());
        std::fs::write(&path, &toml).expect("write temp config");
        let loaded = PolicyConfig::load_from_path_with(
            path.to_str().expect("utf8 temp path"),
            Figment::new(),
        );
        let error = loaded.expect_err("unknown guard key must abort startup");
        assert!(
            error.to_string().contains("require_aproval_api_token"),
            "error must name the mistyped key: {error}"
        );
        std::fs::remove_file(&path).expect("clean temp config");
    }

    /// I105-B: the same rejection applies when the typo arrives through an
    /// env-style provider, i.e. after `Env::prefixed("JARVIS_POLICY_")` has
    /// stripped the prefix. `Serialized` models that shape without mutating
    /// the process environment (edition 2024 makes `set_var` unsafe).
    #[test]
    fn typo_guard_env_key_is_rejected_at_startup() {
        const TYPO: &str = "require_aproval_api_token";
        let name = format!("jarvis-policy-typo-guard-env-{}.toml", std::process::id());
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, base_toml()).expect("write temp config");
        let loaded = PolicyConfig::load_from_path_with(
            path.to_str().expect("utf8 temp path"),
            Figment::from(Serialized::default(TYPO, true)),
        );
        let error = loaded.expect_err("mistyped env guard key must abort startup");
        assert!(
            error.to_string().contains(TYPO),
            "error must name the mistyped key: {error}"
        );
        std::fs::remove_file(&path).expect("clean temp config");
    }
}
