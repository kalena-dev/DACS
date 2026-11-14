//! DACS network enrollment contract.
//!
//! Ownership boundary (see `00-general-plan.md`, `03-network-enrollment.md`):
//! - Person 1 (coordinator) decides project/device authorization, stores the
//!   network references returned here, and renders the states in its UI.
//! - Person 3 (this contract) performs network operations only after that
//!   authorization *and* the owner's consent, and owns network credentials.
//! - Person 2 (worker) receives the device-specific [`EnrollmentGrant`] and
//!   joins with it; it never receives Headscale administration credentials.
//! - Person 4 (builds) receives [`CompilerEndpoint`]s and path/reachability
//!   evidence.
//!
//! Installation, project authorization and network membership are separate
//! states. This crate only models network membership ([`MembershipState`]);
//! the other two live in Person 1's coordinator.

use std::fmt;
use std::net::{IpAddr, SocketAddr};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

mod placement;
pub use placement::{check_guest_node, published_compiler_server};

/// Revision of this wire contract. Bump on any incompatible change.
pub const CONTRACT_REVISION: &str = "dacs-net/2026-11-14.1";

/// Revision of the administrator-level access disclosure the owner must accept.
/// A consent recorded against another revision is rejected.
pub const ADMIN_ACCESS_DISCLOSURE_REVISION: &str = "host-admin-access/1";

/// Text shown by Person 1's setup UI before the owner accepts a named host.
pub const ADMIN_ACCESS_DISCLOSURE: &str = "Accepting this host lets the DACS project run OMP agents and build jobs \
on it under your account with elevation available. Elevated mode is not a sandbox: agents can change files, \
install software and control the desktop. Joining also connects this host to the project's private network \
(Headscale), so other approved project hosts can reach the services DACS opens on it.";

/// Text shown when the host is already logged in to another Tailscale/Headscale
/// control server. Joining DACS displaces that connection.
pub const TAILNET_SWITCH_DISCLOSURE: &str = "This host is already connected to another Tailscale network. The \
Tailscale client runs one network at a time: joining the DACS network disconnects this host from the current one \
until you switch back (`tailscale switch`). Devices and services you reach through the current network will be \
unreachable while DACS is active. DACS never switches networks without this approval.";

/// Tailscale's default control server, used when a client reports no override.
pub const TAILSCALE_DEFAULT_CONTROL_URL: &str = "https://controlplane.tailscale.com";

/// A secret value (auth key). Serialized as a plain string for delivery to the
/// one device it belongs to; never printed by `Debug`/`Display`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    /// Reveal the secret. Callers must not log the result.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

/// Device identity assigned by Person 1's coordinator.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceId(pub String);

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A host name as it appears on the overlay (single DNS label).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HostName(String);

impl HostName {
    pub fn parse(value: &str) -> Result<Self, ContractError> {
        let ok = !value.is_empty()
            && value.len() <= 63
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !value.starts_with('-')
            && !value.ends_with('-');
        if ok {
            Ok(Self(value.to_owned()))
        } else {
            Err(ContractError::InvalidHostName(value.to_owned()))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for HostName {
    type Error = ContractError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<HostName> for String {
    fn from(value: HostName) -> Self {
        value.0
    }
}

impl fmt::Display for HostName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Network roles. Each maps to one Headscale ACL tag; the policy file
/// (`net/deploy/headscale/policy.hujson`) grants traffic by tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeRole {
    Worker,
    Coordinator,
    CompileScheduler,
    CompileServer,
    /// Public bootstrap relay / infrastructure. Enrolled by the infrastructure
    /// administrator only; the enrollment API refuses it.
    Infra,
}

impl NodeRole {
    pub const fn tag(self) -> &'static str {
        match self {
            NodeRole::Worker => "tag:dacs-worker",
            NodeRole::Coordinator => "tag:dacs-coordinator",
            NodeRole::CompileScheduler => "tag:dacs-sccache-scheduler",
            NodeRole::CompileServer => "tag:dacs-sccache-server",
            NodeRole::Infra => "tag:dacs-infra",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        [
            NodeRole::Worker,
            NodeRole::Coordinator,
            NodeRole::CompileScheduler,
            NodeRole::CompileServer,
            NodeRole::Infra,
        ]
        .into_iter()
        .find(|r| r.tag() == tag)
    }
}

/// Whether the enrolling node is a physical PC or a guest (VM/compile
/// environment). A guest joins as its own overlay node and is accounted
/// against its physical host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostKind {
    Physical,
    Guest { physical_host: DeviceId },
}

/// The owner's explicit acceptance of a named host and the administrator-level
/// access disclosure. Recorded locally on the host by Person 1's setup UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostConsent {
    pub device_id: DeviceId,
    /// The host name the owner saw and accepted.
    pub accepted_host: HostName,
    /// Local owner identity (OS account) that accepted.
    pub accepted_by: String,
    #[serde(with = "time::serde::rfc3339")]
    pub accepted_at: OffsetDateTime,
    /// Must equal [`ADMIN_ACCESS_DISCLOSURE_REVISION`].
    pub disclosure_revision: String,
    /// Present only when the host was logged in to another control server.
    pub tailnet_switch: Option<TailnetSwitchConsent>,
}

/// Explicit approval to displace an existing Tailscale/Headscale login.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TailnetSwitchConsent {
    /// Control server the host was using when the owner approved.
    pub previous_control_url: String,
    pub previous_tailnet: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub accepted_at: OffsetDateTime,
}

/// Person 1's individual host approval for this project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostApproval {
    pub device_id: DeviceId,
    pub approved_by: String,
    #[serde(with = "time::serde::rfc3339")]
    pub approved_at: OffsetDateTime,
    /// Opaque reference to the coordinator's authorization record.
    pub authorization_ref: String,
}

/// Coordinator -> netadmin: issue device-specific enrollment material.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollmentRequest {
    pub device_id: DeviceId,
    pub host_name: HostName,
    pub host_kind: HostKind,
    pub roles: Vec<NodeRole>,
    pub consent: HostConsent,
    pub approval: HostApproval,
}

impl EnrollmentRequest {
    /// Check the request is internally consistent and carries both the owner's
    /// consent and the coordinator's individual approval.
    pub fn validate(&self, now: OffsetDateTime) -> Result<(), ContractError> {
        let skew = time::Duration::minutes(5);
        if self.consent.device_id != self.device_id || self.approval.device_id != self.device_id {
            return Err(ContractError::Mismatch(
                "device_id differs between request, consent and approval",
            ));
        }
        if self.consent.accepted_host != self.host_name {
            return Err(ContractError::Mismatch(
                "owner accepted a different host name",
            ));
        }
        if self.consent.disclosure_revision != ADMIN_ACCESS_DISCLOSURE_REVISION {
            return Err(ContractError::StaleDisclosure(
                self.consent.disclosure_revision.clone(),
            ));
        }
        if self.consent.accepted_by.trim().is_empty() || self.approval.approved_by.trim().is_empty()
        {
            return Err(ContractError::Missing(
                "consent.accepted_by / approval.approved_by",
            ));
        }
        if self.approval.authorization_ref.trim().is_empty() {
            return Err(ContractError::Missing("approval.authorization_ref"));
        }
        if self.consent.accepted_at > now + skew || self.approval.approved_at > now + skew {
            return Err(ContractError::Mismatch(
                "consent or approval timestamp is in the future",
            ));
        }
        if self.roles.is_empty() {
            return Err(ContractError::Missing("roles"));
        }
        if self.roles.contains(&NodeRole::Infra) {
            return Err(ContractError::Forbidden(
                "infra nodes are enrolled by the infrastructure administrator",
            ));
        }
        if let HostKind::Guest { physical_host } = &self.host_kind
            && (physical_host.0.trim().is_empty() || *physical_host == self.device_id)
        {
            return Err(ContractError::Mismatch(
                "guest must name a different physical host",
            ));
        }
        Ok(())
    }

    /// Sorted, deduplicated ACL tags for the requested roles.
    pub fn tags(&self) -> Vec<String> {
        let mut roles = self.roles.clone();
        roles.sort();
        roles.dedup();
        roles.into_iter().map(|r| r.tag().to_owned()).collect()
    }
}

/// Where workers reach the coordinator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorEndpoints {
    /// Public HTTPS bootstrap, reachable before the device joins the overlay.
    pub bootstrap_url: String,
    /// Coordinator control API over the overlay (application TLS still required).
    pub overlay_url: String,
}

/// Netadmin -> coordinator -> one device. Single-use, short-lived, tagged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollmentGrant {
    pub contract_revision: String,
    pub device_id: DeviceId,
    pub host_name: HostName,
    /// Headscale `server_url` the device logs in to.
    pub login_server: String,
    pub auth_key: Secret,
    /// Headscale pre-auth key id. Person 1 stores it for status and revocation.
    pub auth_key_id: u64,
    pub tags: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    pub coordinator: CoordinatorEndpoints,
}

/// Coordinator -> netadmin: what is this device's network membership now?
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MembershipQuery {
    pub device_id: DeviceId,
    pub host_name: HostName,
    pub auth_key_id: Option<u64>,
    pub node_id: Option<u64>,
}

/// Network membership state, separate from installation and project
/// authorization (both owned by Person 1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MembershipState {
    /// No enrollment material issued.
    NotEnrolled,
    /// Key issued, not yet used.
    KeyIssued {
        auth_key_id: u64,
        #[serde(with = "time::serde::rfc3339")]
        expires_at: OffsetDateTime,
    },
    /// Key expired before any node used it. A new grant is required.
    KeyExpired { auth_key_id: u64 },
    /// Node registered with the device's key.
    Joined { node_id: u64, online: bool },
    /// Node removed by revocation (or absent after having joined).
    Revoked { node_id: Option<u64> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkMembership {
    pub device_id: DeviceId,
    pub host_name: HostName,
    pub state: MembershipState,
    pub overlay_ips: Vec<IpAddr>,
    pub tags: Vec<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_seen: Option<OffsetDateTime>,
}

/// Coordinator -> netadmin: remove a device from the network.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevocationRequest {
    pub device_id: DeviceId,
    pub auth_key_id: Option<u64>,
    pub node_id: Option<u64>,
    pub requested_by: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevocationOutcome {
    pub device_id: DeviceId,
    /// The pre-auth key was unused and is now expired.
    pub key_expired: bool,
    /// A node was found and deleted from Headscale.
    pub node_deleted: Option<u64>,
    /// After the operation, no node for this device remains registered.
    pub verified_absent: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

/// Local Tailscale client state on a host, as reported to Person 1's setup UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TailnetClientState {
    /// No `tailscale` binary found. Network-client prerequisite missing.
    NotInstalled,
    /// Binary present but the daemon/service is not reachable.
    DaemonUnavailable { detail: String },
    /// No profile logged in; joining displaces nothing.
    LoggedOut,
    /// A profile exists for `control_url` (running, stopped or needing reauth).
    LoggedIn {
        control_url: String,
        backend_state: String,
        tailnet: Option<String>,
        self_name: Option<String>,
        overlay_ips: Vec<IpAddr>,
    },
}

/// A DACS-tagged node registered with Headscale, for Person 1's status UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayNode {
    pub node_id: u64,
    pub name: String,
    pub overlay_ips: Vec<IpAddr>,
    pub tags: Vec<String>,
    pub online: bool,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_seen: Option<OffsetDateTime>,
}

/// Result of checking a host before joining the DACS login server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TailnetAssessment {
    pub login_server: String,
    pub client: TailnetClientState,
    /// The host is already on the DACS network.
    pub already_dacs: bool,
    /// Joining would displace another control server's login; the owner must
    /// approve [`TAILNET_SWITCH_DISCLOSURE`] and record a
    /// [`TailnetSwitchConsent`] naming `previous_control_url`.
    pub requires_switch_consent: bool,
    pub previous_control_url: Option<String>,
    pub disclosure: Option<String>,
}

/// Device-side result handed to Person 2's worker after a successful join.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalJoinResult {
    pub device_id: DeviceId,
    pub host_name: HostName,
    pub overlay_ips: Vec<IpAddr>,
    pub coordinator: CoordinatorEndpoints,
    /// Control server displaced by this join, if any (owner approved it).
    pub displaced_control_url: Option<String>,
}

/// How traffic to a peer travels.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "path", rename_all = "snake_case")]
pub enum PeerPath {
    Direct { endpoint: String },
    Derp { region: String },
    PeerRelay { relay: String },
    Unreachable { detail: String },
}

/// Active probe result (`tailscale ping`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PathProbe {
    pub peer: String,
    pub path: PeerPath,
    pub rtt_ms: Option<f64>,
    #[serde(with = "time::serde::rfc3339")]
    pub measured_at: OffsetDateTime,
}

/// Passive per-peer snapshot (`tailscale status --json`). Diff two snapshots
/// around a build to report which path it used and bytes transferred.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerSnapshot {
    pub host_name: String,
    pub overlay_ips: Vec<IpAddr>,
    pub online: bool,
    pub path: PeerPath,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// TCP reachability probe for a private service (compiler, coordinator).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TcpProbe {
    pub target: SocketAddr,
    /// Target address is inside the overlay prefixes.
    pub overlay_private: bool,
    pub reachable: bool,
    pub connect_ms: Option<f64>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompilerRole {
    /// sccache-dist scheduler.
    Scheduler,
    /// sccache-dist build server.
    Server,
}

/// Stable compiler-node address for Person 4. Addresses are overlay-only; the
/// netadmin refuses to publish a non-overlay address.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompilerEndpoint {
    pub host_name: HostName,
    pub role: CompilerRole,
    pub overlay_ip: IpAddr,
    pub port: u16,
}

impl CompilerEndpoint {
    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.overlay_ip, self.port)
    }
}

/// Error body returned by the netadmin HTTP API.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

/// True when `ip` is inside Headscale's default overlay prefixes
/// (100.64.0.0/10, fd7a:115c:a1e0::/48), which `net/deploy` configures.
pub fn is_overlay_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (o[1] & 0b1100_0000) == 64
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ContractError {
    #[error("invalid host name {0:?}: use 1-63 lowercase letters, digits or '-'")]
    InvalidHostName(String),
    #[error("inconsistent request: {0}")]
    Mismatch(&'static str),
    #[error("missing field: {0}")]
    Missing(&'static str),
    #[error("forbidden: {0}")]
    Forbidden(&'static str),
    #[error(
        "consent was recorded against disclosure {0:?}; the owner must accept the current disclosure"
    )]
    StaleDisclosure(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn request() -> EnrollmentRequest {
        let device = DeviceId("dev-1".into());
        let host = HostName::parse("alice-pc").unwrap();
        EnrollmentRequest {
            device_id: device.clone(),
            host_name: host.clone(),
            host_kind: HostKind::Physical,
            roles: vec![NodeRole::Worker, NodeRole::CompileServer, NodeRole::Worker],
            consent: HostConsent {
                device_id: device.clone(),
                accepted_host: host,
                accepted_by: "alice".into(),
                accepted_at: datetime!(2026-11-14 10:00 UTC),
                disclosure_revision: ADMIN_ACCESS_DISCLOSURE_REVISION.into(),
                tailnet_switch: None,
            },
            approval: HostApproval {
                device_id: device,
                approved_by: "owner".into(),
                approved_at: datetime!(2026-11-14 10:01 UTC),
                authorization_ref: "auth-42".into(),
            },
        }
    }

    #[test]
    fn enrollment_requires_matching_consent_and_approval() {
        let now = datetime!(2026-11-14 10:05 UTC);
        assert_eq!(request().validate(now), Ok(()));

        let mut r = request();
        r.consent.accepted_host = HostName::parse("bob-pc").unwrap();
        assert!(matches!(r.validate(now), Err(ContractError::Mismatch(_))));

        let mut r = request();
        r.approval.device_id = DeviceId("dev-2".into());
        assert!(matches!(r.validate(now), Err(ContractError::Mismatch(_))));

        let mut r = request();
        r.consent.disclosure_revision = "host-admin-access/0".into();
        assert!(matches!(
            r.validate(now),
            Err(ContractError::StaleDisclosure(_))
        ));

        let mut r = request();
        r.roles.push(NodeRole::Infra);
        assert!(matches!(r.validate(now), Err(ContractError::Forbidden(_))));

        let mut r = request();
        r.host_kind = HostKind::Guest {
            physical_host: DeviceId("dev-1".into()),
        };
        assert!(matches!(r.validate(now), Err(ContractError::Mismatch(_))));
    }

    #[test]
    fn tags_are_sorted_and_unique() {
        assert_eq!(
            request().tags(),
            vec!["tag:dacs-worker", "tag:dacs-sccache-server"]
        );
    }

    #[test]
    fn overlay_prefixes() {
        assert!(is_overlay_ip("100.64.0.1".parse().unwrap()));
        assert!(is_overlay_ip("100.127.255.254".parse().unwrap()));
        assert!(!is_overlay_ip("100.128.0.1".parse().unwrap()));
        assert!(!is_overlay_ip("192.168.1.10".parse().unwrap()));
        assert!(is_overlay_ip("fd7a:115c:a1e0::1".parse().unwrap()));
        assert!(!is_overlay_ip("fd7a:115c:a1e1::1".parse().unwrap()));
    }

    #[test]
    fn secret_is_redacted_but_serialized() {
        let s = Secret::new("hskey-auth-abc");
        assert_eq!(format!("{s:?}"), "Secret([redacted])");
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"hskey-auth-abc\"");
    }

    #[test]
    fn deploy_example_request_is_valid() {
        let text = include_str!("../../../deploy/netadmin/enrollment-request.example.json");
        let req: EnrollmentRequest = serde_json::from_str(text).unwrap();
        assert_eq!(req.validate(datetime!(2026-11-14 10:05 UTC)), Ok(()));
        assert_eq!(req.tags(), vec!["tag:dacs-worker", "tag:dacs-coordinator"]);
    }

    #[test]
    fn host_name_validation() {
        assert!(HostName::parse("ok-1").is_ok());
        for bad in ["", "-a", "a-", "Upper", "a.b", &"x".repeat(64)] {
            assert!(HostName::parse(bad).is_err(), "{bad}");
        }
    }
}
