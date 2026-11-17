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

    /// TC-P4: a missing or malformed pattern file must fall back to the
    /// shipped defaults instead of disabling redaction (fail-closed).
    #[tokio::test]
    async fn missing_and_malformed_config_use_default_patterns() {
        let dir = std::env::temp_dir().join("jarvis-context-pii-fallback");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let sample = json!("mail me at user@example.com or 090-1234-5678");
        let expected = json!("mail me at [REDACTED] or [REDACTED]");

        // Missing file: PiiFilter::new must succeed with default patterns.
        let missing = dir.join("missing.toml");
        let config = crate::config::ContextConfig {
            pii_patterns_path: missing.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let filter = PiiFilter::new(&config)
            .await
            .expect("missing file falls back to defaults");
        assert_eq!(
            filter.filter(sample.clone()).expect("filter runs"),
            expected
        );

        // Malformed TOML: same fallback, no panic and no empty pattern set.
        let malformed = dir.join("malformed.toml");
        std::fs::write(&malformed, "not = valid = toml = {").expect("write malformed config");
        let config = crate::config::ContextConfig {
            pii_patterns_path: malformed.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let filter = PiiFilter::new(&config)
            .await
            .expect("malformed file falls back to defaults");
        assert_eq!(filter.filter(sample).expect("filter runs"), expected);

        std::fs::remove_dir_all(&dir).expect("clean temp dir");
    }

    /// A-M19 scope boundary: only string values are scanned. Object keys stay
    /// untouched so the JSON structure is preserved; redacting keys would
    /// change field names and is a separate, still-open design decision.
    #[test]
    fn object_keys_are_preserved_and_only_values_are_redacted() {
        let value = json!({
            "user@example.com": "contact user@example.com",
            "nested": { "owner@corp.example": "a@b.co" }
        });
        let out = filter(true).filter(value).expect("filter runs");

        let obj = out.as_object().expect("top-level object");
        assert!(obj.contains_key("user@example.com"), "key is not redacted");
        assert!(!obj.contains_key("[REDACTED]"), "no key was renamed");
        assert_eq!(out["user@example.com"], "contact [REDACTED]");

        let nested = out["nested"].as_object().expect("nested object");
        assert!(nested.contains_key("owner@corp.example"));
        assert_eq!(out["nested"]["owner@corp.example"], "[REDACTED]");
    }

    /// A-M19 scope boundary: non-string scalars (numbers, booleans, null)
    /// and their array forms are passed through unchanged.
    #[test]
    fn non_string_scalars_are_left_untouched() {
        let value = json!({
            "int": 7,
            "float": 1.5,
            "bool_true": true,
            "bool_false": false,
            "null": null,
            "array": [1, false, null, 2.5, "a@b.co"],
            "email": "a@b.co"
        });
        let original = value.clone();
        let out = filter(true).filter(value).expect("filter runs");

        assert_eq!(out["int"], json!(7));
        assert_eq!(out["float"], json!(1.5));
        assert_eq!(out["bool_true"], json!(true));
        assert_eq!(out["bool_false"], json!(false));
        assert_eq!(out["null"], Value::Null);
        assert!(
            out.as_object().expect("object").contains_key("null"),
            "null field is preserved"
        );
        assert_eq!(out["array"], json!([1, false, null, 2.5, "[REDACTED]"]));
        assert_eq!(out["email"], "[REDACTED]");

        // Only string values changed; the surrounding structure is intact.
        let mut expected = original;
        expected["email"] = json!("[REDACTED]");
        expected["array"][4] = json!("[REDACTED]");
        assert_eq!(out, expected);
    }
}
