//! Worker capabilities, physical-host accounting, reservations and capacity refusals.
//!
//! Accounting rule: capacity (slots) belongs to a *physical host*. Every device —
//! a bare-metal PC or a VM on it — maps to exactly one physical host, so a VM and
//! its host draw from one pool and are never counted twice. Reservations debit
//! physical-host slots. Observed usage is telemetry only and never changes slots.
use crate::ids::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

str_enum! {
    pub enum WorkKind {
        /// An OMP agent session (lead or child).
        AgentSession = "agent_session",
        /// A build job (e.g. one `cargo build` coordinating compile units).
        BuildJob = "build_job",
        TestJob = "test_job",
        /// One eligible compiler invocation from a Cargo build.
        CompileUnit = "compile_unit",
    }
}

str_enum! {
    pub enum Os {
        Windows = "windows",
        Linux = "linux",
        Macos = "macos",
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCapability {
    /// e.g. `omp`, `rustc`, `cargo`, `git`.
    pub name: String,
    pub version: String,
}

/// What a device can run. Reported at enrollment and refreshed by heartbeat.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerCapabilities {
    pub os: Os,
    /// `x86_64`, `aarch64`, ...
    pub arch: String,
    pub work_kinds: Vec<WorkKind>,
    #[serde(default)]
    pub tools: Vec<ToolCapability>,
    /// Rust target triples this device can compile for (compile units).
    #[serde(default)]
    pub rust_targets: Vec<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

/// Physical identity of the machine a device runs on, used to fold VMs onto hosts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostDescriptor {
    /// Stable hash of this machine's identity (e.g. machine-id / IOPlatformUUID / MachineGuid).
    pub fingerprint: String,
    pub is_virtual: bool,
    /// For a VM: the fingerprint of the physical host it runs on, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_fingerprint: Option<String>,
    pub cpu_cores: u32,
    pub memory_mb: u64,
}

/// Declared capacity of one physical host, per work kind. Set by the operator/setup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCapacity {
    pub physical_host_id: PhysicalHostId,
    pub slots: BTreeMap<WorkKind, u32>,
}

/// Observed usage reported by a device. Telemetry only; never used as capacity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ObservedUsage {
    pub at_ms: i64,
    pub cpu_load_pct: f32,
    pub memory_used_mb: u64,
    #[serde(default)]
    pub running: BTreeMap<WorkKind, u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRequirement {
    pub name: String,
    /// Dotted numeric minimum, compared component-wise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_version: Option<String>,
}

fn one() -> u32 { 1 }

/// Eligibility requirement of a task/job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityRequirement {
    pub work_kind: WorkKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<Os>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default)]
    pub tools: Vec<ToolRequirement>,
    /// Required Rust target triple (compile units).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rust_target: Option<String>,
    /// Every listed label must match exactly.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Slots of `work_kind` consumed on the physical host.
    #[serde(default = "one")]
    pub units: u32,
}

str_enum! {
    pub enum ReservationState {
        /// Capacity debited, not yet bound to an attempt (e.g. child reserved at delegation).
        Held = "held",
        /// Bound to a running attempt.
        Bound = "bound",
        Released = "released",
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservationInfo {
    pub reservation_id: ReservationId,
    pub physical_host_id: PhysicalHostId,
    /// Device chosen on that host (set when held for a specific device, always when bound).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<DeviceId>,
    pub work_kind: WorkKind,
    pub units: u32,
    pub task_id: TaskId,
    pub state: ReservationState,
}

str_enum! {
    pub enum CapacityRefusalReason {
        /// No enrolled, active device satisfies the capability requirement.
        NoEligibleWorker = "no_eligible_worker",
        /// Eligible devices exist but their physical hosts have no free slots.
        NoFreeCapacity = "no_free_capacity",
        DepthLimitExceeded = "depth_limit_exceeded",
        ChildLimitExceeded = "child_limit_exceeded",
        ProjectLimitExceeded = "project_limit_exceeded",
        /// Delegating parent attempt is not the task's current running attempt.
        ParentNotRunning = "parent_not_running",
    }
}

/// Explicit capacity refusal. A refused delegation creates no task and no reservation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityRefusal {
    pub work_kind: WorkKind,
    pub units_requested: u32,
    pub reason: CapacityRefusalReason,
    /// Active devices that satisfy the requirement.
    pub eligible_devices: u32,
    /// Largest free slot count among eligible physical hosts.
    pub max_free_units: u32,
    pub detail: String,
}
