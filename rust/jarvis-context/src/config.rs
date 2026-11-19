//! Configuration for Context Builder

use anyhow::Result;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use serde::{Deserialize, Serialize};

/// Absolute default location of the context TOML. The former CWD-relative
/// `config/context.toml` was not found when the service was started outside
/// the repository root.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/jarvis/context.toml";

/// Environment variable that overrides [`DEFAULT_CONFIG_PATH`].
pub const CONFIG_PATH_ENV: &str = "JARVIS_CONTEXT_CONFIG_PATH";

const ENV_PREFIX: &str = "JARVIS_CONTEXT_";

/// HTTP header and gRPC metadata key carrying the shared admin token.
///
/// The HTTP middleware in `http.rs` uses the same literal. Keeping it here lets
/// the gRPC ingress gate reference one canonical name without reaching into a
/// sibling module's private constant.
pub(crate) const CONTEXT_TOKEN_HEADER: &str = "x-jarvis-context-token";

/// Select the TOML path from an optional environment override. Kept pure so
/// both branches are testable without mutating the process environment
/// (edition 2024 makes `set_var` unsafe and parallel tests racy).
fn resolve_config_path(override_path: Option<String>) -> String {
    override_path
        .filter(|path| !path.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_string())
}

/// Compiled default for [`ContextConfig::embedding_dim`].
///
/// A bare `#[serde(default)]` would resolve to `usize::default() == 0` for
/// legacy TOML files that omit the key, which would create a zero-sized Qdrant
/// collection. A named serde default keeps legacy deployments on 384
/// (`sentence-transformers/all-MiniLM-L6-v2`) until they explicitly set
/// `JARVIS_CONTEXT_EMBEDDING_DIM`.
fn default_embedding_dim() -> usize {
    384
}

/// Compare two token byte strings without an early exit on the first mismatch.
///
/// The loop covers the longer input and pads the shorter one, so neither a
/// content mismatch nor a length mismatch short-circuits. Shared by the HTTP
/// middleware and the gRPC ingress gate; no extra crate and no `unsafe` is
/// needed.
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
pub struct ContextConfig {
    pub grpc_addr: String,
    pub http_addr: String,
    pub pg_url: String,
    pub qdrant_url: String,
    pub qdrant_api_key: Option<String>,
    pub collection_name: String,
    pub embedding_model: String,
    /// Vector size of the Qdrant collection. Must match the output dimension
    /// of `embedding_model`; changing only the model leaves a mismatched
    /// collection that fails on upsert/search.
    #[serde(default = "default_embedding_dim")]
    pub embedding_dim: usize,
    pub embedding_url: Option<String>,
    pub pii_patterns_path: String,
    pub max_context_tokens: usize,
    /// Optional shared secret for the HTTP API. When set via
    /// `JARVIS_CONTEXT_ADMIN_TOKEN`, every API route except `/health` requires
    /// it in the `x-jarvis-context-token` header. Unset keeps loopback clients
    /// working unchanged.
    #[serde(default)]
    pub admin_token: Option<String>,
}

impl ContextConfig {
    /// Load from the absolute [`DEFAULT_CONFIG_PATH`], or from the path in
    /// [`CONFIG_PATH_ENV`] when it is set and non-blank, with `JARVIS_CONTEXT_*`
    /// environment overrides applied on top.
    ///
    /// The default file may be absent on real hosts: configuration is supplied
    /// by the `JARVIS_CONTEXT_*` Env provider (`bws run` / systemd), so the
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
        let config: ContextConfig = Figment::new()
            .merge(Toml::file(path))
            // Flat snake_case fields: do NOT use figment's Env split, which
            // would turn JARVIS_CONTEXT_GRPC_ADDR into the nested key `grpc.addr`.
            // The config-path variable only selects the file, so keep it out
            // of the field provider as well.
            .merge(Env::prefixed(ENV_PREFIX).ignore(&["config_path"]))
            .extract()?;
        Ok(config)
    }

    pub fn pg_url(&self) -> &str {
        &self.pg_url
    }

    pub fn qdrant_api_key(&self) -> Option<&str> {
        // An empty JARVIS_CONTEXT_QDRANT_API_KEY means "no key" (the shipped
        // .env template sets the variable to an empty value).
        self.qdrant_api_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
    }

    /// An empty or whitespace-only `JARVIS_CONTEXT_ADMIN_TOKEN` means "no
    /// token", matching the `qdrant_api_key` convention and keeping legacy
    /// deployments unauthenticated.
    pub fn admin_token(&self) -> Option<&str> {
        self.admin_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
    }
}

/// Redacted `Debug`: a derived implementation would print `admin_token` into
/// any config dump. Both transports must never emit the token value to logs.
impl std::fmt::Debug for ContextConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextConfig")
            .field("grpc_addr", &self.grpc_addr)
            .field("http_addr", &self.http_addr)
            .field("pg_url", &self.pg_url)
            .field("qdrant_url", &self.qdrant_url)
            .field("qdrant_api_key", &"<redacted>")
            .field("collection_name", &self.collection_name)
            .field("embedding_model", &self.embedding_model)
            .field("embedding_dim", &self.embedding_dim)
            .field("embedding_url", &self.embedding_url)
            .field("pii_patterns_path", &self.pii_patterns_path)
            .field("max_context_tokens", &self.max_context_tokens)
            .field("admin_token", &"<redacted>")
            .finish()
    }
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            grpc_addr: "127.0.0.1:50054".into(),
            http_addr: "127.0.0.1:8084".into(),
            // No compiled-in credentials: authentication is delegated to libpq
            // (PGUSER / PGPASSWORD / .pgpass / IAM ...). Deployments supply the
            // full URL via JARVIS_CONTEXT_PG_URL or /etc/jarvis/context.toml.
            pg_url: "postgres://localhost:5432/jarvis".into(),
            qdrant_url: "http://127.0.0.1:6334".into(),
            qdrant_api_key: None,
            collection_name: "jarvis_memories".into(),
            embedding_model: "sentence-transformers/all-MiniLM-L6-v2".into(),
            embedding_dim: default_embedding_dim(),
            embedding_url: None,
            pii_patterns_path: "/etc/jarvis/pii_patterns.toml".into(),
            max_context_tokens: 8000,
            admin_token: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::providers::Serialized;
    use std::path::Path;

    /// A config identical to a legacy deployment: no `admin_token` key at all.
    const LEGACY_TOML: &str = r#"
grpc_addr = "127.0.0.1:50054"
http_addr = "127.0.0.1:8084"
pg_url = "postgres://localhost:5432/jarvis"
qdrant_url = "http://127.0.0.1:6334"
collection_name = "jarvis_memories"
embedding_model = "sentence-transformers/all-MiniLM-L6-v2"
pii_patterns_path = "/etc/jarvis/pii_patterns.toml"
max_context_tokens = 8000
"#;

    #[test]
    fn legacy_config_without_admin_token_still_loads() {
        let config: ContextConfig = Figment::from(Toml::string(LEGACY_TOML))
            .extract()
            .expect("legacy config parses");
        assert!(config.admin_token.is_none());
        assert!(config.admin_token().is_none());
        assert_eq!(ContextConfig::default().admin_token, None);
    }

    #[test]
    fn admin_token_from_toml_and_blank_value_disables_auth() {
        let with_token = format!("{LEGACY_TOML}\nadmin_token = \"admin-secret\"\n");
        let config: ContextConfig = Figment::from(Toml::string(&with_token))
            .extract()
            .expect("config with token parses");
        assert_eq!(config.admin_token(), Some("admin-secret"));

        let blank = format!("{LEGACY_TOML}\nadmin_token = \"  \"\n");
        let config: ContextConfig = Figment::from(Toml::string(&blank))
            .extract()
            .expect("blank token parses");
        assert!(
            config.admin_token().is_none(),
            "a blank token is treated as unset"
        );
    }

    /// `JARVIS_CONTEXT_ADMIN_TOKEN` becomes `admin_token` through the same
    /// `Env::prefixed("JARVIS_CONTEXT_")` chain used by `load()`. No OS env var
    /// is mutated; the env-style key is merged as a figment value.
    #[test]
    fn admin_token_env_key_maps_to_field() {
        const ENV_PREFIX: &str = "JARVIS_CONTEXT_";
        const ENV_KEY: &str = "JARVIS_CONTEXT_ADMIN_TOKEN";

        let field = ENV_KEY
            .strip_prefix(ENV_PREFIX)
            .expect("key uses the context env prefix")
            .to_ascii_lowercase();
        assert_eq!(field, "admin_token");

        let config: ContextConfig = Figment::from(Toml::string(LEGACY_TOML))
            .merge(Serialized::default(field.as_str(), "env-secret"))
            .extract()
            .expect("env-style key merges into the admin token field");
        assert_eq!(config.admin_token(), Some("env-secret"));
    }

    /// The gRPC metadata key must match the HTTP header literal exactly;
    /// otherwise the two transports would enforce different secrets.
    #[test]
    fn context_token_header_is_the_shared_header_name() {
        assert_eq!(CONTEXT_TOKEN_HEADER, "x-jarvis-context-token");
    }

    #[test]
    fn token_matches_enforces_exact_value_and_keeps_legacy_open_path() {
        assert!(token_matches(None, None), "no configured token stays open");
        assert!(token_matches(None, Some("anything")));
        assert!(token_matches(Some("  "), None), "blank config stays open");
        assert!(!token_matches(Some("context-secret"), None));
        assert!(token_matches(
            Some("context-secret"),
            Some("context-secret")
        ));
        assert!(!token_matches(
            Some("context-secret"),
            Some("Context-secret")
        ));
        assert!(!token_matches(
            Some("context-secret"),
            Some("context-secret-extra")
        ));
        assert!(!token_matches(Some("context-secret"), Some("")));
    }

    #[test]
    fn debug_output_never_contains_the_admin_token() {
        let config = ContextConfig {
            admin_token: Some("super-secret".into()),
            ..Default::default()
        };
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("super-secret"));
        assert!(rendered.contains("admin_token: \"<redacted>\""));
    }

    /// The compiled-in default must never embed a password. Auth is delegated
    /// to libpq; a hardcoded `user:password@` here would leak into every config
    /// dump and test snapshot. Regression guard for I12-D.
    #[test]
    fn default_pg_url_has_no_embedded_credentials() {
        let config = ContextConfig::default();
        let url = config.pg_url();
        assert_eq!(url, "postgres://localhost:5432/jarvis");
        assert!(
            !url.contains('@'),
            "default pg_url must not carry userinfo/password; delegate to libpq",
        );
    }

    /// `JARVIS_CONTEXT_PG_URL` must keep overriding the configured URL through
    /// the same `Env::prefixed("JARVIS_CONTEXT_")` chain used by `load()`. No
    /// OS env var is mutated; the env-style key is merged as a figment value.
    #[test]
    fn pg_url_env_key_overrides_configured_url() {
        const ENV_PREFIX: &str = "JARVIS_CONTEXT_";
        const ENV_KEY: &str = "JARVIS_CONTEXT_PG_URL";

        let field = ENV_KEY
            .strip_prefix(ENV_PREFIX)
            .expect("key uses the context env prefix")
            .to_ascii_lowercase();
        assert_eq!(field, "pg_url");

        let override_url = "postgres://override-host:5432/override_db";
        let config: ContextConfig = Figment::from(Toml::string(LEGACY_TOML))
            .merge(Serialized::default(field.as_str(), override_url))
            .extract()
            .expect("env-style key merges into the pg_url field");
        assert_eq!(config.pg_url(), override_url);
    }

    /// An explicitly configured URL is preserved verbatim; only the compiled
    /// default is passwordless. This guards against an over-broad change that
    /// would reject legitimate operator-supplied libpq connection strings.
    #[test]
    fn explicit_pg_url_from_toml_is_preserved() {
        let toml = LEGACY_TOML.replace(
            "pg_url = \"postgres://localhost:5432/jarvis\"",
            "pg_url = \"postgres://db.internal:5432/custom_db\"",
        );
        assert_ne!(toml.as_str(), LEGACY_TOML, "fixture replacement must apply");
        let config: ContextConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("explicit pg_url parses");
        assert_eq!(config.pg_url(), "postgres://db.internal:5432/custom_db");
    }

    /// A legacy config (no `embedding_dim` key) must still land on 384 rather
    /// than `usize::default() == 0`, otherwise the collection would be created
    /// with a zero-sized vector.
    #[test]
    fn legacy_config_without_embedding_dim_defaults_to_384() {
        let config: ContextConfig = Figment::from(Toml::string(LEGACY_TOML))
            .extract()
            .expect("legacy config parses");
        assert_eq!(config.embedding_dim, 384);
        assert_eq!(ContextConfig::default().embedding_dim, 384);
    }

    #[test]
    fn embedding_dim_from_toml_is_preserved() {
        let toml = format!("{LEGACY_TOML}\nembedding_dim = 768\n");
        let config: ContextConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("explicit embedding_dim parses");
        assert_eq!(config.embedding_dim, 768);
    }

    /// `JARVIS_CONTEXT_EMBEDDING_DIM` reaches the field through the same
    /// `Env::prefixed("JARVIS_CONTEXT_")` chain as `load()`. Figment's Env
    /// parses an unsigned-integer string into a numeric value, so a numeric
    /// figment value models the env override without mutating the OS env.
    #[test]
    fn embedding_dim_env_key_maps_to_field() {
        const ENV_PREFIX: &str = "JARVIS_CONTEXT_";
        const ENV_KEY: &str = "JARVIS_CONTEXT_EMBEDDING_DIM";

        let field = ENV_KEY
            .strip_prefix(ENV_PREFIX)
            .expect("key uses the context env prefix")
            .to_ascii_lowercase();
        assert_eq!(field, "embedding_dim");

        let config: ContextConfig = Figment::from(Toml::string(LEGACY_TOML))
            .merge(Serialized::default(field.as_str(), 768u64))
            .extract()
            .expect("env-style key merges into embedding_dim");
        assert_eq!(config.embedding_dim, 768);
    }

    /// The service must find its config regardless of the launch directory;
    /// the former `config/context.toml` was CWD-relative.
    #[test]
    fn default_config_path_is_absolute() {
        assert!(Path::new(DEFAULT_CONFIG_PATH).is_absolute());
        assert_eq!(DEFAULT_CONFIG_PATH, "/etc/jarvis/context.toml");
    }

    #[test]
    fn config_path_override_is_honoured() {
        const OVERRIDE: &str = "/tmp/jarvis-context-custom.toml";
        assert_eq!(resolve_config_path(Some(OVERRIDE.into())), OVERRIDE);
        assert_eq!(resolve_config_path(None), DEFAULT_CONFIG_PATH);
        assert_eq!(resolve_config_path(Some("   ".into())), DEFAULT_CONFIG_PATH);
        assert_eq!(
            resolve_config_path(Some(String::new())),
            DEFAULT_CONFIG_PATH
        );
    }

    /// The override variable shares the config prefix, so verify it maps to
    /// the `config_path` key that `load_from_path` filters out of the provider.
    #[test]
    fn config_path_env_name_matches_prefix() {
        let field = CONFIG_PATH_ENV
            .strip_prefix(ENV_PREFIX)
            .expect("override variable uses the context env prefix")
            .to_ascii_lowercase();
        assert_eq!(field, "config_path");
    }
}
