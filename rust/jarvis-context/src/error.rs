//! Error types for Context Builder

use thiserror::Error;

#[derive(Error, Debug)]
pub enum ContextError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("Qdrant error: {0}")]
    Qdrant(String),

    #[error("PII detection error: {0}")]
    Pii(String),

    #[error("Configuration error: {0}")]
    Config(#[from] anyhow::Error),

    #[error("gRPC error: {0}")]
    Grpc(#[from] tonic::Status),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, ContextError>;
