//! Configuration for Event Normalizer

use anyhow::Result;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};

/// Absolute default location of the event TOML. The former CWD-relative
/// `config/event.toml` was not found when the service was started outside
/// the repository root.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/jarvis/event.toml";

/// Environment variable that overrides [`DEFAULT_CONFIG_PATH`].
pub const CONFIG_PATH_ENV: &str = "JARVIS_EVENT_CONFIG_PATH";

const ENV_PREFIX: &str = "JARVIS_EVENT_";

/// Select the TOML path from an optional environment override. Kept pure so
/// both branches are testable without mutating the process environment
/// (edition 2024 makes `set_var` unsafe and parallel tests racy).
fn resolve_config_path(override_path: Option<String>) -> String {
    override_path
        .filter(|path| !path.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_string())
}

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
///
/// The `Mutex` wrapper exists only so `#[cfg(test)]` can reset the snapshot;
/// production reads/writes keep the bare `OnceLock` first-wins semantics.
static INGRESS_ADMIN_TOKEN: Mutex<OnceLock<Option<String>>> = Mutex::new(OnceLock::new());

/// Publish the effective token before any listener starts. Only the first
/// value wins; the value itself is never logged.
pub(crate) fn publish_admin_token(token: Option<String>) {
    let snapshot = match INGRESS_ADMIN_TOKEN.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if snapshot.set(token).is_err() {
        tracing::warn!("event admin token already published; keeping the first value");
    }
}

/// Effective token for HTTP/gRPC ingress authentication.
///
/// Uses the value published by [`EventConfig::load`] when available, and falls
/// back to the environment for embedded callers that never invoked it.
pub(crate) fn ingress_admin_token() -> Option<String> {
    let snapshot = match INGRESS_ADMIN_TOKEN.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    match snapshot.get() {
        Some(token) => token.clone(),
        None => std::env::var(ADMIN_TOKEN_ENV_KEY)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
    }
}

/// Clear the process-wide snapshot so tests do not depend on execution order.
/// Test-only: production code has no path that unpublishes a token.
#[cfg(test)]
fn reset_admin_token_for_test() {
    let mut snapshot = match INGRESS_ADMIN_TOKEN.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    *snapshot = OnceLock::new();
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
    /// Unset keeps the legacy unauthenticated loopback behavior; a configured
    /// but blank value aborts startup (fail-closed).
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
    /// Load from the absolute [`DEFAULT_CONFIG_PATH`], or from the path in
    /// [`CONFIG_PATH_ENV`] when it is set and non-blank, with `JARVIS_EVENT_*`
    /// environment overrides applied on top.
    ///
    /// The default file may be absent on real hosts: configuration is supplied
    /// by the `JARVIS_EVENT_*` Env provider (`bws run` / systemd), so the
    /// service starts with no TOML on disk. `Toml::file` simply contributes no
    /// data for a missing file; every required field then comes from the
    /// environment.
    pub fn load() -> Result<Self> {
        let path = resolve_config_path(std::env::var(CONFIG_PATH_ENV).ok());
        Self::load_from_path(&path)
    }

    /// Parse a config from an explicit TOML path. Split out from [`Self::load`]
    /// so path selection is testable without touching the process environment.
    fn load_from_path(path: &str) -> Result<Self> {
        Self::load_from_path_with(path, Env::prefixed(ENV_PREFIX).ignore(&["config_path"]))
    }

    /// Same parse + publish chain as [`Self::load_from_path`], with the
    /// override provider injected. Tests merge a `Serialized` value after the
    /// env provider so an ambient `JARVIS_EVENT_ADMIN_TOKEN` cannot mask the
    /// value under test, without mutating process-wide env vars.
    fn load_from_path_with<P: figment::Provider>(path: &str, overrides: P) -> Result<Self> {
        // Flat snake_case fields: do NOT use figment's Env split, which would
        // turn JARVIS_EVENT_GRPC_ADDR into the nested key `grpc.addr`.
        // The config-path variable only selects the file, so keep it out of
        // the field provider as well.
        let config: EventConfig = Figment::new()
            .merge(Toml::file(path))
            .merge(overrides)
            .extract()?;
        // I99-A: a configured-but-blank token would otherwise collapse to
        // `None` in `admin_token()` and silently leave the Webhook (8085) and
        // gRPC admin gates unauthenticated. Fail closed at startup, before the
        // value is published to ingress.
        config.validate_admin_token()?;
        publish_admin_token(config.admin_token().map(str::to_owned));
        Ok(config)
    }

    /// Fail-closed startup validation of the optional admin token (I99-A).
    ///
    /// A token that is present but blank (empty or whitespace-only) is
    /// rejected: `admin_token()` would otherwise treat it as unset and both
    /// the HTTP `x-jarvis-event-token` gate and the gRPC metadata gate would
    /// fall back to the legacy unauthenticated loopback path. `None` (unset)
    /// stays valid and keeps that legacy behavior for compatibility.
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
                "admin_token is set but blank; set JARVIS_EVENT_ADMIN_TOKEN \
                 to a non-empty secret or unset it before starting the service"
            );
        }
        Ok(())
    }

    pub fn ha_token(&self) -> &str {
        &self.ha_token
    }

    /// Returns the trimmed token, or `None` when unset/blank. A configured
    /// blank value is rejected at startup by [`Self::validate_admin_token`];
    /// this filter only guards direct callers (defense in depth).
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
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// pid + counter keeps temp config files of concurrent cargo test
    /// processes apart; the in-process token lock alone cannot do that.
    fn unique_temp_file(tag: &str) -> std::path::PathBuf {
        let n = TEST_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "jarvis-event-{tag}-{}-{n}.toml",
            std::process::id()
        ))
    }

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

    /// Fail-closed regression: a set-but-blank token must never silently
    /// collapse to the unauthenticated path. Both the legacy unset case and a
    /// real secret must still validate.
    #[test]
    fn admin_token_from_toml_and_blank_value_is_rejected() {
        let with_token = format!("{LEGACY_TOML}\nadmin_token = \"event-secret\"\n");
        let config: EventConfig = Figment::from(Toml::string(&with_token))
            .extract()
            .expect("config with token parses");
        assert_eq!(config.admin_token(), Some("event-secret"));
        assert!(config.validate_admin_token().is_ok());

        let blank = format!("{LEGACY_TOML}\nadmin_token = \"  \"\n");
        let config: EventConfig = Figment::from(Toml::string(&blank))
            .extract()
            .expect("blank token parses");
        let message = config
            .validate_admin_token()
            .expect_err("blank configured token must be rejected")
            .to_string();
        assert!(
            message.contains("admin_token is set but blank"),
            "{message}"
        );
    }

    /// `None` (unset) remains the legacy unauthenticated compatibility case;
    /// a real secret is accepted. Only a present-but-blank value is an error.
    #[test]
    fn validate_admin_token_accepts_unset_and_non_blank() {
        assert!(EventConfig::default().validate_admin_token().is_ok());
        let config = EventConfig {
            admin_token: Some("event-secret".to_owned()),
            ..Default::default()
        };
        assert!(config.validate_admin_token().is_ok());
    }

    /// Empty, ASCII whitespace and visually distinct Unicode whitespace are
    /// all blank after `trim()` and must abort startup. The fixed error text
    /// interpolates no configuration value, so a non-breaking-space token
    /// cannot be echoed back into startup logs.
    #[test]
    fn validate_admin_token_rejects_blank_and_never_echoes_value() {
        for token in ["", "   ", "\t\n", "\u{00a0}\u{00a0}"] {
            let config = EventConfig {
                admin_token: Some(token.to_owned()),
                ..Default::default()
            };
            let message = config
                .validate_admin_token()
                .expect_err("blank configured token must be rejected")
                .to_string();
            assert!(
                message.contains("admin_token is set but blank"),
                "{message}"
            );
        }
        let config = EventConfig {
            admin_token: Some("\u{00a0}\u{00a0}".to_owned()),
            ..Default::default()
        };
        let message = config
            .validate_admin_token()
            .expect_err("blank configured token must be rejected")
            .to_string();
        assert!(!message.contains('\u{00a0}'), "{message}");
    }

    /// The real startup path must enforce the guard, not just a direct helper
    /// call. An empty override provider is used so no ambient
    /// `JARVIS_EVENT_ADMIN_TOKEN` can mask the TOML value under test.
    #[test]
    fn load_from_path_rejects_blank_admin_token() {
        let toml = format!("{LEGACY_TOML}\nadmin_token = \"   \"\n");
        let path = std::env::temp_dir().join("jarvis-event-blank-admin-token.toml");
        std::fs::write(&path, &toml).expect("write temp config");
        let path_str = path.to_str().expect("utf8 temp path");
        let error = EventConfig::load_from_path_with(path_str, Figment::new())
            .expect_err("blank configured token must abort startup");
        assert!(
            error.to_string().contains("admin_token is set but blank"),
            "{error}"
        );
        std::fs::remove_file(&path).expect("clean temp config");
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

    /// Serializes the two tests that reset the process-wide token snapshot.
    static TOKEN_SNAPSHOT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A second publish is ignored: `load()` can run more than once, and the
    /// first merged value is the one the HTTP/gRPC transports must use.
    #[test]
    fn ingress_token_snapshot_is_first_wins() {
        let _guard = match TOKEN_SNAPSHOT_TEST_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        reset_admin_token_for_test();
        publish_admin_token(Some("first-secret".into()));
        publish_admin_token(Some("second-secret".into()));
        assert_eq!(ingress_admin_token().as_deref(), Some("first-secret"));
        reset_admin_token_for_test();
    }

    /// One-shot check of the real `load_from_path` publish route with TOML as
    /// the sole authority: no env provider is injected, so the token can only
    /// come from `Toml::file(path)`. Both the parsed config and the value
    /// published to ingress are asserted, so dropping the TOML merge (or
    /// relying on `ingress_admin_token()`'s env fallback) fails this test.
    /// First-wins itself is covered by the test above.
    #[test]
    fn load_from_path_publishes_admin_token_from_toml() {
        let _guard = match TOKEN_SNAPSHOT_TEST_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        reset_admin_token_for_test();

        let toml = format!("{LEGACY_TOML}\nadmin_token = \"from-toml\"\n");
        let path = unique_temp_file("i81d-admin-token-file");
        std::fs::write(&path, &toml).expect("write temp config");
        let path_str = path.to_str().expect("utf8 temp path");
        let overrides = Figment::new();
        let config = EventConfig::load_from_path_with(path_str, overrides).expect("config loads");
        assert_eq!(config.admin_token(), Some("from-toml"));
        assert_eq!(ingress_admin_token().as_deref(), Some("from-toml"));

        std::fs::remove_file(&path).expect("clean temp config");
        reset_admin_token_for_test();
    }

    /// Env-override path, kept separate from the TOML-authority test above: a
    /// different value supplied through the override provider replaces the
    /// file's token and is the one published to ingress. The snapshot is reset
    /// first, so the ingress assertion cannot pass via `ingress_admin_token()`'s
    /// env fallback (the "early return" on env presence). `Serialized` is
    /// merged after the env provider so ambient CI vars cannot decide the
    /// outcome.
    #[test]
    fn load_from_path_env_override_publishes_admin_token() {
        let _guard = match TOKEN_SNAPSHOT_TEST_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        reset_admin_token_for_test();

        let toml = format!("{LEGACY_TOML}\nadmin_token = \"from-toml\"\n");
        let path = unique_temp_file("i81d-admin-token-env");
        std::fs::write(&path, &toml).expect("write temp config");
        let path_str = path.to_str().expect("utf8 temp path");
        let overrides = Figment::from(Env::prefixed(ENV_PREFIX).ignore(&["config_path"]))
            .merge(Serialized::default("admin_token", "from-env"));
        let config = EventConfig::load_from_path_with(path_str, overrides).expect("config loads");
        assert_eq!(config.admin_token(), Some("from-env"));
        assert_eq!(ingress_admin_token().as_deref(), Some("from-env"));

        std::fs::remove_file(&path).expect("clean temp config");
        reset_admin_token_for_test();
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

    /// The service must find its config regardless of the launch directory;
    /// the former `config/event.toml` was CWD-relative.
    #[test]
    fn default_config_path_is_absolute() {
        use std::path::Path;
        assert!(Path::new(DEFAULT_CONFIG_PATH).is_absolute());
        assert_eq!(DEFAULT_CONFIG_PATH, "/etc/jarvis/event.toml");
    }

    #[test]
    fn config_path_override_is_honoured() {
        const OVERRIDE: &str = "/tmp/jarvis-event-custom.toml";
        assert_eq!(resolve_config_path(Some(OVERRIDE.into())), OVERRIDE);
        assert_eq!(resolve_config_path(None), DEFAULT_CONFIG_PATH);
        assert_eq!(resolve_config_path(Some("   ".into())), DEFAULT_CONFIG_PATH);
        assert_eq!(
            resolve_config_path(Some(String::new())),
            DEFAULT_CONFIG_PATH
        );
    }

    /// The override variable shares the event env prefix, so verify it maps
    /// to the `config_path` key that `load_from_path` filters out.
    #[test]
    fn config_path_env_name_matches_prefix() {
        let field = CONFIG_PATH_ENV
            .strip_prefix(ENV_PREFIX)
            .expect("override variable uses the event env prefix")
            .to_ascii_lowercase();
        assert_eq!(field, "config_path");
    }
}
