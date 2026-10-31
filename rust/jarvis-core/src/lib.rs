//! JARVIS Core - Shared types and traits

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Unified event type used across JARVIS services
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedEvent {
    pub id: Uuid,
    pub source: EventSource,
    pub event_type: String,
    pub payload: Value,
    pub timestamp: DateTime<Utc>,
    pub correlation_id: String,
}

/// Event source enumeration
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    HomeAssistant,
    Android,
    Gmail,
    Calendar,
    Web,
    Finance,
    Manual,
}

/// Memory entry for context storage
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: Uuid,
    pub content: String,
    pub embedding: Vec<f32>,
    pub metadata: Value,
    pub source: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Policy check request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyCheckRequest {
    pub principal: String,
    pub action: String,
    pub resource: String,
    pub context: Value,
}

/// Policy check response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyCheckResponse {
    pub allowed: bool,
}

/// Approval request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub principal: String,
    pub action: String,
    pub resource: String,
    pub context: Value,
    pub requested_at: i64,
    pub expires_at: i64,
}

/// Approval decision
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ApprovalDecision {
    Approved,
    Denied { reason: String },
}

/// Kill switch status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillSwitchStatus {
    pub activated: bool,
    pub activated_at: Option<i64>,
    pub reason: Option<String>,
    pub activated_by: Option<String>,
}

/// MCP tool execution request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteToolRequest {
    pub tool: String,
    pub args: Value,
    pub principal: String,
}

/// MCP tool execution response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteToolResponse {
    pub result: Value,
}

impl std::fmt::Display for EventSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EventSource::HomeAssistant => write!(f, "home_assistant"),
            EventSource::Android => write!(f, "android"),
            EventSource::Gmail => write!(f, "gmail"),
            EventSource::Calendar => write!(f, "calendar"),
            EventSource::Web => write!(f, "web"),
            EventSource::Finance => write!(f, "finance"),
            EventSource::Manual => write!(f, "manual"),
        }
    }
}

impl std::str::FromStr for EventSource {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "home_assistant" => Ok(EventSource::HomeAssistant),
            "android" => Ok(EventSource::Android),
            "gmail" => Ok(EventSource::Gmail),
            "calendar" => Ok(EventSource::Calendar),
            "web" => Ok(EventSource::Web),
            "finance" => Ok(EventSource::Finance),
            "manual" => Ok(EventSource::Manual),
            _ => Err(anyhow::anyhow!("Unknown event source: {}", s)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_sources() -> [EventSource; 7] {
        [
            EventSource::HomeAssistant,
            EventSource::Android,
            EventSource::Gmail,
            EventSource::Calendar,
            EventSource::Web,
            EventSource::Finance,
            EventSource::Manual,
        ]
    }

    #[test]
    fn event_source_round_trips_through_display_and_from_str() {
        for source in all_sources() {
            let rendered = source.to_string();
            let parsed = rendered
                .parse::<EventSource>()
                .unwrap_or_else(|error| panic!("{rendered} does not parse: {error}"));
            assert_eq!(parsed, source);
        }
    }

    #[test]
    fn from_str_is_case_insensitive() {
        assert_eq!(
            "HOME_ASSISTANT".parse::<EventSource>().expect("uppercase"),
            EventSource::HomeAssistant
        );
    }

    #[test]
    fn unknown_event_source_is_rejected() {
        assert!("mqtt".parse::<EventSource>().is_err());
    }

    #[test]
    fn unified_event_serializes_the_source_as_snake_case() {
        let event = UnifiedEvent {
            id: Uuid::nil(),
            source: EventSource::HomeAssistant,
            event_type: "state_changed".into(),
            payload: serde_json::json!({ "entity_id": "light.kitchen" }),
            timestamp: DateTime::from_timestamp(0, 0).expect("valid timestamp"),
            correlation_id: "corr-1".into(),
        };

        let json = serde_json::to_value(&event).expect("event serializes");
        assert_eq!(json["source"], "home_assistant");
        assert_eq!(json["event_type"], "state_changed");
    }

    #[test]
    fn memory_entry_round_trips() {
        let memory = MemoryEntry {
            id: Uuid::nil(),
            content: "remember this".into(),
            embedding: vec![0.25, 0.5],
            metadata: serde_json::json!({ "kind": "note" }),
            source: "test".into(),
            created_at: DateTime::from_timestamp(0, 0).expect("valid timestamp"),
            updated_at: DateTime::from_timestamp(0, 0).expect("valid timestamp"),
            expires_at: None,
        };

        let encoded = serde_json::to_string(&memory).expect("memory serializes");
        let decoded: MemoryEntry = serde_json::from_str(&encoded).expect("memory parses");
        assert_eq!(decoded.content, memory.content);
        assert_eq!(decoded.embedding, memory.embedding);
        assert_eq!(decoded.metadata, memory.metadata);
    }
}
