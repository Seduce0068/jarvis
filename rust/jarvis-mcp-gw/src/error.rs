//! Error types for MCP Gateway

use thiserror::Error;

#[derive(Error, Debug)]
pub enum McpGatewayError {
    #[error("Tool not in allowlist: {0}")]
    NotAllowed(String),

    #[error("Principal '{principal}' is not allowed to execute tool '{tool}'")]
    PrincipalNotAllowed { principal: String, tool: String },

    #[error("Sandbox error: {0}")]
    Sandbox(String),

    #[error("MCP protocol error: {0}")]
    Mcp(#[from] rmcp::ErrorData),

    #[error("Policy error: {0}")]
    Policy(#[from] jarvis_policy::error::PolicyError),

    #[error("Configuration error: {0}")]
    Config(#[from] anyhow::Error),

    #[error("gRPC error: {0}")]
    Grpc(#[from] tonic::Status),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, McpGatewayError>;
