//! Durable worker execution. Owner credentials remain in the selected OMP profile.
//! Elevated tools are not sandboxed. Desktop admission uses observed native
//! capabilities and exclusive local controls, not browser automation proof.

/// Person 4 build planning/evidence adapter over Person 2's process descriptor.
pub mod build_execution;
pub mod build_runner;
pub mod capacity;
mod journal;
pub mod local_control;
mod omp;
pub mod runtime;
mod supervisor;
pub mod task_board;
mod workspace;

pub use journal::{Journal, JournalError, PendingCommand, PendingResult};
pub use omp::{
    OmpEvent, OmpRpc, OmpRpcError, OmpToolCall, check_pinned_version, installed_version,
};
pub use supervisor::{ManagedChild, ProcessIo, ProcessState, ReservationUsage, Supervisor};
