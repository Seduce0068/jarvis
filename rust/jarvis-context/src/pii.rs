//! PII detection and redaction

use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use tracing::warn;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PiiConfig {
    pub patterns: HashMap<String, String>,
    pub redact_replacement: String,
    pub enabled: bool,
}

pub struct PiiFilter {
    patterns: Vec<(String, Regex)>,
    replacement: String,
    enabled: bool,
}

impl PiiFilter {
    pub async fn new(config: &crate::config::ContextConfig) -> Result<Self> {
        let content = tokio::fs::read_to_string(&config.pii_patterns_path)
            .await
            .unwrap_or_default();
        let pii_config: PiiConfig = toml::from_str(&content).unwrap_or_else(|_| PiiConfig {
            patterns: Self::default_patterns(),
            redact_replacement: "[REDACTED]".into(),
            enabled: true,
        });

        Ok(Self::from_config(pii_config))
    }

    /// Build a filter directly from a config (used by tests and embedders
    /// that keep patterns in memory instead of on disk).
    pub fn from_config(config: PiiConfig) -> Self {
        let mut patterns = Vec::new();
        for (name, pattern) in config.patterns {
            match Regex::new(&pattern) {
                Ok(re) => patterns.push((name, re)),
                Err(error) => warn!("Invalid PII pattern '{}': {}", name, error),
            }
        }
        Self {
            patterns,
            replacement: config.redact_replacement,
            enabled: config.enabled,
        }
    }

    fn default_patterns() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert(
            "email".into(),
            r#"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}"#.into(),
        );
        m.insert("phone_jp".into(), r#"0\d{1,4}-\d{1,4}-\d{4}"#.into());
        m.insert(
            "credit_card".into(),
            r#"\b\d{4}[-\s]?\d{4}[-\s]?\d{4}[-\s]?\d{4}\b"#.into(),
        );
        m.insert(
            "api_key".into(),
            r#"(?i)(api[_-]?key|secret|token|password)\s*[:=]\s*["']?([a-zA-Z0-9_\-]{20,})["']?"#
                .into(),
        );
        m.insert("ipv4".into(), r#"\b(?:\d{1,3}\.){3}\d{1,3}\b"#.into());
        m.insert(
            "uuid".into(),
            r#"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b"#.into(),
        );
        m
    }

    pub fn filter(&self, mut value: Value) -> Result<Value> {
        if !self.enabled {
            return Ok(value);
        }
        self.filter_value(&mut value);
        Ok(value)
    }

    fn filter_value(&self, value: &mut Value) {
        match value {
            Value::String(s) => {
                for (_, re) in &self.patterns {
                    *s = re.replace_all(s, &self.replacement).to_string();
                }
            }
            Value::Array(arr) => {
                for item in arr {
                    self.filter_value(item);
                }
            }
            Value::Object(obj) => {
                for (_, v) in obj {
                    self.filter_value(v);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn filter(enabled: bool) -> PiiFilter {
        let mut patterns = HashMap::new();
        patterns.insert(
            "email".into(),
            r#"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}"#.into(),
        );
        PiiFilter::from_config(PiiConfig {
            patterns,
            redact_replacement: "[REDACTED]".into(),
            enabled,
        })
    }

    #[test]
    fn redacts_strings_recursively() {
        let value = json!({
            "message": "mail me at user@example.com",
            "nested": ["a@b.co", { "text": "x@example.org" }],
            "count": 7
        });
        let out = filter(true).filter(value).expect("filter runs");
        assert_eq!(out["message"], "mail me at [REDACTED]");
        assert_eq!(out["nested"][0], "[REDACTED]");
        assert_eq!(out["nested"][1]["text"], "[REDACTED]");
        assert_eq!(out["count"], 7); // non-strings untouched
    }

    #[test]
    fn disabled_filter_is_passthrough() {
        let value = json!({ "text": "user@example.com" });
        let out = filter(false).filter(value.clone()).expect("filter runs");
        assert_eq!(out, value);
    }

    #[test]
    fn invalid_pattern_is_skipped_not_fatal() {
        let mut patterns = HashMap::new();
        patterns.insert("bad".into(), "[unclosed".into());
        patterns.insert(
            "email".into(),
            r#"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}"#.into(),
        );
        let f = PiiFilter::from_config(PiiConfig {
            patterns,
            redact_replacement: "[X]".into(),
            enabled: true,
        });
        let out = f.filter(json!("a@example.com")).expect("filter runs");
        assert_eq!(out, "[X]");
    }
}
