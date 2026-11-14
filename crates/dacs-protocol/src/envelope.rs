//! Envelopes, attempt scope, worker events and coordinator commands.
//!
//! Rules (normative):
//! * Every state-changing request is an [`Envelope`]. `message_id` is the
//!   deduplication key: a replay with the same id and identical body returns the
//!   original outcome with [`Disposition::Duplicate`] and causes no second effect.
//!   Same id with a different body is refused (`duplicate_mismatch`).
//! * The coordinator persists the effect and the dedup record in one SQLite
//!   transaction *before* answering. No answer means "unknown", never "failed":
//!   retry with the same `message_id`.
//! * Refused requests are not recorded; replaying a refused `message_id` is re-evaluated.
//! * Attempt events carry the attempt's `generation`. Events for an older generation
//!   than the task's current one are ignored with [`Disposition::Stale`].
//! * Attempt events carry a per-attempt `seq` starting at 1 and strictly increasing.
//!   `seq <= last_applied` is ignored with [`Disposition::OutOfOrder`]. Gaps are allowed.
//! * Commands to a device carry a per-device `device_seq`. The inbox redelivers every
//!   command after the acknowledged sequence; workers deduplicate by `command_id`.
use crate::ids::*;
use crate::integration::CandidateSubmission;
use crate::refusal::Refusal;
use crate::task::{Assignment, AttemptOutcome, SessionInfo, TaskContract, TaskMail};
use serde::{Deserialize, Serialize};

pub type Generation = u64;
pub type Seq = u64;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope<T> {
    /// Sender's [`crate::PROTOCOL_VERSION`]; must share `major.minor` with the receiver.
    pub protocol: String,
    pub message_id: MessageId,
    pub correlation_id: CorrelationId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<MessageId>,
    pub sent_at_ms: i64,
    pub body: T,
}

impl<T> Envelope<T> {
    /// New envelope starting a new correlation chain.
    pub fn new(body: T) -> Self {
        Self::correlated(body, CorrelationId::new(), None)
    }
    pub fn correlated(body: T, correlation_id: CorrelationId, causation_id: Option<MessageId>) -> Self {
        Self {
            protocol: crate::PROTOCOL_VERSION.to_owned(),
            message_id: MessageId::new(),
            correlation_id,
            causation_id,
            sent_at_ms: crate::now_ms(),
            body,
        }
    }
}

/// Identity of one attempt at one generation. Every attempt-bound event and command carries it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AttemptScope {
    pub project_id: ProjectId,
    pub task_id: TaskId,
    pub attempt_id: AttemptId,
    pub generation: Generation,
}

/// Coordinator answer to an envelope, sent only after durable commit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ack {
    pub message_id: MessageId,
    pub correlation_id: CorrelationId,
    pub disposition: Disposition,
    /// Endpoint-specific result (e.g. a minted id). Identical on duplicate replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Disposition {
    Applied,
    /// Same `message_id` already applied; original result returned, nothing re-applied.
    Duplicate,
    /// Event's generation is older than the task's current generation; ignored.
    Stale { current_generation: Generation },
    /// Event's `seq` was already applied for this attempt; ignored.
    OutOfOrder { last_applied_seq: Seq },
    Refused { refusal: Refusal },
}

/// Worker -> coordinator: `POST /v1/events` with `Envelope<AttemptEvent>`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AttemptEvent {
    pub scope: AttemptScope,
    pub seq: Seq,
    pub kind: AttemptEventKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttemptEventKind {
    /// Worker accepted the Start command and is preparing the workspace.
    Accepted,
    /// Worker refuses the assignment (e.g. local capacity actually unavailable).
    Rejected { refusal: Refusal },
    /// OMP session started for this attempt (agent work only).
    SessionStarted { session: SessionInfo },
    /// Liveness for this attempt. Device liveness uses the heartbeat endpoint.
    Heartbeat,
    Progress { message: String },
    /// Transport receipt of a mail command. Not consumption.
    MailReceived { mail_id: MailId },
    /// The agent actually read/acted on the mail.
    MailConsumed { mail_id: MailId },
    /// Transport receipt of a revised task contract.
    ContractReceived { rev: u64 },
    /// The agent adopted the revised contract; results after this are judged against `rev`.
    ContractConsumed { rev: u64 },
    /// Immutable candidate output. At most one per attempt.
    CandidateSubmitted { candidate: CandidateSubmission },
    /// Terminal. After this the attempt accepts no further events except duplicates.
    Settled { outcome: AttemptOutcome },
}

/// Coordinator -> worker, delivered through the device inbox.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AttemptCommand {
    pub command_id: CommandId,
    pub device_seq: Seq,
    pub scope: AttemptScope,
    pub issued_at_ms: i64,
    pub kind: AttemptCommandKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttemptCommandKind {
    Start { assignment: Box<Assignment> },
    /// Stop work; the attempt should settle as cancelled.
    Cancel { reason: String },
    /// This generation is no longer current; any further result will be rejected as stale.
    Superseded { current_generation: Generation },
    DeliverMail { mail: TaskMail },
    ContractRevised { contract: TaskContract },
}
