//! Explicit refusals. Every non-2xx coordinator response body is a [`Refusal`].
use crate::capacity::CapacityRefusal;
use serde::{Deserialize, Serialize};

str_enum! {
    pub enum RefusalCode {
        Unauthenticated = "unauthenticated",
        Forbidden = "forbidden",
        NotFound = "not_found",
        InvalidRequest = "invalid_request",
        ProtocolMismatch = "protocol_mismatch",
        DuplicateMismatch = "duplicate_mismatch",
        StaleGeneration = "stale_generation",
        InvalidState = "invalid_state",
        CycleDetected = "cycle_detected",
        CapabilityUnavailable = "capability_unavailable",
        LimitExceeded = "limit_exceeded",
        CapacityUnavailable = "capacity_unavailable",
        PublicationBlocked = "publication_blocked",
        ChecksNotSatisfied = "checks_not_satisfied",
        RevisionMismatch = "revision_mismatch",
        DigestMismatch = "digest_mismatch",
        Conflict = "conflict",
        Internal = "internal",
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Refusal {
    pub code: RefusalCode,
    pub message: String,
    /// Present when `code == capacity_unavailable` (and for limit refusals on delegation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<CapacityRefusal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl Refusal {
    pub fn new(code: RefusalCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), capacity: None, details: None }
    }
}
