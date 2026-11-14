//! Durable workflow results; artifact paths are worker-local until uploaded by the runtime.
use crate::CheckEvidence;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowArtifact {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowResult {
    pub task_id: String,
    pub attempt_id: String,
    pub generation: u64,
    pub source_commit: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub elapsed_ms: u64,
    pub checks: Vec<CheckEvidence>,
    pub artifacts: Vec<WorkflowArtifact>,
    pub distribution_failure: Option<String>,
    pub processes_reconciled: bool,
}

/// Service attempts already admitted on each physical compiler host. Their normal
/// attempt reservations account for resources; the pool adds exclusive ownership,
/// not a second resource charge. Membership is immutable until every service stops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolClaim {
    pub pool_id: String,
    pub service_attempt_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolOwnership {
    pub pool_id: String,
    pub project_id: String,
    pub holder_attempt_id: String,
    pub service_attempt_ids: Vec<String>,
    pub physical_hosts: Vec<String>,
    pub released: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompilerTelemetryRecord {
    pub worker_id: String,
    pub service_attempt_id: String,
    pub record: serde_json::Value,
}
