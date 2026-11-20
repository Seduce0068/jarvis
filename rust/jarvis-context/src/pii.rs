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

/// Reject mistyped keys instead of letting `toml::from_str` silently ignore
/// them. Without this, a typo such as `patern` instead of `patterns` parses
/// successfully and quietly drops the intended PII patterns; the caller then
/// sees a "valid" but weakened filter. Unknown keys now fail parsing, which
/// routes through the warn + fail-closed default-pattern path in `new()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
            // I91-A: `Replacer for &str` interprets `$0`/`$1`/`$name` as
            // capture references. A config value of `redact_replacement =
            // "$0"` would therefore reinsert the matched PII verbatim and
            // turn fail-closed redaction into fail-open disclosure.
            // `NoExpand` forces literal replacement (available since regex
            // 1.0; resolved here as 1.13.1 per Cargo.lock).
            out = re
                .replace_all(&out, regex::NoExpand(self.replacement.as_str()))
                .into_owned();
        }
        out
    }

    /// Return `base` unchanged when `map` does not contain it yet; otherwise
    /// append `#1`, `#2`, ... until an unused key is found. Redaction can
    /// collapse several distinct keys (e.g. two e-mail addresses) onto the
    /// same string, and a redacted key can also collide with an ordinary key
    /// that already equals the replacement. A plain `insert` would then let
    /// the last value overwrite earlier ones and silently drop data. Every
    /// collision gets a numeric suffix so all original values survive, while
    /// the emitted keys never contain the PII that produced them.
    fn unique_key(map: &Map<String, Value>, base: String) -> String {
        if !map.contains_key(&base) {
            return base;
        }
        let mut suffix = 1usize;
        loop {
            let candidate = format!("{}#{}", base, suffix);
            if !map.contains_key(&candidate) {
                return candidate;
            }
            suffix += 1;
        }
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
                // value; ordinary keys are copied through unchanged. Because
                // redaction may make several keys identical, uniquify each
                // key against what has already been inserted before writing,
                // so no value is lost to a last-write-wins collision.
                let mut rebuilt = Map::new();
                for (key, mut v) in std::mem::take(obj) {
                    let base_key = if self.is_pii(&key) {
                        self.redact_string(&key)
                    } else {
                        key
                    };
                    self.filter_value(&mut v, depth + 1);
                    let unique_key = Self::unique_key(&rebuilt, base_key);
                    rebuilt.insert(unique_key, v);
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
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// pid + counter keeps concurrent cargo test processes from sharing (and
    /// deleting) the same scratch directory.
    fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
        let n = TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "jarvis-context-pii-{tag}-{}-{n}",
            std::process::id()
        ))
    }

    /// Absolute path to the shipped config, baked in at compile time.
    const SHIPPED_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../infra/config/pii_patterns.toml"
    );

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
        let dir = unique_temp_dir("fallback");
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

    /// In-memory `MakeWriter` target: no global subscriber, so other tests
    /// can still install their own.
    #[derive(Clone)]
    struct ConfigLogBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for ConfigLogBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("log buffer lock")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Run `future` under a thread-local WARN subscriber and return its output
    /// plus the captured log text. A current-thread runtime keeps every event
    /// on the subscribed thread; the lock serializes capture across tests.
    fn capture_warns<F, T>(future: F) -> (T, String)
    where
        F: std::future::Future<Output = T>,
    {
        static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = CAPTURE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = ConfigLogBuffer(buffer.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_target(false)
            .with_writer(move || writer.clone())
            .finish();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        let output = tracing::subscriber::with_default(subscriber, || runtime.block_on(future));
        let captured = buffer.lock().expect("log buffer lock").clone();
        let logs = String::from_utf8(captured).expect("captured logs are UTF-8");
        (output, logs)
    }

    /// I86-A: the shipped TOML must deserialize into the current `PiiConfig`
    /// schema. With patterns at the top level `toml::from_str` ignored them
    /// and silently fell back to the six default patterns.
    #[test]
    fn shipped_pii_config_matches_pii_config_schema() {
        const SHIPPED: &str = include_str!("../../../infra/config/pii_patterns.toml");
        let parsed: PiiConfig = toml::from_str(SHIPPED).expect("shipped TOML matches PiiConfig");
        assert_eq!(parsed.patterns.len(), 14, "all shipped patterns are loaded");
        assert!(parsed.enabled, "filter stays enabled");
        assert_eq!(parsed.redact_replacement, "[REDACTED]");

        for name in [
            "email",
            "phone_jp",
            "credit_card",
            "api_key",
            "ipv4",
            "uuid",
            "aws_access_key",
            "aws_secret_key",
            "github_token",
            "slack_token",
            "bearer_token",
            "private_key",
            "db_connection",
            "jwt",
        ] {
            assert!(
                parsed.patterns.contains_key(name),
                "missing pattern: {name}"
            );
        }
    }

    /// I86-A: loading the shipped config must not warn (no read/parse or
    /// invalid-regex failure), and its extra patterns must redact AWS,
    /// GitHub and JWT values that the six-pattern default omitted.
    #[test]
    fn shipped_config_loads_without_warning_and_redacts_provider_tokens() {
        const PROBE: &str = "pii-config-capture-probe";
        let config = crate::config::ContextConfig {
            pii_patterns_path: SHIPPED_PATH.into(),
            ..Default::default()
        };

        let (filter, logs) = capture_warns(async {
            let loaded = PiiFilter::new(&config).await.expect("shipped config loads");
            tracing::warn!(probe = PROBE, "capture control event");
            loaded
        });

        assert!(
            logs.contains(PROBE),
            "capture must observe WARN events: {logs}"
        );
        assert!(
            !logs.contains("Failed to read PII pattern config file"),
            "shipped config must be readable: {logs}"
        );
        assert!(
            !logs.contains("Failed to parse PII pattern config file"),
            "shipped config must parse: {logs}"
        );
        assert!(
            !logs.contains("Invalid PII pattern"),
            "all shipped regexes must compile: {logs}"
        );

        assert_eq!(filter.patterns.len(), 14, "all 14 patterns are active");

        let aws = format!("AKIA{}", "A".repeat(16));
        let github = format!("ghp_{}", "a".repeat(36));
        let jwt = format!("eyJ{}.{}.{}", "a".repeat(8), "b".repeat(8), "c".repeat(8));
        let out = filter
            .filter(json!({ "aws": aws, "github": github, "jwt": jwt }))
            .expect("filter runs");

        assert_eq!(out["aws"], "[REDACTED]");
        assert_eq!(out["github"], "[REDACTED]");
        assert_eq!(out["jwt"], "[REDACTED]");
    }

    /// I88-B: an unknown top-level key (e.g. the `patern` typo of `patterns`)
    /// must make TOML deserialization fail instead of being silently ignored.
    /// The failed parse must warn and fall back to the fail-closed shipped
    /// defaults, so a mistyped config cannot silently drop PII patterns.
    #[test]
    fn unknown_config_key_fails_closed_to_default_patterns() {
        const PROBE: &str = "pii-config-unknown-key-probe";
        const TYPO_TOML: &str = r#"
enabled = true
redact_replacement = "[REDACTED]"
patern = { email = "[a-z]+@[a-z]+" }
[patterns]
email = "[a-z]+@[a-z]+"
"#;

        // All three known fields are present and valid; only the extra
        // `patern` key is unknown. Without `deny_unknown_fields` this parses.
        let parsed = toml::from_str::<PiiConfig>(TYPO_TOML);
        assert!(parsed.is_err(), "unknown key must fail deserialization");

        let dir = unique_temp_dir("unknown-key");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("typo.toml");
        std::fs::write(&config_path, TYPO_TOML).expect("write typo config");

        let config = crate::config::ContextConfig {
            pii_patterns_path: config_path.to_string_lossy().into_owned(),
            ..Default::default()
        };

        let (filter, logs) = capture_warns(async {
            let loaded = PiiFilter::new(&config).await.expect("parse is not fatal");
            tracing::warn!(probe = PROBE, "capture control event");
            loaded
        });

        std::fs::remove_dir_all(&dir).expect("clean temp dir");

        assert!(
            logs.contains(PROBE),
            "capture must observe WARN events: {logs}"
        );
        assert!(
            logs.contains("Failed to parse PII pattern config file"),
            "unknown-key parse failure must warn: {logs}"
        );

        // Fail-closed: redaction stays enabled with the six default patterns,
        // not an empty set and not the partly typo'd on-disk config.
        assert!(filter.enabled, "filter stays enabled after fallback");
        assert_eq!(filter.replacement, "[REDACTED]");
        assert_eq!(
            filter.patterns.len(),
            PiiFilter::default_patterns().len(),
            "fallback must use the shipped default patterns"
        );
        let out = filter
            .filter(json!({ "text": "mail me at user@example.com" }))
            .expect("filter runs");
        assert_eq!(out["text"], "mail me at [REDACTED]");
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

    /// I26-C regression: two distinct PII keys collapse to the same
    /// "[REDACTED]" string. Both values must survive under unique keys and no
    /// raw PII may leak into any emitted key.
    #[test]
    fn colliding_pii_keys_preserve_all_values() {
        let value = json!({
            "alice@example.com": "alice-payload",
            "bob@example.com": "bob-payload"
        });
        let out = filter(true).filter(value).expect("filter runs");
        let obj = out.as_object().expect("object");

        assert_eq!(obj.len(), 2, "no value was dropped by the key collision");
        assert!(obj.contains_key("[REDACTED]"));
        assert!(obj.contains_key("[REDACTED]#1"));
        for key in obj.keys() {
            assert!(!key.contains("alice@example.com"), "no PII in key: {key}");
            assert!(!key.contains("bob@example.com"), "no PII in key: {key}");
        }
        let mut values: Vec<&str> = obj
            .values()
            .map(|v| v.as_str().expect("string value"))
            .collect();
        values.sort_unstable();
        assert_eq!(values, vec!["alice-payload", "bob-payload"]);
    }

    /// I26-C regression: three colliding PII keys must receive incrementing
    /// suffixes (#1, #2) instead of overwriting one another.
    #[test]
    fn three_colliding_pii_keys_get_incrementing_suffixes() {
        let value = json!({
            "a@example.com": "value-a",
            "b@example.com": "value-b",
            "c@example.com": "value-c"
        });
        let out = filter(true).filter(value).expect("filter runs");
        let obj = out.as_object().expect("object");

        assert_eq!(obj.len(), 3);
        assert!(obj.contains_key("[REDACTED]"));
        assert!(obj.contains_key("[REDACTED]#1"));
        assert!(obj.contains_key("[REDACTED]#2"));
        let mut values: Vec<&str> = obj
            .values()
            .map(|v| v.as_str().expect("string value"))
            .collect();
        values.sort_unstable();
        assert_eq!(values, vec!["value-a", "value-b", "value-c"]);
    }

    /// I26-C regression: a redacted PII key colliding with an ordinary key
    /// that already equals the replacement string must not clobber it. Both
    /// the ordinary value and the redacted value survive.
    #[test]
    fn redacted_key_colliding_with_ordinary_key_preserves_both() {
        let value = json!({
            "[REDACTED]": "ordinary-value",
            "alice@example.com": "hidden-value"
        });
        let out = filter(true).filter(value).expect("filter runs");
        let obj = out.as_object().expect("object");

        assert_eq!(obj.len(), 2, "ordinary and redacted keys coexist");
        for key in obj.keys() {
            assert!(!key.contains("alice@example.com"), "no PII in key: {key}");
        }
        let mut values: Vec<&str> = obj
            .values()
            .map(|v| v.as_str().expect("string value"))
            .collect();
        values.sort_unstable();
        assert_eq!(values, vec!["hidden-value", "ordinary-value"]);
    }

    /// I26-C regression: an ordinary key that already occupies a suffix slot
    /// is respected, so uniquification skips it rather than overwriting it.
    #[test]
    fn suffix_slot_occupied_by_ordinary_key_is_skipped() {
        let value = json!({
            "[REDACTED]#1": "reserved-value",
            "alice@example.com": "alice-payload",
            "bob@example.com": "bob-payload"
        });
        let out = filter(true).filter(value).expect("filter runs");
        let obj = out.as_object().expect("object");

        assert_eq!(obj.len(), 3);
        assert_eq!(out["[REDACTED]#1"], "reserved-value");
        assert!(obj.contains_key("[REDACTED]"));
        assert!(obj.contains_key("[REDACTED]#2"), "skipped occupied #1 slot");
        let mut values: Vec<&str> = obj
            .values()
            .map(|v| v.as_str().expect("string value"))
            .collect();
        values.sort_unstable();
        assert_eq!(
            values,
            vec!["alice-payload", "bob-payload", "reserved-value"]
        );
    }

    /// I91-A regression: the replacement is inserted literally. With the old
    /// `&str` Replacer, `redact_replacement = "$0"` expands to the whole match
    /// and restores the PII (fail-open to the LLM). `NoExpand` must keep `$0`
    /// as text so no e-mail or API key survives redaction.
    #[test]
    fn dollar_significant_replacement_does_not_restore_pii() {
        let mut patterns = HashMap::new();
        patterns.insert(
            "email".into(),
            r#"[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}"#.into(),
        );
        patterns.insert(
            "api_key".into(),
            r#"(?i)(api[_-]?key|secret|token|password)\s*[:=]\s*["']?([a-zA-Z0-9_\-]{20,})["']?"#
                .into(),
        );
        let filter = PiiFilter::from_config(PiiConfig {
            patterns,
            redact_replacement: "$0".into(),
            enabled: true,
        });

        let probe_value = "X".repeat(24);
        let input = json!({
            "email": "mail me at user@example.com",
            "probe_key": format!("{} = {}", concat!("api_", "key"), probe_value),
        });
        let out = filter.filter(input).expect("filter runs");

        // Literal "$0", not the captured match, must be written out.
        assert_eq!(out["email"], "mail me at $0");
        assert_eq!(out["probe_key"], "$0");

        // Defense in depth: no raw PII may survive anywhere in the output.
        let rendered = out.to_string();
        assert!(
            !rendered.contains("user@example.com"),
            "email leaked: {rendered}"
        );
        assert!(
            !rendered.contains(&probe_value),
            "sensitive token leaked: {rendered}"
        );
    }
}
