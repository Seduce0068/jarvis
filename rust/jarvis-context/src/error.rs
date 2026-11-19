//! Error types for Context Builder

use thiserror::Error;

/// Fixed-vocabulary category for Qdrant failures at the `ContextError` boundary.
///
/// The token is a *private field*: neither sibling modules nor external crates
/// can build a `QdrantErrorKind` from a `String`. A raw `qdrant_client` error's
/// `Display` can embed endpoint URLs and response fragments; because the only
/// construction path is the crate-private `ContextError::qdrant` constructor
/// taking this type, no such text can be stored in [`ContextError::Qdrant`] and
/// then reach stderr/journal/OTLP through `Builder::new()`'s `?` propagation or
/// the HTTP/gRPC transports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QdrantErrorKind {
    token: &'static str,
}

impl QdrantErrorKind {
    /// The single category produced by this crate.
    ///
    /// Crate-private for the same reason as the field: only in-crate call sites
    /// may obtain a kind to pass to `ContextError::qdrant`.
    pub(crate) const FAILED: Self = Self { token: "qdrant" };

    /// Stable token shared with the builder/transport `error_kind` vocabulary.
    pub const fn as_str(self) -> &'static str {
        self.token
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
    /// The payload is [`QdrantErrorKind`], whose inner token is private, so the
    /// only construction path is the crate-private `ContextError::qdrant`
    /// constructor. Its `Display` is the fixed token `qdrant`; no client error,
    /// endpoint or response fragment can enter the error chain here. The
    /// variant stays public so read-only matches such as `Qdrant(_)` in the
    /// transport `error_kind` mappers keep compiling unchanged.
    #[error("Qdrant error: {0}")]
    Qdrant(QdrantErrorKind),

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
    /// The raw client error type is not even accepted as an argument, and the
    /// kind's token is private, so no endpoint, response fragment or formatted
    /// message can be stored here.
    pub(crate) fn qdrant(kind: QdrantErrorKind) -> Self {
        Self::Qdrant(kind)
    }
}

pub type Result<T> = std::result::Result<T, ContextError>;

#[cfg(test)]
mod tests {
    use super::{ContextError, QdrantErrorKind};

    /// The newtype is the boundary: its only value renders as the fixed token,
    /// never as client-supplied text.
    #[test]
    fn qdrant_error_carries_only_fixed_vocabulary() {
        let err = ContextError::qdrant(QdrantErrorKind::FAILED);
        assert_eq!(QdrantErrorKind::FAILED.as_str(), "qdrant");
        assert_eq!(err.to_string(), "Qdrant error: qdrant");
        assert!(!err.to_string().contains("http://"));
    }
}
