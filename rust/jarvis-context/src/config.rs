//! Configuration for Context Builder

use anyhow::Result;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    pub grpc_addr: String,
    pub http_addr: String,
    pub pg_url: String,
    pub qdrant_url: String,
    pub qdrant_api_key: Option<String>,
    pub collection_name: String,
    pub embedding_model: String,
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
    pub fn load() -> Result<Self> {
        let config: ContextConfig = Figment::new()
            .merge(Toml::file("config/context.toml"))
            // Flat snake_case fields: do NOT use figment's Env split, which
            // would turn JARVIS_CONTEXT_GRPC_ADDR into the nested key `grpc.addr`.
            .merge(Env::prefixed("JARVIS_CONTEXT_"))
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

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            grpc_addr: "127.0.0.1:50054".into(),
            http_addr: "127.0.0.1:8084".into(),
            pg_url: "postgres://jarvis:jarvis@localhost:5432/jarvis".into(),
            qdrant_url: "http://127.0.0.1:6334".into(),
            qdrant_api_key: None,
            collection_name: "jarvis_memories".into(),
            embedding_model: "sentence-transformers/all-MiniLM-L6-v2".into(),
            embedding_url: None,
            pii_patterns_path: "/etc/jarvis/pii_patterns.toml".into(),
            max_context_tokens: 8000,
            admin_token: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ContextConfig;
    use figment::{
        Figment,
        providers::{Format, Serialized, Toml},
    };

    /// A config identical to a legacy deployment: no `admin_token` key at all.
    const LEGACY_TOML: &str = r#"
grpc_addr = "127.0.0.1:50054"
http_addr = "127.0.0.1:8084"
pg_url = "postgres://jarvis:jarvis@localhost:5432/jarvis"
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
}
