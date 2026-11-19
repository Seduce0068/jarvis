//! Canonical approval domain types (P1-7, 案A).
//!
//! `jarvis-core` is the single source of truth for the approval domain; the
//! HTTP JSON and gRPC DTOs are boundary contracts that project these types.
//! Keeping the definition here prevents the policy crate, the transports and
//! the UI from drifting into subtly different shapes.
//!
//! The serialized form is a boundary contract: [`ApprovalDecision`] uses an
//! internally tagged enum (`decision`) and chrono's RFC 3339 timestamps.
//! Golden tests at the bottom of this module lock that contract.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Canonical approval request owned by `jarvis-core`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: Uuid,
    pub principal: String,
    pub action: String,
    pub resource: String,
    pub context: Value,
    pub requested_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// Canonical approval decision.
///
/// `#[serde(tag = "decision")]` is part of the HTTP contract: the JSON object
/// carries `decision: "Approved" | "Denied" | "Pending" | "Expired"` at the
/// top level and flattens the variant fields beside it (`approved_by`,
/// `reason`, `approved_at`). Do not rename the tag or any variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision")]
pub enum ApprovalDecision {
    Approved {
        approved_by: String,
        approved_at: DateTime<Utc>,
    },
    Denied {
        reason: String,
    },
    Pending,
    Expired,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epoch() -> DateTime<Utc> {
        DateTime::from_timestamp(0, 0).expect("epoch is a valid timestamp")
    }

    // ---- golden tests: HTTP decision JSON contract -------------------------

    #[test]
    fn approved_decision_json_is_tagged_with_rfc3339_timestamp() {
        let decision = ApprovalDecision::Approved {
            approved_by: "tester".into(),
            approved_at: epoch(),
        };
        assert_eq!(
            serde_json::to_string(&decision).expect("serializes"),
            r#"{"decision":"Approved","approved_by":"tester","approved_at":"1970-01-01T00:00:00Z"}"#
        );
    }

    #[test]
    fn denied_pending_and_expired_decision_json_is_unchanged() {
        let denied = ApprovalDecision::Denied {
            reason: "not now".into(),
        };
        assert_eq!(
            serde_json::to_string(&denied).expect("serializes"),
            r#"{"decision":"Denied","reason":"not now"}"#
        );
        assert_eq!(
            serde_json::to_string(&ApprovalDecision::Pending).expect("serializes"),
            r#"{"decision":"Pending"}"#
        );
        assert_eq!(
            serde_json::to_string(&ApprovalDecision::Expired).expect("serializes"),
            r#"{"decision":"Expired"}"#
        );
    }

    // ---- regression: serde round-trip of all four variants -----------------

    #[test]
    fn all_four_decision_variants_round_trip() {
        let cases = [
            ApprovalDecision::Approved {
                approved_by: "alice".into(),
                approved_at: epoch(),
            },
            ApprovalDecision::Denied {
                reason: "policy".into(),
            },
            ApprovalDecision::Pending,
            ApprovalDecision::Expired,
        ];
        for decision in cases {
            let json = serde_json::to_string(&decision).expect("serializes");
            let decoded: ApprovalDecision = serde_json::from_str(&json).expect("parses");
            assert_eq!(decoded, decision, "round trip failed for {json}");
        }
    }
}
