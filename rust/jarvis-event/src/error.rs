//! Error types for Event Normalizer

use thiserror::Error;

#[derive(Error, Debug)]
pub enum EventError {
    #[error("Home Assistant error: {0}")]
    HomeAssistant(String),

    #[error("Android MCP error: {0}")]
    AndroidMcp(String),

    #[error("Policy error: {0}")]
    Policy(#[from] jarvis_policy::error::PolicyError),

    #[error("Context error: {0}")]
    Context(#[from] jarvis_context::error::ContextError),

    #[error("Configuration error: {0}")]
    Config(#[from] anyhow::Error),

    #[error("gRPC error: {0}")]
    Grpc(#[from] tonic::Status),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
}

pub type Result<T> = std::result::Result<T, EventError>;
