//! Configuration for Event Normalizer

use anyhow::Result;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

/// Selects how `Normalizer` obtains context for an event.
///
/// `InProcess` is the default and keeps the slice-1 `DefaultContextProvider`
/// route (an embedded `ContextBuilder`) byte-for-byte. `Grpc` is opt-in and
/// routes through `GrpcContextProvider` to the context sidecar, which is what
/// lets the additive `source` / `timestamp` fields survive the hop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextMode {
    #[default]
    InProcess,
    Grpc,
}

/// Default endpoint of the context sidecar (its existing gRPC listen port).
fn default_context_grpc_addr() -> String {
    "http://127.0.0.1:50054".to_string()
}

/// HTTP header and gRPC metadata key carrying the event admin token.
pub(crate) const EVENT_TOKEN_HEADER: &str = "x-jarvis-event-token";

/// Environment variable mapped to [`EventConfig::admin_token`] by the
/// `JARVIS_EVENT_` figment prefix. Kept as a constant so a typo cannot
/// silently disable the ingress gate.
const ADMIN_TOKEN_ENV_KEY: &str = "JARVIS_EVENT_ADMIN_TOKEN";

/// Process-wide snapshot of the effective admin token, published by
/// [`EventConfig::load`].
///
/// `EventNormalizer` passes only an `Arc<Normalizer>` to the HTTP/gRPC
/// `serve` entry points (signatures fixed in `lib.rs`), and `Normalizer` lives
/// in a sibling module so its private `config` is not reachable from the
/// transport modules. Publishing the already-merged value here lets both TOML
/// and environment configuration drive the gate without touching those call
/// sites. `None` means "no token configured" (legacy loopback behavior).
static INGRESS_ADMIN_TOKEN: OnceLock<Option<String>> = OnceLock::new();

/// Publish the effective token before any listener starts. Only the first
/// value wins; the value itself is never logged.
pub(crate) fn publish_admin_token(token: Option<String>) {
    if INGRESS_ADMIN_TOKEN.set(token).is_err() {
        tracing::warn!("event admin token already published; keeping the first value");
    }
}

/// Effective token for HTTP/gRPC ingress authentication.
///
/// Uses the value published by [`EventConfig::load`] when available, and falls
/// back to the environment for embedded callers that never invoked it.
pub(crate) fn ingress_admin_token() -> Option<String> {
    match INGRESS_ADMIN_TOKEN.get() {
        Some(token) => token.as_ref().cloned(),
        None => std::env::var(ADMIN_TOKEN_ENV_KEY)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
    }
}

/// Compare two token byte strings without an early exit on the first mismatch.
///
/// The loop covers the longer input and pads the shorter one, so neither a
/// content mismatch nor a length mismatch short-circuits. Shared by the HTTP
/// middleware and the gRPC service; no extra crate and no `unsafe` is needed.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let max = a.len().max(b.len());
    let mut diff = a.len() ^ b.len();
    for (x, y) in a
        .iter()
        .map(Some)
        .chain(std::iter::repeat(None))
        .zip(b.iter().map(Some).chain(std::iter::repeat(None)))
        .take(max)
    {
        diff |= (x.copied().unwrap_or(0) ^ y.copied().unwrap_or(0)) as usize;
    }
    diff == 0
}

/// Authorize a presented token against the configured value.
///
/// A missing/blank configured token keeps the legacy unauthenticated loopback
/// path. A missing or non-UTF-8 presented value is never valid when a token is
/// configured. Neither value is logged.
pub(crate) fn token_matches(expected: Option<&str>, presented: Option<&str>) -> bool {
    let Some(expected) = expected.map(str::trim).filter(|t| !t.is_empty()) else {
        return true;
    };
    let Some(presented) = presented else {
        return false;
    };
    constant_time_eq(expected, presented)
}

#[derive(Clone, Serialize, Deserialize)]
pub struct EventConfig {
    pub grpc_addr: String,
    pub http_addr: String,
    pub webhook_addr: String,
    pub ha_url: String,
    #[serde(default)]
    pub ha_token: String,
    pub android_mcp_url: String,
    /// OpenCode server URL that receives policy-approved events.
    pub opencode_url: String,
    /// OpenCode location (working directory) used for agent sessions.
    pub opencode_location: String,
    /// OpenCode server password (when the server requires authentication).
    #[serde(default)]
    pub opencode_password: Option<String>,
    pub policy_grpc_addr: String,
    /// Optional shared secret for `/v1/events` and gRPC `EmitEvent`.
    /// `JARVIS_EVENT_ADMIN_TOKEN` maps here through the figment env prefix.
    /// Unset/blank keeps the legacy unauthenticated loopback behavior.
    #[serde(default)]
    pub admin_token: Option<String>,
    /// Context backend selection. Missing/legacy configs stay on `in_process`.
    #[serde(default)]
    pub context_mode: ContextMode,
    /// gRPC endpoint of the context sidecar (only read in `grpc` mode).
    #[serde(default = "default_context_grpc_addr")]
    pub context_grpc_addr: String,
    /// Optional `x-jarvis-context-token` value for `BuildContext` calls.
    /// Blank/unset keeps the legacy unauthenticated loopback path.
    #[serde(default)]
    pub context_grpc_token: Option<String>,
}

impl EventConfig {
    pub fn load() -> Result<Self> {
        // Flat snake_case fields: do NOT use figment's Env split, which would
        // turn JARVIS_EVENT_GRPC_ADDR into the nested key `grpc.addr`.
        let config: EventConfig = Figment::new()
            .merge(Toml::file("config/event.toml"))
            .merge(Env::prefixed("JARVIS_EVENT_"))
            .extract()?;
        publish_admin_token(config.admin_token().map(str::to_owned));
        Ok(config)
    }

    pub fn ha_token(&self) -> &str {
        &self.ha_token
    }

    /// An empty or whitespace-only value means "no token" (loopback compat).
    pub fn admin_token(&self) -> Option<&str> {
        self.admin_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
    }

    /// Trimmed token for the context gRPC sidecar; blank means "no token".
    pub fn context_grpc_token(&self) -> Option<&str> {
        self.context_grpc_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
    }
}

/// Redacted `Debug`: a derived implementation would print `admin_token` (and
/// other secrets) into any config dump, violating the "never log token values"
/// contract.
impl std::fmt::Debug for EventConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventConfig")
            .field("grpc_addr", &self.grpc_addr)
            .field("http_addr", &self.http_addr)
            .field("webhook_addr", &self.webhook_addr)
            .field("ha_url", &self.ha_url)
            .field("ha_token", &"<redacted>")
            .field("android_mcp_url", &self.android_mcp_url)
            .field("opencode_url", &self.opencode_url)
            .field("opencode_location", &self.opencode_location)
            .field("opencode_password", &"<redacted>")
            .field("policy_grpc_addr", &self.policy_grpc_addr)
            .field("admin_token", &"<redacted>")
            .field("context_mode", &self.context_mode)
            .field("context_grpc_addr", &self.context_grpc_addr)
            .field("context_grpc_token", &"<redacted>")
            .finish()
    }
}

impl Default for EventConfig {
    fn default() -> Self {
        Self {
            grpc_addr: "127.0.0.1:50053".into(),
            http_addr: "127.0.0.1:8083".into(),
            webhook_addr: "127.0.0.1:8085".into(),
            ha_url: "http://homeassistant.local:8123".into(),
            ha_token: "".into(),
            android_mcp_url: "http://127.0.0.1:49374".into(),
            opencode_url: "http://127.0.0.1:49374".into(),
            opencode_location: "/home/jarvis/repos".into(),
            opencode_password: None,
            policy_grpc_addr: "http://127.0.0.1:50051".into(),
            admin_token: None,
            context_mode: ContextMode::InProcess,
            context_grpc_addr: default_context_grpc_addr(),
            context_grpc_token: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::providers::Serialized;

    /// A config identical to a legacy deployment: no `admin_token` key at all.
    const LEGACY_TOML: &str = r#"
grpc_addr = "127.0.0.1:50053"
http_addr = "127.0.0.1:8083"
webhook_addr = "127.0.0.1:8085"
ha_url = "http://homeassistant.local:8123"
android_mcp_url = "http://127.0.0.1:49374"
opencode_url = "http://127.0.0.1:49374"
opencode_location = "/home/jarvis/repos"
policy_grpc_addr = "http://127.0.0.1:50051"
"#;

    #[test]
    fn legacy_config_without_admin_token_stays_open() {
        let config: EventConfig = Figment::from(Toml::string(LEGACY_TOML))
            .extract()
            .expect("legacy config parses");
        assert!(config.admin_token.is_none());
        assert!(config.admin_token().is_none());
        assert!(EventConfig::default().admin_token.is_none());
    }

    #[test]
    fn admin_token_from_toml_and_blank_value_disables_auth() {
        let with_token = format!("{LEGACY_TOML}\nadmin_token = \"event-secret\"\n");
        let config: EventConfig = Figment::from(Toml::string(&with_token))
            .extract()
            .expect("config with token parses");
        assert_eq!(config.admin_token(), Some("event-secret"));

        let blank = format!("{LEGACY_TOML}\nadmin_token = \"  \"\n");
        let config: EventConfig = Figment::from(Toml::string(&blank))
            .extract()
            .expect("blank token parses");
        assert!(
            config.admin_token().is_none(),
            "a blank token is treated as unset"
        );
    }

    /// `JARVIS_EVENT_ADMIN_TOKEN` becomes `admin_token` through the same
    /// `Env::prefixed("JARVIS_EVENT_")` chain used by `load()`. No OS env var
    /// is mutated; the env-style key is merged as a figment value.
    #[test]
    fn admin_token_env_key_maps_to_field() {
        const PREFIX: &str = "JARVIS_EVENT_";
        const KEY: &str = "JARVIS_EVENT_ADMIN_TOKEN";
        let field = KEY
            .strip_prefix(PREFIX)
            .expect("key uses the event env prefix")
            .to_ascii_lowercase();
        assert_eq!(field, "admin_token");

        let config: EventConfig = Figment::from(Toml::string(LEGACY_TOML))
            .merge(Serialized::default(field.as_str(), "env-secret"))
            .extract()
            .expect("env-style key merges into the admin token field");
        assert_eq!(config.admin_token(), Some("env-secret"));
    }

    #[test]
    fn debug_output_never_contains_the_admin_token() {
        let config = EventConfig {
            admin_token: Some("super-secret".into()),
            ..Default::default()
        };
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("super-secret"));
    }

    #[test]
    fn token_matches_enforces_exact_value_and_keeps_legacy_open_path() {
        assert!(token_matches(None, None), "no configured token stays open");
        assert!(token_matches(None, Some("anything")));
        assert!(token_matches(Some("  "), None), "blank config stays open");
        assert!(!token_matches(Some("secret"), None));
        assert!(token_matches(Some("secret"), Some("secret")));
        assert!(!token_matches(Some("secret"), Some("secreT")));
        assert!(!token_matches(Some("secret"), Some("secret-extra")));
        assert!(!token_matches(Some("secret"), Some("")));
    }

    #[test]
    fn ingress_token_snapshot_is_used_by_transports() {
        publish_admin_token(Some("snapshot-secret".into()));
        assert_eq!(ingress_admin_token().as_deref(), Some("snapshot-secret"));
    }

    /// I31-B: a legacy deployment with no new keys keeps the in-process route
    /// and the canonical sidecar endpoint; no opt-in flag is implicitly on.
    #[test]
    fn default_context_mode_is_in_process() {
        let default = EventConfig::default();
        assert_eq!(default.context_mode, ContextMode::InProcess);
        assert_eq!(default.context_grpc_addr, "http://127.0.0.1:50054");

        let legacy: EventConfig = Figment::from(Toml::string(LEGACY_TOML))
            .extract()
            .expect("legacy config without context keys parses");
        assert_eq!(legacy.context_mode, ContextMode::InProcess);
        assert_eq!(legacy.context_grpc_addr, "http://127.0.0.1:50054");
        assert!(legacy.context_grpc_token().is_none());
    }

    /// I31-B: the `grpc` mode and sidecar settings parse from TOML.
    #[test]
    fn context_grpc_mode_and_settings_from_toml() {
        let toml = format!(
            "{LEGACY_TOML}\ncontext_mode = \"grpc\"\n\
             context_grpc_addr = \"http://127.0.0.1:50055\"\n\
             context_grpc_token = \"ctx-secret\"\n"
        );
        let config: EventConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("context grpc config parses");
        assert_eq!(config.context_mode, ContextMode::Grpc);
        assert_eq!(config.context_grpc_addr, "http://127.0.0.1:50055");
        assert_eq!(config.context_grpc_token(), Some("ctx-secret"));
    }

    /// I31-B: the figment `JARVIS_EVENT_` prefix maps the new keys to the same
    /// flat snake_case fields (no OS env mutation, same chain as `load()`).
    #[test]
    fn context_grpc_env_keys_map_to_fields() {
        let config: EventConfig = Figment::from(Toml::string(LEGACY_TOML))
            .merge(Serialized::default("context_mode", "grpc"))
            .merge(Serialized::default(
                "context_grpc_addr",
                "http://127.0.0.1:50056",
            ))
            .merge(Serialized::default("context_grpc_token", "env-ctx-secret"))
            .extract()
            .expect("env-style context keys merge");
        assert_eq!(config.context_mode, ContextMode::Grpc);
        assert_eq!(config.context_grpc_addr, "http://127.0.0.1:50056");
        assert_eq!(config.context_grpc_token(), Some("env-ctx-secret"));
    }

    #[test]
    fn blank_context_grpc_token_is_treated_as_unset() {
        let toml = format!("{LEGACY_TOML}\ncontext_grpc_token = \"  \"\n");
        let config: EventConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("blank token parses");
        assert!(config.context_grpc_token().is_none());
    }

    #[test]
    fn debug_output_never_contains_the_context_grpc_token() {
        let config = EventConfig {
            context_grpc_token: Some("ctx-top-secret".into()),
            ..Default::default()
        };
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("ctx-top-secret"));
        assert!(rendered.contains("context_grpc_token: \"<redacted>\""));
    }
}
