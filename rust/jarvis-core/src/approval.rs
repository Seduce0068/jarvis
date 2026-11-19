//! Canonical approval domain types (P1-7, 案A).
//!
//! `jarvis-core` is the single source of truth for the approval domain; the
//! HTTP JSON and gRPC DTOs are boundary contracts that project these types.
//! Keeping the definition here prevents the policy crate, the transports and
//! the UI from drifting into subtly different shapes.
//!
//! Two compatibility rules are intentional:
//!
//! * the legacy [`crate::ApprovalRequest`] / [`crate::ApprovalDecision`] at the
//!   crate root stay untouched (`String` id, `i64` timestamps, two decision
//!   variants), so existing callers keep compiling without a `deprecated`
//!   attribute (CI builds with `-D warnings`);
//! * `TryFrom<crate::ApprovalRequest>` is the only bridge from that legacy
//!   shape. Malformed ids or out-of-range timestamps become an explicit
//!   [`ApprovalConversionError`] instead of being silently defaulted.
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

/// Explicit failure modes when converting a legacy [`crate::ApprovalRequest`]
/// into the canonical [`ApprovalRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalConversionError {
    /// The legacy `id` is not a parseable UUID.
    InvalidId { value: String },
    /// The legacy `requested_at` cannot be represented as `DateTime<Utc>`.
    InvalidRequestedAt { value: i64 },
    /// The legacy `expires_at` cannot be represented as `DateTime<Utc>`.
    InvalidExpiresAt { value: i64 },
}

impl std::fmt::Display for ApprovalConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidId { value } => {
                write!(f, "invalid approval id '{value}': expected a UUID")
            }
            Self::InvalidRequestedAt { value } => {
                write!(
                    f,
                    "invalid requested_at {value}: out of range for DateTime<Utc>"
                )
            }
            Self::InvalidExpiresAt { value } => {
                write!(
                    f,
                    "invalid expires_at {value}: out of range for DateTime<Utc>"
                )
            }
        }
    }
}

impl std::error::Error for ApprovalConversionError {}

impl TryFrom<crate::ApprovalRequest> for ApprovalRequest {
    type Error = ApprovalConversionError;

    fn try_from(legacy: crate::ApprovalRequest) -> Result<Self, Self::Error> {
        let id = Uuid::parse_str(&legacy.id).map_err(|_| ApprovalConversionError::InvalidId {
            value: legacy.id.clone(),
        })?;
        let requested_at = DateTime::from_timestamp(legacy.requested_at, 0).ok_or(
            ApprovalConversionError::InvalidRequestedAt {
                value: legacy.requested_at,
            },
        )?;
        let expires_at = DateTime::from_timestamp(legacy.expires_at, 0).ok_or(
            ApprovalConversionError::InvalidExpiresAt {
                value: legacy.expires_at,
            },
        )?;
        Ok(Self {
            id,
            principal: legacy.principal,
            action: legacy.action,
            resource: legacy.resource,
            context: legacy.context,
            requested_at,
            expires_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_ID: &str = "3f9a1c2e-7b4d-4a1f-9c33-0d2e5f6a7b88";

    fn epoch() -> DateTime<Utc> {
        DateTime::from_timestamp(0, 0).expect("epoch is a valid timestamp")
    }

    fn legacy_request(id: &str, requested_at: i64, expires_at: i64) -> crate::ApprovalRequest {
        crate::ApprovalRequest {
            id: id.into(),
            principal: "jarvis".into(),
            action: "execute".into(),
            resource: "tool:git.commit".into(),
            context: serde_json::json!({ "key": "value" }),
            requested_at,
            expires_at,
        }
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

    // ---- regression: legacy conversion success / failure -------------------

    #[test]
    fn legacy_request_converts_with_valid_id_and_timestamps() {
        let canonical =
            ApprovalRequest::try_from(legacy_request(VALID_ID, 0, 60)).expect("valid converts");
        assert_eq!(
            canonical.id,
            Uuid::parse_str(VALID_ID).expect("uuid parses")
        );
        assert_eq!(canonical.principal, "jarvis");
        assert_eq!(canonical.action, "execute");
        assert_eq!(canonical.resource, "tool:git.commit");
        assert_eq!(canonical.context, serde_json::json!({ "key": "value" }));
        assert_eq!(canonical.requested_at, epoch());
        assert_eq!(
            canonical.expires_at,
            epoch() + chrono::Duration::seconds(60)
        );
    }

    #[test]
    fn legacy_request_rejects_invalid_id() {
        let error = ApprovalRequest::try_from(legacy_request("not-a-uuid", 0, 60))
            .expect_err("malformed id must fail explicitly");
        assert_eq!(
            error,
            ApprovalConversionError::InvalidId {
                value: "not-a-uuid".into()
            }
        );
    }

    #[test]
    fn legacy_request_rejects_out_of_range_timestamps() {
        let error = ApprovalRequest::try_from(legacy_request(VALID_ID, i64::MAX, 60))
            .expect_err("out-of-range requested_at must fail explicitly");
        assert_eq!(
            error,
            ApprovalConversionError::InvalidRequestedAt { value: i64::MAX }
        );

        let error = ApprovalRequest::try_from(legacy_request(VALID_ID, 0, i64::MIN))
            .expect_err("out-of-range expires_at must fail explicitly");
        assert_eq!(
            error,
            ApprovalConversionError::InvalidExpiresAt { value: i64::MIN }
        );
    }

    // ---- regression: legacy JSON can still be read -------------------------

    #[test]
    fn legacy_approval_json_is_readable_and_convertible() {
        let legacy_json = r#"{
            "id": "3f9a1c2e-7b4d-4a1f-9c33-0d2e5f6a7b88",
            "principal": "jarvis",
            "action": "execute",
            "resource": "tool:git.commit",
            "context": {"reason": "nightly build"},
            "requested_at": 0,
            "expires_at": 60
        }"#;
        let legacy: crate::ApprovalRequest =
            serde_json::from_str(legacy_json).expect("legacy JSON still deserializes");
        let canonical = ApprovalRequest::try_from(legacy).expect("converts after parsing");
        assert_eq!(
            canonical.id,
            Uuid::parse_str(VALID_ID).expect("uuid parses")
        );
        assert_eq!(canonical.requested_at, epoch());
        assert_eq!(canonical.context["reason"], "nightly build");
    }

    #[test]
    fn legacy_two_variant_decision_json_still_deserializes() {
        // The old `ApprovalDecision` had no tag; both variants must remain
        // readable so already-queued legacy decisions do not break.
        let approved: crate::ApprovalDecision =
            serde_json::from_str(r#""Approved""#).expect("legacy Approved parses");
        assert!(matches!(approved, crate::ApprovalDecision::Approved));
        let denied: crate::ApprovalDecision =
            serde_json::from_str(r#"{"Denied":{"reason":"no"}}"#).expect("legacy Denied parses");
        assert!(matches!(
            denied,
            crate::ApprovalDecision::Denied { reason } if reason == "no"
        ));
    }
}
