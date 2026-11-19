//! Error types for Context Builder

use thiserror::Error;

/// Fixed-vocabulary category for Qdrant failures at the `ContextError` boundary.
///
/// A raw `qdrant_client` error is never accepted here: its `Display` can embed
/// endpoint URLs and response fragments. Only this single fixed token can be
/// turned into the text carried by [`ContextError::Qdrant`], so a formatted
/// client error cannot reach stderr/journal/OTLP through `Builder::new()`'s
/// `?` propagation or the HTTP/gRPC transports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QdrantErrorKind {
    /// Any Qdrant client failure, reduced to a non-sensitive category.
    Failed,
}

impl QdrantErrorKind {
    /// Stable token shared with the builder/transport `error_kind` vocabulary.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Failed => "qdrant",
        }
    }
}

impl std::fmt::Display for QdrantErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Error, Debug)]
pub enum ContextError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    /// A Qdrant failure carrying fixed vocabulary only.
    ///
    /// Production code must build this with [`ContextError::qdrant`]: the raw
    /// client error is discarded before that constructor is reached. The
    /// `String` field is retained for source compatibility with the existing
    /// gRPC log-safety test, which deliberately injects a hostile string to
    /// prove the transport never echoes error text. No builder call site may
    /// construct this variant directly.
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

impl ContextError {
    /// Construct a Qdrant failure from the fixed vocabulary.
    ///
    /// The raw client error type is not even accepted as an argument, so no
    /// endpoint, response fragment or formatted message can be stored here.
    pub(crate) fn qdrant(kind: QdrantErrorKind) -> Self {
        Self::Qdrant(kind.as_str().to_owned())
    }
}

pub type Result<T> = std::result::Result<T, ContextError>;
