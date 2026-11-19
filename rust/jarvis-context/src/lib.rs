//! JARVIS Context Builder - Main library

pub mod builder;
pub mod config;
pub mod embedding;
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
}

impl ContextBuilder {
    pub async fn new(config: config::ContextConfig) -> Result<Self> {
        let pg_pool = Arc::new(builder::memory::create_pg_pool(&config).await?);
        let qdrant_client = Arc::new(builder::memory::create_qdrant_client(&config).await?);
        let pii_filter = Arc::new(pii::PiiFilter::new(&config).await?);
        let builder = Arc::new(
            builder::Builder::new(
                config.clone(),
                pg_pool.clone(),
                qdrant_client.clone(),
                pii_filter.clone(),
            )
            .await?,
        );

        // (a/P1-2) Retry vector upserts that were queued after a Qdrant
        // failure. Reconcile never fails startup: it warns and leaves failed
        // rows in `pending_embeddings` for the next start.
        builder.reconcile_pending_embeddings().await;

        Ok(Self {
            config,
            builder,
            pg_pool,
            qdrant_client,
            pii_filter,
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
