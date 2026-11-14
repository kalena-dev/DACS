//! Opaque, string-backed identifiers. Coordinator-minted ids are `<prefix>_<uuidv7>`
//! (time ordered). Externally minted ids (OMP session ids) are accepted verbatim.
use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! id_types {
    ($($(#[$meta:meta])* $name:ident = $prefix:literal;)+) => {$(
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        #[allow(clippy::new_without_default)]
        impl $name {
            pub const PREFIX: &'static str = $prefix;
            /// Mint a new time-ordered id.
            pub fn new() -> Self { Self(format!("{}_{}", $prefix, uuid::Uuid::now_v7().simple())) }
            pub fn as_str(&self) -> &str { &self.0 }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
        }
        impl From<String> for $name { fn from(s: String) -> Self { Self(s) } }
        impl From<&str> for $name { fn from(s: &str) -> Self { Self(s.to_owned()) } }
        impl AsRef<str> for $name { fn as_ref(&self) -> &str { &self.0 } }
    )+};
}

id_types! {
    ProjectId = "prj";
    /// Task intent: what should be done. Survives any number of attempts.
    TaskId = "tsk";
    /// One execution of a task at one generation on one device.
    AttemptId = "att";
    /// OMP session id as reported by the worker's OMP adapter (externally minted).
    SessionId = "ses";
    /// An enrolled machine (physical or virtual) running a DACS worker.
    DeviceId = "dev";
    /// Physical machine that owns capacity. VMs map to their host's id.
    PhysicalHostId = "hst";
    /// Deduplication key of one envelope. Replays reuse it.
    MessageId = "msg";
    /// Ties a chain of requests, commands and events together.
    CorrelationId = "cor";
    ReservationId = "rsv";
    PlanId = "pln";
    InvitationId = "inv";
    CredentialId = "crd";
    UploadId = "upl";
    CandidateId = "cnd";
    ConstructionId = "cst";
    DecisionId = "dec";
    IntentId = "int";
    MailId = "mail";
    CommandId = "cmd";
    ResultId = "res";
    RequestId = "req";
}
