//! DACS worker-side durable receipt, result outbox and supervised OMP RPC.
//!
//! The journal intentionally stores the pinned wire payloads as JSON. This lets
//! the worker durably accept commands before executing them while shared contract
//! types are being finalized by Person 1.

/// Person 4 build planning/evidence adapter over Person 2's process descriptor.
pub mod build_execution;
mod journal;
mod omp;
mod supervisor;
pub mod task_board;

pub use journal::{Journal, JournalError, PendingCommand, PendingResult};
pub use omp::{OmpEvent, OmpRpc, OmpRpcError, OmpToolCall};
pub use supervisor::{ManagedChild, ProcessState, Supervisor};
