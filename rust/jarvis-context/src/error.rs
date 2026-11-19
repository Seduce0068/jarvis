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

/// Test-only fixture shared by the HTTP and gRPC transport tests (I55-C).
///
/// Keeping the seven representative errors in one place is what lets a single
/// cross-cutting test drive all six RPCs with the *same* inputs. A transport
/// that silently changes its mapping can no longer pass by leaving an old
/// string literal in its source: the tests compare real return values and
/// real log records produced by these errors.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{ContextError, QdrantErrorKind};

    /// Marker embedded in every error chain; it must never reach a status
    /// message or a log record. Deliberately shaped to avoid the repository
    /// secret scanner's credential-key patterns.
    pub(crate) const SENSITIVE_PROBE: &str = "I55C-VOCAB-PROBE-DO-NOT-LOG";

    /// The seven fixed categories both transports must produce.
    pub(crate) const ERROR_KIND_TOKENS: [&str; 7] = [
        "database",
        "qdrant",
        "pii",
        "config",
        "grpc",
        "serialization",
        "other",
    ];

    /// Selector for exactly one representative of each category.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum ErrorCase {
        Database,
        Qdrant,
        Pii,
        Config,
        Grpc,
        Serialization,
        Other,
    }

    impl ErrorCase {
        pub(crate) const ALL: [Self; 7] = [
            Self::Database,
            Self::Qdrant,
            Self::Pii,
            Self::Config,
            Self::Grpc,
            Self::Serialization,
            Self::Other,
        ];

        pub(crate) fn token(self) -> &'static str {
            match self {
                Self::Database => "database",
                Self::Qdrant => "qdrant",
                Self::Pii => "pii",
                Self::Config => "config",
                Self::Grpc => "grpc",
                Self::Serialization => "serialization",
                Self::Other => "other",
            }
        }

        /// Build a real error of this category with the probe in its chain.
        ///
        /// The outer `context` layer proves that a typed variant is still found
        /// by `downcast_ref` after `anyhow` wrapping (matching the existing
        /// no-leak regressions) while the probe itself must stay unlogged.
        pub(crate) fn into_error(self) -> anyhow::Error {
            let err: anyhow::Error = match self {
                Self::Database => ContextError::Database(sqlx::Error::RowNotFound).into(),
                Self::Qdrant => ContextError::qdrant(QdrantErrorKind::FAILED).into(),
                Self::Pii => ContextError::Pii(SENSITIVE_PROBE.to_owned()).into(),
                Self::Config => ContextError::Config(anyhow::anyhow!("{SENSITIVE_PROBE}")).into(),
                Self::Grpc => ContextError::Grpc(tonic::Status::internal(SENSITIVE_PROBE)).into(),
                Self::Serialization => ContextError::Serialization(
                    serde_json::from_str::<serde_json::Value>(SENSITIVE_PROBE)
                        .expect_err("the probe is not JSON"),
                )
                .into(),
                Self::Other => anyhow::anyhow!("{SENSITIVE_PROBE}"),
            };
            err.context(SENSITIVE_PROBE)
        }
    }

    /// Assert that `logs` holds exactly one record whose structured fields carry
    /// both `operation=<operation>` and `error_kind=<kind>`.
    ///
    /// Each RPC is driven with its own subscriber/buffer so the (operation,
    /// category) pair is checked in isolation. An aggregate token count (the old
    /// `matches(token).count() == 3` assertion) cannot detect one RPC emitting a
    /// wrong category while another emits the expected token twice; counting
    /// records that carry *both* fields per RPC can. Quotes are stripped so the
    /// check stays stable across `tracing-subscriber` formatter versions.
    pub(crate) fn assert_single_operation_kind(
        logs: &str,
        operation: &str,
        kind: &str,
        label: &str,
    ) {
        let normalized = logs.replace('"', "");
        let op_field = format!("operation={operation}");
        let kind_field = format!("error_kind={kind}");
        let has_field = |field: &str, line: &str| -> bool {
            line.match_indices(field).any(|(idx, _)| {
                let rest = &line[idx + field.len()..];
                rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace())
            })
        };
        let count = normalized
            .lines()
            .filter(|line| {
                has_field(op_field.as_str(), line) && has_field(kind_field.as_str(), line)
            })
            .count();
        assert_eq!(
            count, 1,
            "{label}: expected exactly one log record with '{op_field}' + '{kind_field}', found {count}: {logs}"
        );
    }
}
