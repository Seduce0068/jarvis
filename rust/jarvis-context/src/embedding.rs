//! Query embedding client.
//!
//! Sends the query text to an Ollama-compatible embedding endpoint
//! (`POST /api/embed`). The endpoint is optional: when
//! `JARVIS_CONTEXT_EMBEDDING_URL` is unset or empty, vector search is
//! skipped with a warning instead of silently searching with garbage
//! vectors.

use anyhow::{Context, Result};
use serde_json::Value;
use std::time::Duration;

#[derive(Clone)]
pub struct EmbeddingClient {
    client: reqwest::Client,
    base_url: Option<String>,
    model: String,
}

impl EmbeddingClient {
    pub fn from_config(config: &crate::config::ContextConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .context("building embedding HTTP client")?;
        let base_url = config
            .embedding_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(|url| url.trim_end_matches('/').to_string());
        Ok(Self {
            client,
            base_url,
            model: config.embedding_model.clone(),
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.base_url.is_some()
    }

    /// Embed `text` via the configured endpoint.
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let Some(base_url) = &self.base_url else {
            anyhow::bail!("embedding endpoint not configured (set JARVIS_CONTEXT_EMBEDDING_URL)");
        };
        let response = self
            .client
            .post(format!("{base_url}/api/embed"))
            .json(&serde_json::json!({ "model": self.model, "input": text }))
            .send()
            .await
            .context("calling embedding endpoint")?;
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("embedding endpoint returned HTTP {status}");
        }
        let body: Value = response
            .json()
            .await
            .context("decoding embedding response")?;
        parse_embedding_response(&body)
    }
}

/// Parse an Ollama-style embedding response.
///
/// Supports the current `/api/embed` shape (`{"embeddings": [[..]]}`) and the
/// legacy `/api/embeddings` shape (`{"embedding": [..]}`).
pub fn parse_embedding_response(body: &Value) -> Result<Vec<f32>> {
    let vector = body
        .get("embeddings")
        .and_then(|value| value.get(0))
        .and_then(Value::as_array)
        .or_else(|| body.get("embedding").and_then(Value::as_array));
    let Some(values) = vector else {
        anyhow::bail!("unexpected embedding response: missing `embeddings`/`embedding` array");
    };
    if values.is_empty() {
        anyhow::bail!("embedding vector is empty");
    }
    values
        .iter()
        .map(|value| {
            let float = value
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("embedding contains a non-numeric value"))?;
            // A perfectly finite f64 (e.g. 1e100) saturates to +/-Inf on the
            // narrowing cast. Validate the *f32* result so a saturated vector
            // can never reach Qdrant's upsert/distance computation. The message
            // is fixed vocabulary: no component value is ever rendered.
            let cast = float as f32;
            if !cast.is_finite() {
                anyhow::bail!("embedding contains a non-finite value after f32 conversion");
            }
            Ok(cast)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_ollama_embed_shape() -> Result<()> {
        let body = json!({ "embeddings": [[0.1, 0.2, 0.3]] });
        assert_eq!(parse_embedding_response(&body)?, vec![0.1, 0.2, 0.3]);
        Ok(())
    }

    #[test]
    fn parses_legacy_embeddings_shape() -> Result<()> {
        let body = json!({ "embedding": [1.0, -0.5] });
        assert_eq!(parse_embedding_response(&body)?, vec![1.0, -0.5]);
        Ok(())
    }

    #[test]
    fn rejects_unexpected_shapes() {
        assert!(parse_embedding_response(&json!({ "foo": 1 })).is_err());
        assert!(parse_embedding_response(&json!({ "embeddings": [[]] })).is_err());
        assert!(parse_embedding_response(&json!({ "embedding": ["x"] })).is_err());
    }

    /// A finite f64 far above `f32::MAX` saturates to `f32::INFINITY` on the
    /// narrowing cast. The post-cast `is_finite()` check must reject it, so no
    /// saturated vector can enter the query-search path. Pure: no HTTP client
    /// and no live embedding endpoint are involved.
    #[test]
    fn rejects_finite_f64_values_that_saturate_to_non_finite_f32() {
        assert_eq!(1e100_f64 as f32, f32::INFINITY, "test premise: saturation");

        for body in [
            json!({ "embeddings": [[1e100_f64, 0.2, 0.3]] }),
            json!({ "embedding": [-1e100_f64, 0.2, 0.3] }),
        ] {
            let err = parse_embedding_response(&body)
                .expect_err("f64 -> f32 saturation must be rejected at parse time");
            assert!(
                format!("{err:#}").contains("non-finite"),
                "fixed rejection reason must be reported: {err:#}"
            );
        }

        // Ordinary finite values keep working.
        assert!(parse_embedding_response(&json!({ "embedding": [0.1, 0.2, 0.3] })).is_ok());
    }

    #[test]
    fn empty_url_disables_client() -> Result<()> {
        let config = crate::config::ContextConfig {
            embedding_url: Some("  ".into()),
            ..Default::default()
        };
        let client = EmbeddingClient::from_config(&config)?;
        assert!(!client.is_enabled());
        Ok(())
    }
}
