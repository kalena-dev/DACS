use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod workflow;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Invalid,
    Unauthorized,
    Forbidden,
    NotFound,
    Conflict,
    CapacityUnavailable,
    Io,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
}
impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
pub type Result<T, E = ApiError> = std::result::Result<T, E>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Principal {
    pub device_id: Option<String>,
    pub is_admin: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Resources {
    #[serde(default)]
    pub agent_slots: u32,
    #[serde(default)]
    pub cpu_millis: u32,
    #[serde(default)]
    pub memory_mb: u64,
    #[serde(default)]
    pub disk_mb: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    Workspace,
    External,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Waiting,
    Ready,
    Executing,
    Validating,
    Publishing,
    Accepted,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub target_branch: String,
    pub contract_revision: u64,
    pub policy_revision: u64,
    pub max_tasks: u32,
    pub max_depth: u32,
    pub requirements: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskExecution {
    #[default]
    Agent,
    Workflow {
        request: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowCompletion {
    pub success: bool,
    pub artifact_digest: String,
    pub evidence: Vec<CheckEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSpec {
    pub project_id: String,
    pub title: String,
    pub instructions: String,
    pub source_commit: String,
    #[serde(default)]
    pub execution: TaskExecution,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    pub resources: Resources,
    pub effect_class: EffectClass,
    pub lease_seconds: u64,
    pub max_runtime_seconds: u64,
    pub contract_revision: u64,
    pub policy_revision: u64,
    #[serde(default)]
    pub max_retries: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: String,
    pub spec: TaskSpec,
    pub state: TaskState,
    pub generation: u64,
    pub parent_task_id: Option<String>,
    pub attempt_id: Option<String>,
    pub integrated_sha: Option<String>,
    #[serde(default)]
    pub workflow_result: Option<WorkflowCompletion>,
    pub blocked_reason: Option<String>,
    pub cancel_requested: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerRegistration {
    pub name: String,
    pub physical_host_id: String,
    pub os: String,
    pub architecture: String,
    pub capabilities: Vec<String>,
    pub capacity: Resources,
    pub omp_version: String,
    pub policy_revision: u64,
    pub owner_consented: bool,
    pub consent_host: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerRecord {
    pub id: String,
    pub project_id: String,
    pub registration: WorkerRegistration,
    pub approved: bool,
    pub revoked: bool,
    pub draining: bool,
    pub last_seen: i64,
    pub reserved: Resources,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub id: String,
    pub task_id: String,
    pub worker_id: String,
    pub generation: u64,
    pub state: String,
    pub lease_expires_at: i64,
    pub started_at: i64,
    pub settled: bool,
    pub pending_children: Vec<String>,
    pub last_sequence: u64,
    pub stop_reason: Option<String>,
    pub resources: Resources,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckEvidence {
    pub name: String,
    pub command: String,
    pub source_sha: String,
    pub passed: bool,
    pub artifact_digest: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateSubmission {
    pub task_id: String,
    pub attempt_id: String,
    pub generation: u64,
    pub commit_sha: String,
    pub contract_revision: u64,
    pub policy_revision: u64,
    #[serde(default)]
    pub evidence: Vec<CheckEvidence>,
    #[serde(default)]
    pub unresolved: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateRecord {
    pub id: String,
    pub project_id: String,
    pub submission: CandidateSubmission,
    pub integration_sha: Option<String>,
    pub expected_head: Option<String>,
    pub decision: Option<ReviewDecision>,
    pub publication_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewDecision {
    pub candidate_sha: String,
    pub approve: bool,
    pub reason: String,
    #[serde(default)]
    pub checks_run: Vec<CheckEvidence>,
    #[serde(default)]
    pub skipped_checks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Publication {
    pub id: String,
    pub candidate_id: String,
    pub project_id: String,
    pub task_id: String,
    pub target_branch: String,
    pub expected_head: Option<String>,
    pub candidate_sha: String,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub sequence: i64,
    pub at: i64,
    pub project_id: Option<String>,
    pub kind: String,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlMessage {
    pub sequence: i64,
    pub id: String,
    pub worker_id: String,
    pub attempt_id: Option<String>,
    pub generation: Option<u64>,
    pub kind: String,
    pub payload: Value,
}

/// Coordinator-side facts about one queued `ControlMessage`. Kept out of the message
/// itself: workers journal and compare the exact message form they receive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageDelivery {
    /// `ControlMessage::sequence` of the message described.
    pub sequence: i64,
    /// Coordinator clock (Unix seconds) when the message was queued; 0 when not recorded.
    pub queued_at: i64,
    /// The recipient acknowledged its journal through at least this message.
    pub acknowledged: bool,
}

/// One direct exchange between the coordinator administrator and a device's agent,
/// outside any task. The device receives it as a `chat` control message and answers
/// with `Command::ReplyChat`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: String,
    pub worker_id: String,
    pub text: String,
    /// Coordinator clock (Unix seconds).
    pub sent_at: i64,
    /// The agent's answer, or the device's reason when `failed`.
    pub reply: Option<String>,
    /// The device produced no agent answer; `reply` says why.
    pub failed: bool,
    pub replied_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    pub attempt_id: String,
    pub worker_id: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: Option<f64>,
    pub estimated: bool,
    pub at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerRuntimeStatus {
    pub worker_id: String,
    pub provider_authenticated: Option<bool>,
    pub network_ready: Option<bool>,
    pub desktop_session: Option<String>,
    pub desktop_active: bool,
    pub desktop_paused: bool,
    pub observed_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub projects: Vec<Project>,
    pub workers: Vec<WorkerRecord>,
    pub tasks: Vec<TaskRecord>,
    pub attempts: Vec<AttemptRecord>,
    pub candidates: Vec<CandidateRecord>,
    pub publications: Vec<Publication>,
    pub events: Vec<Event>,
    #[serde(default)]
    pub messages: Vec<ControlMessage>,
    #[serde(default)]
    pub usage: Vec<UsageRecord>,
    #[serde(default)]
    pub runtime_status: Vec<WorkerRuntimeStatus>,
    /// One entry per message in `messages`.
    #[serde(default)]
    pub deliveries: Vec<MessageDelivery>,
    /// Direct chat exchanges, oldest first.
    #[serde(default)]
    pub chats: Vec<ChatMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invitation {
    pub token: String,
    pub project_id: String,
    pub host_name: String,
    pub expires_at: i64,
    pub requirements: Value,
    pub revoked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Enrollment {
    pub worker: WorkerRecord,
    pub device_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerEvent {
    Started,
    Progress {
        message: String,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cost_usd: Option<f64>,
        estimated: bool,
    },
    Settled,
    WorkflowCompleted {
        success: bool,
        artifact_digest: String,
        evidence: Vec<CheckEvidence>,
    },
    CompilerTelemetry {
        records: Vec<Value>,
    },
    Failed {
        message: String,
    },
    CostCapped {
        message: String,
    },
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    CreateProject {
        name: String,
        target_branch: String,
        max_tasks: u32,
        max_depth: u32,
    },
    SetRequirements {
        project_id: String,
        requirements: Value,
    },
    CreateInvitation {
        project_id: String,
        host_name: String,
        expires_in_seconds: u64,
    },
    RevokeInvitation {
        token: String,
    },
    ApproveWorker {
        worker_id: String,
    },
    RevokeWorker {
        worker_id: String,
    },
    SetDraining {
        worker_id: String,
        draining: bool,
    },
    Heartbeat {
        registration: WorkerRegistration,
    },
    ReportRuntimeStatus {
        status: WorkerRuntimeStatus,
    },
    RequestDesktopPause {
        worker_id: String,
    },
    CreateTask {
        spec: TaskSpec,
    },
    AdmitTask {
        task_id: String,
        worker_id: Option<String>,
    },
    Delegate {
        parent_attempt_id: String,
        generation: u64,
        spec: TaskSpec,
    },
    ReportEvent {
        attempt_id: String,
        generation: u64,
        sequence: u64,
        event: WorkerEvent,
    },
    SubmitCandidate {
        submission: CandidateSubmission,
    },
    ReviewCandidate {
        candidate_id: String,
        decision: ReviewDecision,
    },
    CancelTask {
        task_id: String,
    },
    AbandonAttempt {
        attempt_id: String,
        reason: String,
    },
    ReconcileWorker {
        stopped_attempt_ids: Vec<String>,
    },
    InvalidateContract {
        project_id: String,
    },
    SendMessage {
        project_id: String,
        recipient_task_id: String,
        message_id: String,
        kind: String,
        body: Value,
    },
    AcknowledgeMessages {
        through_sequence: i64,
    },
    /// Administrator: queue a direct chat message for one device's agent.
    SendChat {
        worker_id: String,
        text: String,
    },
    /// The addressed device: its agent's answer to a chat message, or why there is none.
    ReplyChat {
        chat_id: String,
        reply: String,
        failed: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub protocol_version: u32,
    pub command_id: String,
    pub command: Command,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResponse {
    pub command_id: String,
    pub result: Value,
}
