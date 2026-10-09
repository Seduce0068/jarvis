//! Configuration for Context Builder

use anyhow::Result;
use figment::{Figment, providers::{Toml, Env}};
use serde::{Deserialize, Serialize};
use secrecy::SecretString;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    pub grpc_addr: String,
    pub http_addr: String,
    pub pg_url: SecretString,
    pub qdrant_url: String,
    pub qdrant_api_key: Option<SecretString>,
    pub collection_name: String,
    pub embedding_model: String,
    pub pii_patterns_path: String,
    pub max_context_tokens: usize,
    pub policy_grpc_addr: String,
}

impl ContextConfig {
    pub fn load() -> Result<Self> {
        let config: ContextConfig = Figment::new()
            .merge(Toml::file("config/context.toml"))
            .merge(Env::prefixed("JARVIS_CONTEXT_").split("_"))
            .extract()?;
        Ok(config)
    }
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            grpc_addr: "127.0.0.1:50054".into(),
            http_addr: "127.0.0.1:8084".into(),
            pg_url: SecretString::new("postgres://jarvis:jarvis@localhost:5432/jarvis".into()),
            qdrant_url: "http://127.0.0.1:6334".into(),
            qdrant_api_key: None,
            collection_name: "jarvis_memories".into(),
            embedding_model: "sentence-transformers/all-MiniLM-L6-v2".into(),
            pii_patterns_path: "/etc/jarvis/pii_patterns.toml".into(),
            max_context_tokens: 8000,
            policy_grpc_addr: "http://127.0.0.1:50051".into(),
        }
    }
}