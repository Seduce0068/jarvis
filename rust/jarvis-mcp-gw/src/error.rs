//! Error types for MCP Gateway

use thiserror::Error;

/// Transport-neutral classification of a gateway failure.
///
/// HTTP/gRPC adapters map this to their own status codes instead of parsing
/// the human-readable message, so rewording an error cannot silently change
/// the API contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Caller mistake (malformed arguments, oversized payload, bad input).
    ClientError,
    /// The caller is authenticated but not permitted to perform the action.
    Forbidden,
    /// Gateway-side failure the caller cannot fix by changing its input.
    Internal,
}

#[derive(Error, Debug)]
pub enum McpGatewayError {
    #[error("Tool not in allowlist: {0}")]
    NotAllowed(String),

    #[error("Arguments for tool '{tool}' exceed {max} bytes ({actual} bytes)")]
    ArgsTooLarge {
        tool: String,
        max: usize,
        actual: usize,
    },

    /// A caller-supplied argument failed validation (native handlers).
    /// The original wording is preserved for the response body, but the
    /// status is derived from the variant, not from this text.
    #[error("{0}")]
    ClientError(String),

    #[error("Policy denied tool '{tool}' for principal '{principal}'")]
    PolicyDenied { tool: String, principal: String },

    #[error("Principal '{principal}' is not allowed to execute tool '{tool}'")]
    PrincipalNotAllowed { principal: String, tool: String },

    #[error(
        "Tool '{tool}' requires human approval, but the approval request failed; denying (fail-closed)"
    )]
    ApprovalRequired { tool: String },

    #[error("Tool '{tool}' was not approved: {reason}")]
    ApprovalDenied { tool: String, reason: String },

    #[error("Sandbox error: {0}")]
    Sandbox(String),

    #[error("MCP protocol error: {0}")]
    Mcp(#[from] rmcp::ErrorData),

    #[error("Configuration error: {0}")]
    Config(#[from] anyhow::Error),

    #[error("gRPC error: {0}")]
    Grpc(#[from] tonic::Status),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

impl McpGatewayError {
    /// Classify this error for transport adapters without reading its text.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::ArgsTooLarge { .. } | Self::ClientError(_) => ErrorKind::ClientError,
            Self::NotAllowed(_)
            | Self::PolicyDenied { .. }
            | Self::PrincipalNotAllowed { .. }
            | Self::ApprovalRequired { .. }
            | Self::ApprovalDenied { .. } => ErrorKind::Forbidden,
            Self::Sandbox(_)
            | Self::Mcp(_)
            | Self::Config(_)
            | Self::Grpc(_)
            | Self::Serialization(_) => ErrorKind::Internal,
        }
    }
}

pub type Result<T> = std::result::Result<T, McpGatewayError>;
