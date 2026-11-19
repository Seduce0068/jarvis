//! PII detection and redaction

use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use tracing::warn;

/// Upper bound on the value walker's recursion. `serde_json` already caps
/// deserialized input at this depth; the guard protects programmatically
/// built trees from exhausting the stack.
const MAX_DEPTH: usize = 128;

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
        // Never silently swallow a config load/parse failure. The old code used
        // unwrap_or_default()/unwrap_or_else(), so a broken on-disk config was
        // indistinguishable from a valid one that simply happened to use the
        // defaults. Log loudly and keep the fail-closed fallback (redaction on,
        // shipped patterns) so an invalid config cannot disable PII filtering.
        let content = match tokio::fs::read_to_string(&config.pii_patterns_path).await {
            Ok(content) => content,
            Err(error) => {
                warn!(
                    path = %config.pii_patterns_path,
                    error = %error,
                    "Failed to read PII pattern config file; applying shipped default \
                     patterns (fail-closed)"
                );
                return Ok(Self::from_config(Self::default_config()));
            }
        };

        let pii_config: PiiConfig = match toml::from_str(&content) {
            Ok(parsed) => parsed,
            Err(error) => {
                warn!(
                    path = %config.pii_patterns_path,
                    error = %error,
                    "Failed to parse PII pattern config file; applying shipped default \
                     patterns (fail-closed)"
                );
                Self::default_config()
            }
        };

        Ok(Self::from_config(pii_config))
    }

    /// Fail-closed configuration: redaction enabled with the shipped patterns.
    fn default_config() -> PiiConfig {
        PiiConfig {
            patterns: Self::default_patterns(),
            redact_replacement: "[REDACTED]".into(),
            enabled: true,
        }
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
        self.filter_value(&mut value, 0);
        Ok(value)
    }

    /// True when any configured pattern matches part of `s`. Used to decide
    /// whether an object key must be redacted.
    fn is_pii(&self, s: &str) -> bool {
        self.patterns.iter().any(|(_, re)| re.is_match(s))
    }

    /// Apply every configured pattern to `s`, replacing matches with the
    /// configured redaction string. Shared by string values and object keys.
    fn redact_string(&self, s: &str) -> String {
        let mut out = s.to_owned();
        for (_, re) in &self.patterns {
            out = re.replace_all(&out, &self.replacement).into_owned();
        }
        out
    }

    fn filter_value(&self, value: &mut Value, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        match value {
            Value::String(s) => *s = self.redact_string(s),
            Value::Array(arr) => {
                for item in arr {
                    self.filter_value(item, depth + 1);
                }
            }
            Value::Object(obj) => {
                // Keys cannot be renamed in place, so rebuild the map. A key
                // matching a PII pattern is redacted with the same rules as a
                // value; ordinary keys are copied through unchanged.
                let mut rebuilt = Map::new();
                for (key, mut v) in std::mem::take(obj) {
                    let new_key = if self.is_pii(&key) {
                        self.redact_string(&key)
                    } else {
                        key
                    };
                    self.filter_value(&mut v, depth + 1);
                    rebuilt.insert(new_key, v);
                }
                *obj = rebuilt;
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

    /// I12-E: ordinary object keys are preserved unchanged; only string
    /// values are redacted. Keys that themselves contain PII are covered by
    /// `pii_matching_object_keys_are_redacted`.
    #[test]
    fn object_keys_are_preserved_and_only_values_are_redacted() {
        let value = json!({
            "user_id": "contact user@example.com",
            "nested": { "owner_name": "a@b.co" }
        });
        let out = filter(true).filter(value).expect("filter runs");

        let obj = out.as_object().expect("top-level object");
        assert!(obj.contains_key("user_id"), "ordinary key is preserved");
        assert!(!obj.contains_key("[REDACTED]"), "no key was renamed");
        assert_eq!(out["user_id"], "contact [REDACTED]");

        let nested = out["nested"].as_object().expect("nested object");
        assert!(nested.contains_key("owner_name"));
        assert_eq!(out["nested"]["owner_name"], "[REDACTED]");
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

    /// I12-E regression: a key whose name matches a PII pattern is redacted at
    /// every depth, including inside arrays, while non-matching siblings stay
    /// intact.
    #[test]
    fn pii_matching_object_keys_are_redacted() {
        let value = json!({
            "user@example.com": "safe value",
            "nested": {
                "owner@corp.example": "a@b.co",
                "keep_me": "plain"
            },
            "items": [
                { "contact@example.org": "x" },
                "plain string"
            ]
        });
        let out = filter(true).filter(value).expect("filter runs");

        let obj = out.as_object().expect("top-level object");
        assert!(!obj.contains_key("user@example.com"), "PII key was renamed");
        assert_eq!(out["[REDACTED]"], "safe value");

        let nested = out["nested"].as_object().expect("nested object");
        assert!(!nested.contains_key("owner@corp.example"));
        assert_eq!(nested["[REDACTED]"], "[REDACTED]");
        assert_eq!(nested["keep_me"], "plain");

        let items = out["items"].as_array().expect("items array");
        assert_eq!(items[0]["[REDACTED]"], "x");
        assert_eq!(items[1], "plain string");
    }

    /// I12-E: disabling the filter leaves PII-bearing keys untouched.
    #[test]
    fn disabled_filter_preserves_pii_keys() {
        let value = json!({ "user@example.com": "a@b.co" });
        let out = filter(false).filter(value.clone()).expect("filter runs");
        assert_eq!(out, value);
    }

    /// I12-E: when a pattern matches only part of a key, the matching substring
    /// is replaced and the surrounding text is preserved.
    #[test]
    fn pii_substring_in_key_is_redacted() {
        let value = json!({ "id=user@example.com": "ok" });
        let out = filter(true).filter(value).expect("filter runs");
        let obj = out.as_object().expect("object");
        assert!(!obj.contains_key("id=user@example.com"));
        assert!(obj.contains_key("id=[REDACTED]"));
        assert_eq!(out["id=[REDACTED]"], "ok");
    }

    /// I12-E: the depth guard prevents stack exhaustion on programmatically
    /// built trees deeper than serde_json's own parse limit; shallow redaction
    /// must still work.
    #[test]
    fn deeply_nested_value_does_not_overflow_stack() {
        let mut deep = json!("user@example.com");
        for _ in 0..MAX_DEPTH + 16 {
            deep = json!({ "nested": deep });
        }
        let value = json!({ "email": "user@example.com", "deep": deep });
        let out = filter(true).filter(value).expect("filter runs");

        assert_eq!(out["email"], "[REDACTED]");
        assert!(out["deep"].is_object());
    }
}
