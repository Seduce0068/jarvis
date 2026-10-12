//! JARVIS Context Builder - Main library

pub mod builder;
pub mod config;
pub mod error;
pub mod grpc;
pub mod http;
pub mod pii;

use anyhow::Result;
use qdrant_client::Qdrant;
use std::sync::Arc;

#[derive(Clone)]
#[allow(dead_code)]
pub struct ContextBuilder {
    config: config::ContextConfig,
    builder: Arc<builder::Builder>,
    pg_pool: Arc<deadpool_postgres::Pool>,
    qdrant_client: Arc<Qdrant>,
    pii_filter: Arc<pii::PiiFilter>,
    policy_client: Arc<jarvis_policy::PolicyEngine>,
}

impl ContextBuilder {
    pub async fn new(config: config::ContextConfig) -> Result<Self> {
        let pg_pool = Arc::new(builder::memory::create_pg_pool(&config).await?);
        let qdrant_client = Arc::new(builder::memory::create_qdrant_client(&config).await?);
        let pii_filter = Arc::new(pii::PiiFilter::new(&config).await?);
        let policy_client = Arc::new(
            jarvis_policy::PolicyEngine::new(jarvis_policy::config::PolicyConfig::load()?).await?,
        );
        let builder = Arc::new(
            builder::Builder::new(
                config.clone(),
                pg_pool.clone(),
                qdrant_client.clone(),
                pii_filter.clone(),
                policy_client.clone(),
            )
            .await?,
        );

        Ok(Self {
            config,
            builder,
            pg_pool,
            qdrant_client,
            pii_filter,
            policy_client,
        })
    }

    pub async fn build_context(
        &self,
        event: &jarvis_core::UnifiedEvent,
    ) -> Result<serde_json::Value> {
        self.builder.build_context(event).await
    }

    pub async fn store_memory(&self, memory: jarvis_core::MemoryEntry) -> Result<()> {
        self.builder.store_memory(memory).await
    }

    pub async fn search_memory(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<jarvis_core::MemoryEntry>> {
        self.builder.search_memory(query, limit).await
    }

    pub async fn serve_grpc(&self, addr: std::net::SocketAddr) -> Result<()> {
        grpc::serve(self.clone(), addr).await
    }

    pub async fn serve_http(&self, addr: std::net::SocketAddr) -> Result<()> {
        http::serve(Arc::new(self.clone()), addr).await
    }
}
