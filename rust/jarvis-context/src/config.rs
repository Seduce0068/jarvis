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
    pub policy_grpc_addr: String,
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
            policy_grpc_addr: "http://127.0.0.1:50051".into(),
        }
    }
}
