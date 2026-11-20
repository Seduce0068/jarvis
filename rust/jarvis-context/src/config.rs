//! Configuration for Context Builder

use anyhow::Result;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use serde::{Deserialize, Serialize};

/// Absolute default location of the context TOML. The former CWD-relative
/// `config/context.toml` was not found when the service was started outside
/// the repository root.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/jarvis/context.toml";

/// Environment variable that overrides [`DEFAULT_CONFIG_PATH`].
pub const CONFIG_PATH_ENV: &str = "JARVIS_CONTEXT_CONFIG_PATH";

const ENV_PREFIX: &str = "JARVIS_CONTEXT_";

/// HTTP header and gRPC metadata key carrying the shared admin token.
///
/// The HTTP middleware in `http.rs` uses the same literal. Keeping it here lets
/// the gRPC ingress gate reference one canonical name without reaching into a
/// sibling module's private constant.
pub(crate) const CONTEXT_TOKEN_HEADER: &str = "x-jarvis-context-token";

/// Select the TOML path from an optional environment override. Kept pure so
/// both branches are testable without mutating the process environment
/// (edition 2024 makes `set_var` unsafe and parallel tests racy).
fn resolve_config_path(override_path: Option<String>) -> String {
    override_path
        .filter(|path| !path.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CONFIG_PATH.to_string())
}

/// Compiled default for [`ContextConfig::embedding_dim`].
///
/// A bare `#[serde(default)]` would resolve to `usize::default() == 0` for
/// legacy TOML files that omit the key, which would create a zero-sized Qdrant
/// collection. A named serde default keeps legacy deployments on 384
/// (`sentence-transformers/all-MiniLM-L6-v2`) until they explicitly set
/// `JARVIS_CONTEXT_EMBEDDING_DIM`.
fn default_embedding_dim() -> usize {
    384
}

/// Compare two token byte strings without an early exit on the first mismatch.
///
/// The loop covers the longer input and pads the shorter one, so neither a
/// content mismatch nor a length mismatch short-circuits. Shared by the HTTP
/// middleware and the gRPC ingress gate; no extra crate and no `unsafe` is
/// needed.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let max = a.len().max(b.len());
    let mut diff = a.len() ^ b.len();
    for (x, y) in a
        .iter()
        .map(Some)
        .chain(std::iter::repeat(None))
        .zip(b.iter().map(Some).chain(std::iter::repeat(None)))
        .take(max)
    {
        diff |= (x.copied().unwrap_or(0) ^ y.copied().unwrap_or(0)) as usize;
    }
    diff == 0
}

/// Authorize a presented token against the configured value.
///
/// A missing/blank configured token keeps the legacy unauthenticated loopback
/// path. A missing or non-UTF-8 presented value is never valid when a token is
/// configured. Neither value is logged.
pub(crate) fn token_matches(expected: Option<&str>, presented: Option<&str>) -> bool {
    let Some(expected) = expected.map(str::trim).filter(|t| !t.is_empty()) else {
        return true;
    };
    let Some(presented) = presented else {
        return false;
    };
    constant_time_eq(expected, presented)
}

/// Shared ingress validation for the HTTP and gRPC boundaries (I55-A).
///
/// Every rule below lives in exactly one place so the two transports cannot
/// drift: HTTP maps [`InvalidInput`] to a bare 400 and gRPC maps it to
/// `InvalidArgument`, both through their own `reject_invalid_input` wrapper.
/// The helper returns only a static `field` label — never the offending bytes,
/// vector contents or timestamp — so the fixed-vocabulary `invalid_input` log
/// record is identical on both ingresses and leaks no caller data.
pub(crate) mod validation {
    use chrono::{DateTime, Utc};
    use jarvis_core::MemoryEntry;

    /// Upper bound on a caller-supplied `search_memory` limit, shared by both
    /// transports.
    ///
    /// Without a common ceiling a negative gRPC `int32` was widened with `as`
    /// into `usize::MAX` and reached Qdrant through the over-fetch path, while
    /// HTTP rejected the same value during deserialization. One explicit
    /// bounded constant keeps both ingresses fail-closed; `try_from` handles
    /// the sign and this constant handles the magnitude.
    pub(crate) const MAX_SEARCH_LIMIT: usize = 1_000;

    /// Maximum UTF-8 byte length of a caller-supplied `content` (I57-C).
    ///
    /// `content` is stored verbatim in PostgreSQL and can be rendered into JSON
    /// logs; without a bound one request can exhaust row storage or the log
    /// pipeline. 1 MiB is generous for a single memory entry.
    pub(crate) const MAX_CONTENT_BYTES: usize = 1_048_576;

    /// Maximum UTF-8 byte length of a caller-supplied `source` (I57-C).
    ///
    /// `source` is a short provenance label, not a document body.
    pub(crate) const MAX_SOURCE_BYTES: usize = 4_096;

    /// Maximum UTF-8 byte length of the raw `metadata` JSON string (I57-C).
    ///
    /// Parsed metadata becomes a Qdrant payload; bounding the raw bytes rejects
    /// oversized payloads before a large `serde_json::Value` is allocated.
    pub(crate) const MAX_METADATA_BYTES: usize = 65_536;

    /// Maximum nesting depth of the parsed `metadata` object (I57-C).
    ///
    /// Deeply nested objects inflate Qdrant payload serialization and JSON log
    /// size without bound even when the total byte count is modest.
    pub(crate) const MAX_METADATA_DEPTH: usize = 16;

    /// The only information a boundary validation failure carries outward.
    ///
    /// `field` is always a static label chosen here (never derived from input),
    /// so it is safe to log and to echo as a fixed client-facing message.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct InvalidInput {
        pub(crate) field: &'static str,
    }

    // Fixed message only: an `InvalidInput` must never render caller data even
    // if a `Builder`-layer caller wraps it in an `anyhow::Error` chain.
    impl std::fmt::Display for InvalidInput {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("invalid client input")
        }
    }
    impl std::error::Error for InvalidInput {}

    /// Emit the single fixed-vocabulary log record for a rejected input.
    ///
    /// Both transports call this before mapping to their own status code, so
    /// journald/OTLP output is uniform and the offending value is never read.
    pub(crate) fn log_invalid_input(field: &'static str) {
        tracing::warn!(
            error_kind = "invalid_input",
            field = field,
            "invalid client input"
        );
    }

    /// Validate a caller-supplied search limit, returning the safe `usize`.
    ///
    /// `i64` is the transport-neutral input: gRPC widens its `int32` (a
    /// negative value fails `try_from`) and HTTP now deserializes its limit as
    /// `i64` too, so both ingresses take the identical path. Zero is kept as a
    /// valid "return nothing" request for backwards compatibility.
    pub(crate) fn validate_search_limit(limit: i64) -> Result<usize, InvalidInput> {
        let limit = usize::try_from(limit).map_err(|_| InvalidInput { field: "limit" })?;
        if limit > MAX_SEARCH_LIMIT {
            return Err(InvalidInput { field: "limit" });
        }
        Ok(limit)
    }

    /// `true` only for a JSON object. Qdrant's payload map is built from object
    /// entries; a scalar/array value would silently produce an empty payload.
    fn is_object(value: &serde_json::Value) -> bool {
        matches!(value, serde_json::Value::Object(_))
    }

    /// Reject a text field larger than its transport-neutral byte budget (I57-C).
    ///
    /// The `field` label is always static, so the fixed-vocabulary rejection
    /// path cannot carry any of the oversized caller data into logs.
    fn validate_text_bytes(
        value: &str,
        max: usize,
        field: &'static str,
    ) -> Result<(), InvalidInput> {
        if value.len() > max {
            return Err(InvalidInput { field });
        }
        Ok(())
    }

    /// Depth check that stops descending as soon as the limit is exceeded, so a
    /// hostile nested object cannot drive unbounded recursion (I57-C).
    fn metadata_depth_within(value: &serde_json::Value, depth: usize) -> bool {
        if depth > MAX_METADATA_DEPTH {
            return false;
        }
        match value {
            serde_json::Value::Object(map) => map
                .values()
                .all(|entry| metadata_depth_within(entry, depth + 1)),
            serde_json::Value::Array(items) => items
                .iter()
                .all(|entry| metadata_depth_within(entry, depth + 1)),
            _ => true,
        }
    }

    /// Shared metadata checks used by both the ingress parser and the core-store
    /// defense-in-depth recheck (I57-C).
    fn validate_metadata_value(value: &serde_json::Value) -> Result<(), InvalidInput> {
        if !is_object(value) {
            return Err(InvalidInput { field: "metadata" });
        }
        // `Value`'s `Display` is infallible. Checking the canonical serialization
        // prevents a bypass via escaped/reshaped input whose raw form is short.
        if value.to_string().len() > MAX_METADATA_BYTES {
            return Err(InvalidInput { field: "metadata" });
        }
        if !metadata_depth_within(value, 0) {
            return Err(InvalidInput { field: "metadata" });
        }
        Ok(())
    }

    /// Parse the `metadata` string, require a JSON object and enforce the shared
    /// byte/depth bounds (I55-A + I57-C).
    ///
    /// A scalar such as `"text"` or `123` used to satisfy `from_str` and pass
    /// the boundary, but `memory::upsert_vector` only copies `Value::Object`
    /// entries into the Qdrant payload — the caller's data was silently lost.
    pub(crate) fn parse_metadata(raw: &str) -> Result<serde_json::Value, InvalidInput> {
        // Reject the oversized payload before allocating a `Value` for it.
        validate_text_bytes(raw, MAX_METADATA_BYTES, "metadata")?;
        let value: serde_json::Value =
            serde_json::from_str(raw).map_err(|_| InvalidInput { field: "metadata" })?;
        validate_metadata_value(&value)?;
        Ok(value)
    }

    /// Validate an embedding vector against the configured dimension.
    ///
    /// `embedding_dim` is fail-closed at startup; without this check the write
    /// path stayed fail-open and forwarded a wrong-sized (or empty) vector to
    /// Qdrant, contradicting that startup guarantee. An empty vector is a
    /// special case of the same mismatch and is rejected explicitly.
    ///
    /// Every component must also be finite. proto3 `repeated float` carries
    /// NaN and +/-Inf verbatim (JSON cannot represent them), so a correctly
    /// sized vector of non-finite values previously passed this shared check
    /// and reached Qdrant's upsert and distance computation. Rejecting here
    /// covers both ingress paths and the core store recheck, which all call
    /// this same helper.
    ///
    /// The builder also calls this at every Qdrant boundary (query embedding,
    /// pending-vector replay, reconcile upsert and pre-search) so an embedding
    /// produced upstream — including a finite f64 that saturated to infinity
    /// on the f32 cast — cannot reach Qdrant through a non-ingress route.
    pub(crate) fn validate_embedding(
        embedding: &[f32],
        embedding_dim: usize,
    ) -> Result<(), InvalidInput> {
        if embedding.is_empty()
            || embedding.len() != embedding_dim
            || !embedding.iter().all(|v| v.is_finite())
        {
            return Err(InvalidInput { field: "embedding" });
        }
        Ok(())
    }

    /// Resolve a caller-supplied expiry timestamp.
    ///
    /// Non-positive values are the documented "no expiry" sentinel (HTTP
    /// absent/`0`, gRPC proto3 `<= 0`). A *positive* value outside chrono's
    /// range used to degrade silently to `None` and store the memory with no
    /// expiry; it is now rejected at the boundary.
    ///
    /// Boundary note: this is a **required** memory field, unlike the additive
    /// event `source`/`timestamp` and the ranked `score`, which keep their
    /// degrade-on-invalid contract elsewhere.
    pub(crate) fn parse_expires_at(raw: i64) -> Result<Option<DateTime<Utc>>, InvalidInput> {
        if raw <= 0 {
            return Ok(None);
        }
        DateTime::from_timestamp(raw, 0)
            .map(Some)
            .ok_or(InvalidInput {
                field: "expires_at",
            })
    }

    /// Build a complete `MemoryEntry` from raw ingress fields, enforcing every
    /// I55-A rule in one place.
    ///
    /// The caller-supplied id is deliberately *not* accepted: both transports
    /// mint a fresh UUID so a client cannot overwrite an existing record
    /// (I7-E / I8-A). Only required memory fields are validated here; the
    /// additive fields (`event` `source`/`timestamp` on the build path and the
    /// ranked `score` on responses) intentionally keep their degrade contract
    /// outside this boundary. `score` starts as `None` for a stored entry.
    pub(crate) fn prepare_memory(
        content: String,
        embedding: Vec<f32>,
        metadata_raw: &str,
        source: String,
        expires_at_raw: i64,
        embedding_dim: usize,
    ) -> Result<MemoryEntry, InvalidInput> {
        validate_embedding(&embedding, embedding_dim)?;
        // I57-C: bound the stored text fields before building the entry so an
        // oversized payload never reaches PostgreSQL or the JSON log pipeline.
        validate_text_bytes(&content, MAX_CONTENT_BYTES, "content")?;
        validate_text_bytes(&source, MAX_SOURCE_BYTES, "source")?;
        let metadata = parse_metadata(metadata_raw)?;
        let expires_at = parse_expires_at(expires_at_raw)?;
        Ok(MemoryEntry {
            id: uuid::Uuid::new_v4(),
            content,
            embedding,
            metadata,
            source,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            expires_at,
            score: None,
        })
    }

    /// Defense-in-depth recheck at the core store boundary.
    ///
    /// Normal ingress already passed [`prepare_memory`]; this guards direct
    /// callers of `ContextBuilder::store_memory` too. A failure is returned as
    /// an `InvalidInput` error that the transports map back to a bare 400 /
    /// `InvalidArgument` rather than an internal error.
    pub(crate) fn validate_memory_for_store(
        memory: &MemoryEntry,
        embedding_dim: usize,
    ) -> Result<(), InvalidInput> {
        validate_embedding(&memory.embedding, embedding_dim)?;
        validate_text_bytes(&memory.content, MAX_CONTENT_BYTES, "content")?;
        validate_text_bytes(&memory.source, MAX_SOURCE_BYTES, "source")?;
        validate_metadata_value(&memory.metadata)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;

        #[test]
        fn limit_rejects_negative_and_oversized_values() {
            assert_eq!(
                validate_search_limit(-1),
                Err(InvalidInput { field: "limit" })
            );
            assert_eq!(
                validate_search_limit(MAX_SEARCH_LIMIT as i64 + 1),
                Err(InvalidInput { field: "limit" })
            );
            assert_eq!(validate_search_limit(0).expect("zero is allowed"), 0);
            assert_eq!(validate_search_limit(5).expect("valid limit"), 5);
            assert_eq!(
                validate_search_limit(MAX_SEARCH_LIMIT as i64).expect("boundary value"),
                MAX_SEARCH_LIMIT
            );
        }

        #[test]
        fn metadata_requires_a_json_object() {
            assert_eq!(
                parse_metadata(r#"{"kind":"test"}"#).expect("object is accepted"),
                json!({"kind": "test"})
            );
            for scalar in [r#""text""#, "123", "[1, 2]", "null"] {
                assert_eq!(
                    parse_metadata(scalar),
                    Err(InvalidInput { field: "metadata" }),
                    "non-object metadata must be rejected: {scalar}"
                );
            }
            assert_eq!(
                parse_metadata("{not-json").err().map(|e| e.field),
                Some("metadata")
            );
        }

        #[test]
        fn embedding_must_match_the_configured_dimension_and_be_non_empty() {
            assert!(validate_embedding(&[0.1, 0.2, 0.3], 3).is_ok());
            assert_eq!(
                validate_embedding(&[], 3),
                Err(InvalidInput { field: "embedding" })
            );
            assert_eq!(
                validate_embedding(&[0.1, 0.2], 3),
                Err(InvalidInput { field: "embedding" })
            );
            assert_eq!(
                validate_embedding(&[0.1; 4], 3),
                Err(InvalidInput { field: "embedding" })
            );
        }

        /// proto3 `repeated float` can carry NaN and both infinities even when
        /// the vector length matches the configured dimension. The shared
        /// validator must reject them so no non-finite vector reaches Qdrant's
        /// upsert / distance computation. gRPC is the ingress that can actually
        /// transport these values; HTTP uses the very same function.
        #[test]
        fn embedding_rejects_nan_and_infinities() {
            for non_finite in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                assert_eq!(
                    validate_embedding(&[non_finite, 0.2, 0.3], 3),
                    Err(InvalidInput { field: "embedding" }),
                    "non-finite component at index 0 must be rejected: {non_finite}"
                );
                assert_eq!(
                    validate_embedding(&[0.1, non_finite, 0.3], 3),
                    Err(InvalidInput { field: "embedding" }),
                    "non-finite component at index 1 must be rejected: {non_finite}"
                );
            }
            // A fully finite vector of the right size remains valid.
            assert!(validate_embedding(&[0.1, 0.2, 0.3], 3).is_ok());
        }

        /// `embedding.rs` rejects the f64 -> f32 saturation at parse time, but
        /// every Qdrant boundary also gates the resulting vector through this
        /// shared validator. A saturated value must therefore be rejected here
        /// too, with the same fixed `InvalidInput { field: "embedding" }`.
        #[test]
        fn embedding_rejects_f64_saturated_f32_value() {
            let saturated = 1e100_f64 as f32;
            assert_eq!(saturated, f32::INFINITY, "test premise: saturation");
            assert_eq!(
                validate_embedding(&[saturated, 0.2, 0.3], 3),
                Err(InvalidInput { field: "embedding" })
            );
        }

        /// The defense-in-depth core recheck goes through `validate_embedding`,
        /// so a direct `store_memory` caller cannot inject NaN past ingress.
        #[test]
        fn core_store_recheck_rejects_non_finite_embedding() {
            let mut memory = prepare_memory(
                "content".into(),
                vec![0.1, 0.2, 0.3],
                r#"{"kind":"test"}"#,
                "source".into(),
                0,
                3,
            )
            .expect("valid memory entry");
            memory.embedding[1] = f32::NAN;
            assert_eq!(
                validate_memory_for_store(&memory, 3),
                Err(InvalidInput { field: "embedding" })
            );
        }

        #[test]
        fn positive_out_of_range_expiry_is_rejected() {
            assert_eq!(parse_expires_at(0).expect("unset"), None);
            assert_eq!(parse_expires_at(-5).expect("sentinel"), None);
            assert_eq!(
                parse_expires_at(1_800_000_000)
                    .expect("valid")
                    .map(|t| t.timestamp()),
                Some(1_800_000_000)
            );
            assert_eq!(
                parse_expires_at(i64::MAX),
                Err(InvalidInput {
                    field: "expires_at"
                })
            );
        }

        #[test]
        fn invalid_input_error_never_renders_caller_data() {
            let err = InvalidInput { field: "limit" };
            assert_eq!(err.to_string(), "invalid client input");
            assert!(!err.to_string().contains("limit"));
        }

        /// Build a valid three-dimensional entry with caller-controlled text.
        fn entry(content: &str, source: &str, metadata: &str) -> Result<MemoryEntry, InvalidInput> {
            prepare_memory(
                content.into(),
                vec![0.1_f32, 0.2, 0.3],
                metadata,
                source.into(),
                0,
                3,
            )
        }

        /// I57-C: stored text fields and raw metadata are bounded at ingress.
        #[test]
        fn prepare_memory_rejects_oversized_text_and_metadata() {
            let valid_metadata = r#"{"kind":"test"}"#;
            assert!(entry("content", "source", valid_metadata).is_ok());

            let long_content = "x".repeat(MAX_CONTENT_BYTES + 1);
            let err = entry(&long_content, "source", valid_metadata).unwrap_err();
            assert_eq!(err.field, "content");

            let long_source = "s".repeat(MAX_SOURCE_BYTES + 1);
            let err = entry("content", &long_source, valid_metadata).unwrap_err();
            assert_eq!(err.field, "source");

            let long_metadata = format!(r#"{{"k":"{}"}}"#, "m".repeat(MAX_METADATA_BYTES));
            let err = entry("content", "source", &long_metadata).unwrap_err();
            assert_eq!(err.field, "metadata");

            // Exact boundary values stay accepted.
            let content_at_limit = "x".repeat(MAX_CONTENT_BYTES);
            let source_at_limit = "s".repeat(MAX_SOURCE_BYTES);
            let metadata_at_limit = format!(r#"{{"k":"{}"}}"#, "m".repeat(MAX_METADATA_BYTES - 8));
            assert!(entry(&content_at_limit, "source", valid_metadata).is_ok());
            assert!(entry("content", &source_at_limit, valid_metadata).is_ok());
            assert!(entry("content", "source", &metadata_at_limit).is_ok());
        }

        /// I57-C: `validate_text_bytes` is defined in UTF-8 bytes. A pure
        /// multi-byte value at exactly `max` bytes is accepted, while one more
        /// multi-byte character (over the byte budget but still far under it
        /// in characters) is rejected. A switch to `chars().count()` would fail
        /// the second assertion.
        #[test]
        fn validate_text_bytes_uses_utf8_byte_length_not_char_count() {
            let at_limit = "あ".repeat(10); // 10 chars, 30 UTF-8 bytes
            assert_eq!(at_limit.len(), 30, "test premise");
            assert_eq!(at_limit.chars().count(), 10, "test premise");
            assert!(validate_text_bytes(&at_limit, 30, "source").is_ok());

            let over_limit = "あ".repeat(11); // 11 chars, 33 UTF-8 bytes
            assert_eq!(over_limit.len(), 33, "test premise");
            assert_eq!(
                validate_text_bytes(&over_limit, 30, "source"),
                Err(InvalidInput { field: "source" })
            );
        }

        /// I57-C: `validate_text_bytes` counts UTF-8 bytes, not characters.
        /// A multi-byte value whose byte length sits exactly on the limit is
        /// accepted even though its `chars().count()` is far smaller; one more
        /// multi-byte character crosses the byte budget and must be rejected.
        /// Guards against regressing to a character-count check.
        #[test]
        fn prepare_memory_counts_multibyte_text_bytes_not_chars() {
            let valid_metadata = r#"{"kind":"test"}"#;
            // "あ" is 3 bytes in UTF-8. Fill to exactly MAX_SOURCE_BYTES with
            // whole multibyte characters plus a single ASCII byte of padding.
            let filler = "あ".repeat(MAX_SOURCE_BYTES / 3);
            let source_at_limit = format!("{filler}a");
            assert_eq!(
                source_at_limit.len(),
                MAX_SOURCE_BYTES,
                "test premise: exact UTF-8 byte boundary"
            );
            assert!(
                source_at_limit.chars().count() < MAX_SOURCE_BYTES,
                "test premise: character count differs from byte count"
            );
            assert!(
                entry("content", &source_at_limit, valid_metadata).is_ok(),
                "a value exactly at the byte limit stays accepted"
            );

            let over_limit = format!("{source_at_limit}あ");
            assert_eq!(
                over_limit.len(),
                MAX_SOURCE_BYTES + "あ".len(),
                "test premise: one more 3-byte character exceeds the limit"
            );
            let err = entry("content", &over_limit, valid_metadata).unwrap_err();
            assert_eq!(err.field, "source");
        }

        /// I57-C: metadata nesting is bounded both at ingress and at the
        /// core-store recheck.
        #[test]
        fn metadata_depth_limit_is_enforced() {
            let shallow: serde_json::Value =
                serde_json::from_str(r#"{"a":{"b":{"c":1}}}"#).expect("shallow object parses");
            assert!(validate_metadata_value(&shallow).is_ok());

            let raw = format!(
                "{}0{}",
                r#"{"a":"#.repeat(MAX_METADATA_DEPTH + 1),
                "}".repeat(MAX_METADATA_DEPTH + 1)
            );
            let deep: serde_json::Value = serde_json::from_str(&raw).expect("nested object parses");
            assert_eq!(
                validate_metadata_value(&deep),
                Err(InvalidInput { field: "metadata" })
            );
            let err = entry("content", "source", &raw).unwrap_err();
            assert_eq!(err.field, "metadata");
        }

        /// I57-C: the core-store recheck applies the same byte bounds so a
        /// direct `store_memory` caller cannot bypass ingress validation.
        #[test]
        fn core_store_recheck_enforces_size_bounds() {
            let mut memory = entry("content", "source", r#"{"kind":"test"}"#).unwrap();
            assert!(validate_memory_for_store(&memory, 3).is_ok());

            memory.content = "x".repeat(MAX_CONTENT_BYTES + 1);
            let err = validate_memory_for_store(&memory, 3).unwrap_err();
            assert_eq!(err.field, "content");
            memory.content.clear();

            memory.source = "s".repeat(MAX_SOURCE_BYTES + 1);
            let err = validate_memory_for_store(&memory, 3).unwrap_err();
            assert_eq!(err.field, "source");
        }

        /// I57-C: the defense-in-depth core-store recheck counts UTF-8 bytes
        /// too, so a direct `store_memory` caller cannot slip an oversized
        /// multibyte value past the shared bound by relying on char count.
        #[test]
        fn core_store_recheck_counts_multibyte_text_bytes_not_chars() {
            let mut memory = entry("content", "source", r#"{"kind":"test"}"#).unwrap();
            let filler = "あ".repeat(MAX_SOURCE_BYTES / 3);
            let source_at_limit = format!("{filler}a");
            assert_eq!(source_at_limit.len(), MAX_SOURCE_BYTES, "test premise");
            memory.source = source_at_limit;
            assert!(validate_memory_for_store(&memory, 3).is_ok());

            memory.source.push('あ');
            assert_eq!(
                memory.source.len(),
                MAX_SOURCE_BYTES + "あ".len(),
                "test premise: one more multibyte character exceeds the limit"
            );
            let err = validate_memory_for_store(&memory, 3).unwrap_err();
            assert_eq!(err.field, "source");
        }
    }
}

/// Unknown keys are rejected at load time: a one-character typo of an
/// opt-in security key such as `admin_token` must not be silently ignored.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextConfig {
    pub grpc_addr: String,
    pub http_addr: String,
    pub pg_url: String,
    pub qdrant_url: String,
    pub qdrant_api_key: Option<String>,
    pub collection_name: String,
    pub embedding_model: String,
    /// Vector size of the Qdrant collection. Must match the output dimension
    /// of `embedding_model`; changing only the model leaves a mismatched
    /// collection that fails on upsert/search.
    #[serde(default = "default_embedding_dim")]
    pub embedding_dim: usize,
    pub embedding_url: Option<String>,
    pub pii_patterns_path: String,
    pub max_context_tokens: usize,
    /// Optional shared secret for the HTTP API. When set via
    /// `JARVIS_CONTEXT_ADMIN_TOKEN`, every API route except `/health` requires
    /// it in the `x-jarvis-context-token` header. Unset keeps loopback clients
    /// working unchanged; a configured but blank value aborts startup
    /// (fail-closed).
    #[serde(default)]
    pub admin_token: Option<String>,
}

impl ContextConfig {
    /// Load from the absolute [`DEFAULT_CONFIG_PATH`], or from the path in
    /// [`CONFIG_PATH_ENV`] when it is set and non-blank, with `JARVIS_CONTEXT_*`
    /// environment overrides applied on top.
    ///
    /// The default file may be absent on real hosts: configuration is supplied
    /// by the `JARVIS_CONTEXT_*` Env provider (`bws run` / systemd), so the
    /// service starts with no TOML on disk. `Toml::file` simply contributes no
    /// data for a missing file; every required field then comes from the
    /// environment.
    pub fn load() -> Result<Self> {
        let path = resolve_config_path(std::env::var(CONFIG_PATH_ENV).ok());
        Self::load_from_path(&path)
    }

    /// Parse a config from an explicit TOML path. Split out from [`Self::load`]
    /// so path selection is testable without touching the process environment.
    fn load_from_path(path: &str) -> Result<Self> {
        let config: ContextConfig = Figment::new()
            .merge(Toml::file(path))
            // Flat snake_case fields: do NOT use figment's Env split, which
            // would turn JARVIS_CONTEXT_GRPC_ADDR into the nested key `grpc.addr`.
            // The config-path variable only selects the file, so keep it out
            // of the field provider as well.
            .merge(Env::prefixed(ENV_PREFIX).ignore(&["config_path"]))
            .extract()?;
        // I99-A: a configured-but-blank token would otherwise collapse to
        // `None` in `admin_token()` and leave the `/v1/*` memory API and gRPC
        // memory gates unauthenticated. Fail closed at startup.
        config.validate_admin_token()?;
        Ok(config)
    }

    /// Fail-closed startup validation of the optional admin token (I99-A).
    ///
    /// A token that is present but blank (empty or whitespace-only) is
    /// rejected: `admin_token()` would otherwise treat it as unset and both
    /// the HTTP `/v1/*` memory API and the gRPC metadata gate would fall back
    /// to the legacy unauthenticated loopback path. `None` (unset) stays valid
    /// and keeps that legacy behavior for compatibility.
    ///
    /// The error is a fixed literal: no configured token value is ever
    /// interpolated, so secrets cannot reach startup logs.
    pub fn validate_admin_token(&self) -> Result<()> {
        if self
            .admin_token
            .as_deref()
            .is_some_and(|token| token.trim().is_empty())
        {
            anyhow::bail!(
                "admin_token is set but blank; set JARVIS_CONTEXT_ADMIN_TOKEN \
                 to a non-empty secret or unset it before starting the service"
            );
        }
        Ok(())
    }

    pub fn pg_url(&self) -> &str {
        &self.pg_url
    }

    pub fn qdrant_api_key(&self) -> Option<&str> {
        // An empty JARVIS_CONTEXT_QDRANT_API_KEY means "no key" (the shipped
        // .env template sets the variable to an empty value).
        self.qdrant_api_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
    }

    /// Returns the trimmed token, or `None` when unset/blank. A configured but
    /// blank `JARVIS_CONTEXT_ADMIN_TOKEN` is rejected at startup by
    /// [`Self::validate_admin_token`]; this filter only guards direct callers
    /// (defense in depth).
    pub fn admin_token(&self) -> Option<&str> {
        self.admin_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
    }
}

/// Redacted `Debug`: a derived implementation would print `admin_token` and a
/// userinfo-bearing `pg_url` into any config dump. Neither secret may reach
/// logs; `pg_url()` still returns the unredacted URL for real connections.
impl std::fmt::Debug for ContextConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextConfig")
            .field("grpc_addr", &self.grpc_addr)
            .field("http_addr", &self.http_addr)
            // I86-B (b): pg_url may carry userinfo (user:credential@host) and
            // the loader deliberately accepts such URLs for real connections.
            // Redact the whole field here so `format!("{config:?}")` cannot
            // leak an embedded password into logs; `pg_url()` is unchanged.
            .field("pg_url", &"<redacted>")
            .field("qdrant_url", &self.qdrant_url)
            .field("qdrant_api_key", &"<redacted>")
            .field("collection_name", &self.collection_name)
            .field("embedding_model", &self.embedding_model)
            .field("embedding_dim", &self.embedding_dim)
            .field("embedding_url", &self.embedding_url)
            .field("pii_patterns_path", &self.pii_patterns_path)
            .field("max_context_tokens", &self.max_context_tokens)
            .field("admin_token", &"<redacted>")
            .finish()
    }
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            grpc_addr: "127.0.0.1:50054".into(),
            http_addr: "127.0.0.1:8084".into(),
            // No compiled-in credentials: authentication is delegated to libpq
            // (PGUSER / PGPASSWORD / .pgpass / IAM ...). Deployments supply the
            // full URL via JARVIS_CONTEXT_PG_URL or /etc/jarvis/context.toml.
            pg_url: "postgres://localhost:5432/jarvis".into(),
            qdrant_url: "http://127.0.0.1:6334".into(),
            qdrant_api_key: None,
            collection_name: "jarvis_memories".into(),
            embedding_model: "sentence-transformers/all-MiniLM-L6-v2".into(),
            embedding_dim: default_embedding_dim(),
            embedding_url: None,
            pii_patterns_path: "/etc/jarvis/pii_patterns.toml".into(),
            max_context_tokens: 8000,
            admin_token: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::providers::Serialized;
    use std::path::Path;

    /// A config identical to a legacy deployment: no `admin_token` key at all.
    const LEGACY_TOML: &str = r#"
grpc_addr = "127.0.0.1:50054"
http_addr = "127.0.0.1:8084"
pg_url = "postgres://localhost:5432/jarvis"
qdrant_url = "http://127.0.0.1:6334"
collection_name = "jarvis_memories"
embedding_model = "sentence-transformers/all-MiniLM-L6-v2"
pii_patterns_path = "/etc/jarvis/pii_patterns.toml"
max_context_tokens = 8000
"#;

    #[test]
    fn legacy_config_without_admin_token_still_loads() {
        let config: ContextConfig = Figment::from(Toml::string(LEGACY_TOML))
            .extract()
            .expect("legacy config parses");
        assert!(config.admin_token.is_none());
        assert!(config.admin_token().is_none());
        assert_eq!(ContextConfig::default().admin_token, None);
    }

    /// Fail-closed regression: a set-but-blank token must never silently
    /// collapse to the unauthenticated path. Both the legacy unset case and a
    /// real secret must still validate.
    #[test]
    fn admin_token_from_toml_and_blank_value_is_rejected() {
        let with_token = format!("{LEGACY_TOML}\nadmin_token = \"admin-secret\"\n");
        let config: ContextConfig = Figment::from(Toml::string(&with_token))
            .extract()
            .expect("config with token parses");
        assert_eq!(config.admin_token(), Some("admin-secret"));
        assert!(config.validate_admin_token().is_ok());

        let blank = format!("{LEGACY_TOML}\nadmin_token = \"  \"\n");
        let config: ContextConfig = Figment::from(Toml::string(&blank))
            .extract()
            .expect("blank token parses");
        let message = config
            .validate_admin_token()
            .expect_err("blank configured token must be rejected")
            .to_string();
        assert!(
            message.contains("admin_token is set but blank"),
            "{message}"
        );
    }

    /// `None` (unset) remains the legacy unauthenticated compatibility case;
    /// a real secret is accepted. Only a present-but-blank value is an error.
    #[test]
    fn validate_admin_token_accepts_unset_and_non_blank() {
        assert!(ContextConfig::default().validate_admin_token().is_ok());
        let config = ContextConfig {
            admin_token: Some("admin-secret".to_owned()),
            ..Default::default()
        };
        assert!(config.validate_admin_token().is_ok());
    }

    /// Empty, ASCII whitespace and visually distinct Unicode whitespace are
    /// all blank after `trim()` and must abort startup. The fixed error text
    /// interpolates no configuration value, so a non-breaking-space token
    /// cannot be echoed back into startup logs.
    #[test]
    fn validate_admin_token_rejects_blank_and_never_echoes_value() {
        for token in ["", "   ", "\t\n", "\u{00a0}\u{00a0}"] {
            let config = ContextConfig {
                admin_token: Some(token.to_owned()),
                ..Default::default()
            };
            let message = config
                .validate_admin_token()
                .expect_err("blank configured token must be rejected")
                .to_string();
            assert!(
                message.contains("admin_token is set but blank"),
                "{message}"
            );
        }
        let config = ContextConfig {
            admin_token: Some("\u{00a0}\u{00a0}".to_owned()),
            ..Default::default()
        };
        let message = config
            .validate_admin_token()
            .expect_err("blank configured token must be rejected")
            .to_string();
        assert!(!message.contains('\u{00a0}'), "{message}");
    }

    /// The real startup path (`load` -> `load_from_path`) must enforce the
    /// guard, not just a directly called helper.
    #[test]
    fn load_from_path_rejects_blank_admin_token() {
        let dir = std::env::temp_dir().join("jarvis-context-config-blank-token");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("context.toml");
        let toml = format!("{LEGACY_TOML}\nadmin_token = \"   \"\n");
        std::fs::write(&path, &toml).expect("write config");
        let error = ContextConfig::load_from_path(path.to_str().expect("utf8 path"))
            .expect_err("blank configured token must abort startup");
        assert!(
            error.to_string().contains("admin_token is set but blank"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `JARVIS_CONTEXT_ADMIN_TOKEN` becomes `admin_token` through the same
    /// `Env::prefixed("JARVIS_CONTEXT_")` chain used by `load()`. No OS env var
    /// is mutated; the env-style key is merged as a figment value.
    #[test]
    fn admin_token_env_key_maps_to_field() {
        const ENV_PREFIX: &str = "JARVIS_CONTEXT_";
        const ENV_KEY: &str = "JARVIS_CONTEXT_ADMIN_TOKEN";

        let field = ENV_KEY
            .strip_prefix(ENV_PREFIX)
            .expect("key uses the context env prefix")
            .to_ascii_lowercase();
        assert_eq!(field, "admin_token");

        let config: ContextConfig = Figment::from(Toml::string(LEGACY_TOML))
            .merge(Serialized::default(field.as_str(), "env-secret"))
            .extract()
            .expect("env-style key merges into the admin token field");
        assert_eq!(config.admin_token(), Some("env-secret"));
    }

    /// The gRPC metadata key must match the HTTP header literal exactly;
    /// otherwise the two transports would enforce different secrets.
    #[test]
    fn context_token_header_is_the_shared_header_name() {
        assert_eq!(CONTEXT_TOKEN_HEADER, "x-jarvis-context-token");
    }

    #[test]
    fn token_matches_enforces_exact_value_and_keeps_legacy_open_path() {
        assert!(token_matches(None, None), "no configured token stays open");
        assert!(token_matches(None, Some("anything")));
        assert!(token_matches(Some("  "), None), "blank config stays open");
        assert!(!token_matches(Some("context-secret"), None));
        assert!(token_matches(
            Some("context-secret"),
            Some("context-secret")
        ));
        assert!(!token_matches(
            Some("context-secret"),
            Some("Context-secret")
        ));
        assert!(!token_matches(
            Some("context-secret"),
            Some("context-secret-extra")
        ));
        assert!(!token_matches(Some("context-secret"), Some("")));
    }

    #[test]
    fn debug_output_never_contains_the_admin_token() {
        let config = ContextConfig {
            admin_token: Some("super-secret".into()),
            ..Default::default()
        };
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("super-secret"));
        assert!(rendered.contains("admin_token: \"<redacted>\""));
    }

    /// I86-B: a configured `pg_url` may embed userinfo, and the loader accepts
    /// it verbatim for real connections. The manual `Debug` must redact the
    /// whole field so a config dump cannot leak the password, while `pg_url()`
    /// stays unchanged on the connection path.
    #[test]
    fn debug_output_never_contains_the_pg_url_password() {
        let embedded = "dummy-password-42";
        let url = format!("postgres://jarvis:{embedded}@db.internal:5432/jarvis");
        let config = ContextConfig {
            pg_url: url.clone(),
            ..Default::default()
        };
        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains(embedded),
            "pg_url userinfo/password must not appear in Debug output"
        );
        assert!(rendered.contains("pg_url: \"<redacted>\""));
        assert_eq!(config.pg_url(), url, "real connection URL stays intact");
    }

    /// The compiled-in default must never embed a password. Auth is delegated
    /// to libpq; a hardcoded `user:password@` here would leak into every config
    /// dump and test snapshot. Regression guard for I12-D.
    #[test]
    fn default_pg_url_has_no_embedded_credentials() {
        let config = ContextConfig::default();
        let url = config.pg_url();
        assert_eq!(url, "postgres://localhost:5432/jarvis");
        assert!(
            !url.contains('@'),
            "default pg_url must not carry userinfo/password; delegate to libpq",
        );
    }

    /// `JARVIS_CONTEXT_PG_URL` must keep overriding the configured URL through
    /// the same `Env::prefixed("JARVIS_CONTEXT_")` chain used by `load()`. No
    /// OS env var is mutated; the env-style key is merged as a figment value.
    #[test]
    fn pg_url_env_key_overrides_configured_url() {
        const ENV_PREFIX: &str = "JARVIS_CONTEXT_";
        const ENV_KEY: &str = "JARVIS_CONTEXT_PG_URL";

        let field = ENV_KEY
            .strip_prefix(ENV_PREFIX)
            .expect("key uses the context env prefix")
            .to_ascii_lowercase();
        assert_eq!(field, "pg_url");

        let override_url = "postgres://override-host:5432/override_db";
        let config: ContextConfig = Figment::from(Toml::string(LEGACY_TOML))
            .merge(Serialized::default(field.as_str(), override_url))
            .extract()
            .expect("env-style key merges into the pg_url field");
        assert_eq!(config.pg_url(), override_url);
    }

    /// An explicitly configured URL is preserved verbatim; only the compiled
    /// default is passwordless. This guards against an over-broad change that
    /// would reject legitimate operator-supplied libpq connection strings.
    #[test]
    fn explicit_pg_url_from_toml_is_preserved() {
        let toml = LEGACY_TOML.replace(
            "pg_url = \"postgres://localhost:5432/jarvis\"",
            "pg_url = \"postgres://db.internal:5432/custom_db\"",
        );
        assert_ne!(toml.as_str(), LEGACY_TOML, "fixture replacement must apply");
        let config: ContextConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("explicit pg_url parses");
        assert_eq!(config.pg_url(), "postgres://db.internal:5432/custom_db");
    }

    /// A legacy config (no `embedding_dim` key) must still land on 384 rather
    /// than `usize::default() == 0`, otherwise the collection would be created
    /// with a zero-sized vector.
    #[test]
    fn legacy_config_without_embedding_dim_defaults_to_384() {
        let config: ContextConfig = Figment::from(Toml::string(LEGACY_TOML))
            .extract()
            .expect("legacy config parses");
        assert_eq!(config.embedding_dim, 384);
        assert_eq!(ContextConfig::default().embedding_dim, 384);
    }

    #[test]
    fn embedding_dim_from_toml_is_preserved() {
        let toml = format!("{LEGACY_TOML}\nembedding_dim = 768\n");
        let config: ContextConfig = Figment::from(Toml::string(&toml))
            .extract()
            .expect("explicit embedding_dim parses");
        assert_eq!(config.embedding_dim, 768);
    }

    /// `JARVIS_CONTEXT_EMBEDDING_DIM` reaches the field through the same
    /// `Env::prefixed("JARVIS_CONTEXT_")` chain as `load()`. Figment's Env
    /// parses an unsigned-integer string into a numeric value, so a numeric
    /// figment value models the env override without mutating the OS env.
    #[test]
    fn embedding_dim_env_key_maps_to_field() {
        const ENV_PREFIX: &str = "JARVIS_CONTEXT_";
        const ENV_KEY: &str = "JARVIS_CONTEXT_EMBEDDING_DIM";

        let field = ENV_KEY
            .strip_prefix(ENV_PREFIX)
            .expect("key uses the context env prefix")
            .to_ascii_lowercase();
        assert_eq!(field, "embedding_dim");

        let config: ContextConfig = Figment::from(Toml::string(LEGACY_TOML))
            .merge(Serialized::default(field.as_str(), 768u64))
            .extract()
            .expect("env-style key merges into embedding_dim");
        assert_eq!(config.embedding_dim, 768);
    }

    /// The service must find its config regardless of the launch directory;
    /// the former `config/context.toml` was CWD-relative.
    #[test]
    fn default_config_path_is_absolute() {
        assert!(Path::new(DEFAULT_CONFIG_PATH).is_absolute());
        assert_eq!(DEFAULT_CONFIG_PATH, "/etc/jarvis/context.toml");
    }

    #[test]
    fn config_path_override_is_honoured() {
        const OVERRIDE: &str = "/tmp/jarvis-context-custom.toml";
        assert_eq!(resolve_config_path(Some(OVERRIDE.into())), OVERRIDE);
        assert_eq!(resolve_config_path(None), DEFAULT_CONFIG_PATH);
        assert_eq!(resolve_config_path(Some("   ".into())), DEFAULT_CONFIG_PATH);
        assert_eq!(
            resolve_config_path(Some(String::new())),
            DEFAULT_CONFIG_PATH
        );
    }

    /// The override variable shares the config prefix, so verify it maps to
    /// the `config_path` key that `load_from_path` filters out of the provider.
    #[test]
    fn config_path_env_name_matches_prefix() {
        let field = CONFIG_PATH_ENV
            .strip_prefix(ENV_PREFIX)
            .expect("override variable uses the context env prefix")
            .to_ascii_lowercase();
        assert_eq!(field, "config_path");
    }

    /// I105-B: a one-character typo of the ingress-token key must abort
    /// startup instead of being silently ignored (which would leave the
    /// unauthenticated legacy path in place). This crate has no `require_*`
    /// boolean guard; `admin_token` is its opt-in security boundary.
    #[test]
    fn typo_guard_key_is_rejected_at_startup() {
        let dir = std::env::temp_dir().join("jarvis-context-config-typo-guard");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("context.toml");
        let mut toml = LEGACY_TOML.to_owned();
        // `admin_tokn` differs by one character from the real `admin_token`.
        toml.push_str("admin_tokn = \"guard-secret\"\n");
        std::fs::write(&path, &toml).expect("write config");
        let error = ContextConfig::load_from_path(path.to_str().expect("utf8 path"))
            .expect_err("unknown guard key must abort startup");
        assert!(
            error.to_string().contains("admin_tokn"),
            "error must name the mistyped key: {error}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
