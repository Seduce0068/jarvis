//! Error types for Policy Engine

use thiserror::Error;

#[derive(Error, Debug)]
pub enum PolicyError {
    #[error("Cedar evaluation error: {0}")]
    Cedar(String),

    #[error("Casbin enforcement error: {0}")]
    Casbin(#[from] casbin::Error),

    #[error("Kill switch activated: {0}")]
    KillSwitch(String),

    #[error("Approval timeout")]
    ApprovalTimeout,

    #[error("Approval denied: {0}")]
    ApprovalDenied(String),

    #[error("Invalid principal: {0}")]
    InvalidPrincipal(String),

    #[error("Invalid resource: {0}")]
    InvalidResource(String),

    #[error("Configuration error: {0}")]
    Config(#[from] anyhow::Error),

    #[error("gRPC error: {0}")]
    Grpc(#[from] tonic::Status),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, PolicyError>;
