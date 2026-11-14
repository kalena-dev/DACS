//! Enrollment, membership, revocation and compiler-endpoint operations.
//!
//! Every operation is restricted to DACS-tagged nodes and keys
//! ([`ENROLLABLE_ROLES`]); the operation token held by Person 1's coordinator
//! cannot touch infrastructure (`tag:dacs-infra`), personal nodes, users or the
//! policy. Headscale is the only state store: Person 1 keeps the returned
//! `auth_key_id`/`node_id` in its own database.

use std::net::IpAddr;

use dacs_net_contract::{
    CONTRACT_REVISION, CompilerEndpoint, CompilerRole, ContractError, CoordinatorEndpoints,
    EnrollmentGrant, EnrollmentRequest, HostName, MembershipQuery, MembershipState,
    NetworkMembership, NodeRole, OverlayNode, RevocationOutcome, RevocationRequest, Secret,
    is_overlay_ip,
};
use time::OffsetDateTime;

use crate::audit::Audit;
use crate::headscale::{HeadscaleApi, HeadscaleError, Node, PreAuthKey};

pub const ENROLLABLE_ROLES: [NodeRole; 4] = [
    NodeRole::Worker,
    NodeRole::Coordinator,
    NodeRole::CompileScheduler,
    NodeRole::CompileServer,
];

#[derive(Clone, Debug)]
pub struct CompilerPorts {
    pub scheduler: u16,
    pub server: u16,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub login_server: String,
    pub coordinator: CoordinatorEndpoints,
    pub key_ttl: time::Duration,
    pub compiler_ports: CompilerPorts,
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Upstream(#[from] HeadscaleError),
}

pub struct Service<H> {
    hs: H,
    settings: Settings,
    audit: Audit,
}

fn is_dacs_tagset(tags: &[String]) -> bool {
    !tags.is_empty()
        && tags
            .iter()
            .all(|t| NodeRole::from_tag(t).is_some_and(|r| ENROLLABLE_ROLES.contains(&r)))
}

/// Why a freshly created key must not be handed out, if it must not. The
/// grant reports the requested tags and expiry, so the key must match them.
fn unusable_key(key: &PreAuthKey, tags: &[String], expires_at: OffsetDateTime) -> Option<&'static str> {
    let mut got = key.acl_tags.clone();
    got.sort();
    got.dedup();
    let mut want = tags.to_vec();
    want.sort();
    if key.key.is_empty() {
        Some("pre-auth key returned without secret")
    } else if key.reusable || key.ephemeral {
        Some("headscale created a reusable/ephemeral key")
    } else if key.used {
        Some("headscale returned an already used key")
    } else if got != want {
        Some("headscale created a key with other tags than requested")
    } else if key
        .expiration
        .is_none_or(|e| e > expires_at + time::Duration::minutes(1))
    {
        Some("headscale created a key that outlives the requested expiry")
    } else {
        None
    }
}

fn parse_ips(node: &Node) -> Vec<IpAddr> {
    node.ip_addresses
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect()
}

fn node_name(node: &Node) -> &str {
    if node.given_name.is_empty() {
        &node.name
    } else {
        &node.given_name
    }
}

impl<H: HeadscaleApi> Service<H> {
    pub fn new(hs: H, settings: Settings, audit: Audit) -> Self {
        Self {
            hs,
            settings,
            audit,
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub async fn health(&self) -> Result<(), ServiceError> {
        Ok(self.hs.health().await?)
    }

    /// Issue a single-use, short-lived, tagged pre-auth key for one device.
    /// Requires the owner's consent and the coordinator's approval in `req`.
    pub async fn enroll(
        &self,
        req: &EnrollmentRequest,
        now: OffsetDateTime,
    ) -> Result<EnrollmentGrant, ServiceError> {
        req.validate(now).map_err(|e| match e {
            ContractError::Forbidden(_) => ServiceError::Forbidden(e.to_string()),
            _ => ServiceError::BadRequest(e.to_string()),
        })?;
        let tags = req.tags();

        let nodes = self.hs.list_nodes().await?;
        if let Some(existing) = nodes
            .iter()
            .find(|n| node_name(n) == req.host_name.as_str())
        {
            return Err(ServiceError::Conflict(format!(
                "host name {} is already registered as node {}; revoke it before re-enrolling",
                req.host_name, existing.id
            )));
        }

        let expires_at = now + self.settings.key_ttl;
        let key = self.hs.create_tagged_key(&tags, expires_at).await?;
        if let Some(problem) = unusable_key(&key, &tags, expires_at) {
            // Never hand out a key that could enroll more than one device, carry
            // other tags or outlive the grant. Expire it and audit the refusal so
            // a failed cleanup can still be reconciled by key id.
            let expired = self.hs.expire_key(key.id).await;
            if let Err(e) = &expired {
                tracing::error!(auth_key_id = key.id, error = %e, "could not expire refused pre-auth key");
            }
            self.audit.record(
                "enroll_refused",
                serde_json::json!({
                    "device_id": req.device_id,
                    "host_name": req.host_name,
                    "auth_key_id": key.id,
                    "problem": problem,
                    "key_expired": expired.is_ok(),
                }),
            );
            return Err(ServiceError::Upstream(HeadscaleError::Decode(
                problem.into(),
            )));
        }

        self.audit.record(
            "enroll",
            serde_json::json!({
                "device_id": req.device_id,
                "host_name": req.host_name,
                "host_kind": req.host_kind,
                "tags": tags,
                "auth_key_id": key.id,
                "expires_at": expires_at.unix_timestamp(),
                "consent_by": req.consent.accepted_by,
                "tailnet_switch_consented": req.consent.tailnet_switch.is_some(),
                "approved_by": req.approval.approved_by,
                "authorization_ref": req.approval.authorization_ref,
            }),
        );

        Ok(EnrollmentGrant {
            contract_revision: CONTRACT_REVISION.to_owned(),
            device_id: req.device_id.clone(),
            host_name: req.host_name.clone(),
            login_server: self.settings.login_server.clone(),
            auth_key: Secret::new(key.key),
            auth_key_id: key.id,
            tags,
            expires_at,
            coordinator: self.settings.coordinator.clone(),
        })
    }

    /// Resolve the device's node. When both ids are given they must identify
    /// the same node; a stale or mistyped pair is a conflict, never a guess.
    fn find_node(
        nodes: &[Node],
        node_id: Option<u64>,
        key_id: Option<u64>,
    ) -> Result<Option<Node>, ServiceError> {
        let by_id = node_id.and_then(|id| nodes.iter().find(|n| n.id == id));
        let by_key = key_id.and_then(|k| {
            nodes
                .iter()
                .find(|n| n.pre_auth_key.as_ref().is_some_and(|p| p.id == k))
        });
        match (node_id, key_id, by_id, by_key) {
            (Some(_), Some(_), None, None) => Ok(None),
            (Some(_), Some(_), Some(a), Some(b)) if a.id == b.id => Ok(Some(a.clone())),
            (Some(id), Some(k), ..) => Err(ServiceError::Conflict(format!(
                "node_id {id} and auth_key_id {k} do not identify the same node"
            ))),
            _ => Ok(by_id.or(by_key).cloned()),
        }
    }

    async fn find_key(&self, key_id: u64) -> Result<Option<PreAuthKey>, ServiceError> {
        Ok(self
            .hs
            .list_keys()
            .await?
            .into_iter()
            .find(|k| k.id == key_id))
    }

    /// Current network membership for a device. Renames the node to the
    /// approved host name so its overlay DNS name is stable.
    pub async fn membership(
        &self,
        q: &MembershipQuery,
        now: OffsetDateTime,
    ) -> Result<NetworkMembership, ServiceError> {
        let nodes = self.hs.list_nodes().await?;
        let node = Self::find_node(&nodes, q.node_id, q.auth_key_id)?;

        if let Some(mut node) = node {
            if !is_dacs_tagset(&node.tags) {
                return Err(ServiceError::Forbidden(format!(
                    "node {} is not a DACS-enrolled node",
                    node.id
                )));
            }
            if node_name(&node) != q.host_name.as_str() {
                match self.hs.rename_node(node.id, q.host_name.as_str()).await {
                    Ok(renamed) => node = renamed,
                    Err(e) => {
                        tracing::warn!(node = node.id, error = %e, "could not rename node to approved host name")
                    }
                }
            }
            return Ok(NetworkMembership {
                device_id: q.device_id.clone(),
                host_name: q.host_name.clone(),
                state: MembershipState::Joined {
                    node_id: node.id,
                    online: node.online,
                },
                overlay_ips: parse_ips(&node),
                tags: node.tags.clone(),
                last_seen: node.last_seen,
            });
        }

        let state = match (q.auth_key_id, q.node_id) {
            (Some(key_id), node_id) => match self.find_key(key_id).await? {
                Some(k) if !is_dacs_tagset(&k.acl_tags) => {
                    return Err(ServiceError::Forbidden(format!(
                        "key {key_id} is not a DACS enrollment key"
                    )));
                }
                Some(k) if k.used => MembershipState::Revoked { node_id },
                Some(k) => match k.expiration {
                    Some(exp) if exp > now => MembershipState::KeyIssued {
                        auth_key_id: key_id,
                        expires_at: exp,
                    },
                    _ => MembershipState::KeyExpired {
                        auth_key_id: key_id,
                    },
                },
                None => MembershipState::Revoked { node_id },
            },
            (None, Some(node_id)) => MembershipState::Revoked {
                node_id: Some(node_id),
            },
            (None, None) => MembershipState::NotEnrolled,
        };
        Ok(NetworkMembership {
            device_id: q.device_id.clone(),
            host_name: q.host_name.clone(),
            state,
            overlay_ips: Vec::new(),
            tags: Vec::new(),
            last_seen: None,
        })
    }

    /// Expire the device's unused key and delete its node, then verify absence.
    /// A deleted node cannot reconnect: its key is single-use and already
    /// consumed, so re-joining needs a new approved grant.
    pub async fn revoke(
        &self,
        req: &RevocationRequest,
        now: OffsetDateTime,
    ) -> Result<RevocationOutcome, ServiceError> {
        if req.auth_key_id.is_none() && req.node_id.is_none() {
            return Err(ServiceError::BadRequest(
                "auth_key_id or node_id is required".into(),
            ));
        }
        if req.requested_by.trim().is_empty() {
            return Err(ServiceError::BadRequest("requested_by is required".into()));
        }

        // Resolve and check everything before mutating anything, so a
        // mismatched or foreign target changes nothing.
        let nodes = self.hs.list_nodes().await?;
        let target = Self::find_node(&nodes, req.node_id, req.auth_key_id)?;
        if let Some(node) = &target
            && !is_dacs_tagset(&node.tags)
        {
            return Err(ServiceError::Forbidden(format!(
                "node {} is not a DACS-enrolled node",
                node.id
            )));
        }
        let key = match req.auth_key_id {
            Some(key_id) => self.find_key(key_id).await?,
            None => None,
        };
        if let Some(k) = &key
            && !is_dacs_tagset(&k.acl_tags)
        {
            return Err(ServiceError::Forbidden(format!(
                "key {} is not a DACS enrollment key",
                k.id
            )));
        }

        let mut key_expired = false;
        if let Some(k) = &key
            && !k.used
            && k.expiration.is_some_and(|e| e > now)
        {
            self.hs.expire_key(k.id).await?;
            key_expired = true;
        }

        let mut node_deleted = None;
        if let Some(node) = target {
            match self.hs.delete_node(node.id).await {
                Ok(()) => node_deleted = Some(node.id),
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(e.into()),
            }
        }

        let after = self.hs.list_nodes().await?;
        let verified_absent = matches!(
            Self::find_node(&after, req.node_id.or(node_deleted), req.auth_key_id),
            Ok(None)
        );

        self.audit.record(
            "revoke",
            serde_json::json!({
                "device_id": req.device_id,
                "auth_key_id": req.auth_key_id,
                "node_id": req.node_id,
                "requested_by": req.requested_by,
                "reason": req.reason,
                "key_expired": key_expired,
                "node_deleted": node_deleted,
                "verified_absent": verified_absent,
            }),
        );

        Ok(RevocationOutcome {
            device_id: req.device_id.clone(),
            key_expired,
            node_deleted,
            verified_absent,
            at: now,
        })
    }

    /// DACS-tagged overlay nodes, for Person 1's status UI.
    pub async fn nodes(&self) -> Result<Vec<OverlayNode>, ServiceError> {
        let mut out: Vec<OverlayNode> = self
            .hs
            .list_nodes()
            .await?
            .into_iter()
            .filter(|n| is_dacs_tagset(&n.tags))
            .map(|n| OverlayNode {
                node_id: n.id,
                name: node_name(&n).to_owned(),
                overlay_ips: parse_ips(&n),
                tags: n.tags.clone(),
                online: n.online,
                last_seen: n.last_seen,
            })
            .collect();
        out.sort_by_key(|n| n.node_id);
        Ok(out)
    }

    /// Stable overlay addresses for Person 4's sccache-dist registration.
    /// Only overlay IPv4 addresses are published; compiler endpoints are never
    /// given a public address.
    pub async fn compiler_endpoints(&self) -> Result<Vec<CompilerEndpoint>, ServiceError> {
        let mut out = Vec::new();
        for node in self.hs.list_nodes().await? {
            if !is_dacs_tagset(&node.tags) {
                continue;
            }
            let Ok(host_name) = HostName::parse(node_name(&node)) else {
                continue;
            };
            let Some(ip) = parse_ips(&node)
                .into_iter()
                .find(|ip| ip.is_ipv4() && is_overlay_ip(*ip))
            else {
                continue;
            };
            for (role, tag, port) in [
                (
                    CompilerRole::Scheduler,
                    NodeRole::CompileScheduler.tag(),
                    self.settings.compiler_ports.scheduler,
                ),
                (
                    CompilerRole::Server,
                    NodeRole::CompileServer.tag(),
                    self.settings.compiler_ports.server,
                ),
            ] {
                if node.tags.iter().any(|t| t == tag) {
                    out.push(CompilerEndpoint {
                        host_name: host_name.clone(),
                        role,
                        overlay_ip: ip,
                        port,
                    });
                }
            }
        }
        out.sort_by(|a, b| {
            (a.role as u8, a.host_name.as_str()).cmp(&(b.role as u8, b.host_name.as_str()))
        });
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use dacs_net_contract::{
        ADMIN_ACCESS_DISCLOSURE_REVISION, DeviceId, HostApproval, HostConsent, HostKind,
    };
    use time::macros::datetime;

    #[derive(Default)]
    struct Fake {
        keys: Mutex<Vec<PreAuthKey>>,
        nodes: Mutex<Vec<Node>>,
        next: Mutex<u64>,
    }

    impl Fake {
        /// Simulate a device using key `key_id` to register.
        fn register(&self, key_id: u64, name: &str, ip: &str) -> u64 {
            let mut keys = self.keys.lock().unwrap();
            let k = keys.iter_mut().find(|k| k.id == key_id).unwrap();
            assert!(!k.used, "single-use key reused");
            k.used = true;
            let mut next = self.next.lock().unwrap();
            *next += 1;
            let node = Node {
                id: 100 + *next,
                name: name.into(),
                given_name: name.into(),
                ip_addresses: vec![ip.into()],
                online: true,
                pre_auth_key: Some(k.clone()),
                tags: k.acl_tags.clone(),
                ..Default::default()
            };
            let id = node.id;
            self.nodes.lock().unwrap().push(node);
            id
        }
    }

    impl HeadscaleApi for Fake {
        async fn health(&self) -> Result<(), HeadscaleError> {
            Ok(())
        }
        async fn create_tagged_key(
            &self,
            tags: &[String],
            expiration: OffsetDateTime,
        ) -> Result<PreAuthKey, HeadscaleError> {
            let mut next = self.next.lock().unwrap();
            *next += 1;
            let k = PreAuthKey {
                id: *next,
                key: format!("hskey-auth-{next}"),
                expiration: Some(expiration),
                acl_tags: tags.to_vec(),
                ..Default::default()
            };
            self.keys.lock().unwrap().push(k.clone());
            Ok(k)
        }
        async fn expire_key(&self, id: u64) -> Result<(), HeadscaleError> {
            let mut keys = self.keys.lock().unwrap();
            let k = keys.iter_mut().find(|k| k.id == id).unwrap();
            k.expiration = Some(datetime!(2000-01-01 0:00 UTC));
            Ok(())
        }
        async fn list_keys(&self) -> Result<Vec<PreAuthKey>, HeadscaleError> {
            Ok(self.keys.lock().unwrap().clone())
        }
        async fn list_nodes(&self) -> Result<Vec<Node>, HeadscaleError> {
            Ok(self.nodes.lock().unwrap().clone())
        }
        async fn delete_node(&self, id: u64) -> Result<(), HeadscaleError> {
            let mut nodes = self.nodes.lock().unwrap();
            let before = nodes.len();
            nodes.retain(|n| n.id != id);
            if nodes.len() == before {
                return Err(HeadscaleError::Status {
                    status: 404,
                    body: "not found".into(),
                });
            }
            Ok(())
        }
        async fn rename_node(&self, id: u64, name: &str) -> Result<Node, HeadscaleError> {
            let mut nodes = self.nodes.lock().unwrap();
            let n = nodes.iter_mut().find(|n| n.id == id).unwrap();
            n.given_name = name.into();
            Ok(n.clone())
        }
    }

    fn service() -> Service<Fake> {
        Service::new(
            Fake::default(),
            Settings {
                login_server: "https://hs.example.org".into(),
                coordinator: CoordinatorEndpoints {
                    bootstrap_url: "https://bootstrap.example.org".into(),
                    overlay_url: "https://coordinator.dacs.internal:7443".into(),
                },
                key_ttl: time::Duration::minutes(15),
                compiler_ports: CompilerPorts {
                    scheduler: 10600,
                    server: 10501,
                },
            },
            Audit::disabled(),
        )
    }

    fn request(host: &str, roles: Vec<NodeRole>) -> EnrollmentRequest {
        let device = DeviceId(format!("dev-{host}"));
        let host = HostName::parse(host).unwrap();
        EnrollmentRequest {
            device_id: device.clone(),
            host_name: host.clone(),
            host_kind: HostKind::Physical,
            roles,
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
                authorization_ref: "auth-1".into(),
            },
        }
    }

    const NOW: OffsetDateTime = datetime!(2026-11-14 10:02 UTC);

    #[tokio::test]
    async fn full_lifecycle_issue_join_revoke() {
        let svc = service();
        let req = request("alice-pc", vec![NodeRole::Worker, NodeRole::CompileServer]);
        let grant = svc.enroll(&req, NOW).await.unwrap();
        assert_eq!(
            grant.tags,
            vec!["tag:dacs-worker", "tag:dacs-sccache-server"]
        );
        assert_eq!(grant.expires_at, NOW + time::Duration::minutes(15));

        let q = MembershipQuery {
            device_id: req.device_id.clone(),
            host_name: req.host_name.clone(),
            auth_key_id: Some(grant.auth_key_id),
            node_id: None,
        };
        assert!(matches!(
            svc.membership(&q, NOW).await.unwrap().state,
            MembershipState::KeyIssued { .. }
        ));

        // Device registers with a different OS hostname; adapter renames it.
        let node_id = svc
            .hs
            .register(grant.auth_key_id, "DESKTOP-1234", "100.64.0.5");
        let m = svc.membership(&q, NOW).await.unwrap();
        assert_eq!(
            m.state,
            MembershipState::Joined {
                node_id,
                online: true
            }
        );
        assert_eq!(m.overlay_ips, vec!["100.64.0.5".parse::<IpAddr>().unwrap()]);
        assert_eq!(svc.nodes().await.unwrap()[0].name, "alice-pc");

        let eps = svc.compiler_endpoints().await.unwrap();
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0].port, 10501);

        let out = svc
            .revoke(
                &RevocationRequest {
                    device_id: req.device_id.clone(),
                    auth_key_id: Some(grant.auth_key_id),
                    node_id: Some(node_id),
                    requested_by: "owner".into(),
                    reason: "test".into(),
                },
                NOW,
            )
            .await
            .unwrap();
        assert_eq!(out.node_deleted, Some(node_id));
        assert!(!out.key_expired, "used key needs no expiry");
        assert!(out.verified_absent);
        assert_eq!(
            svc.membership(&q, NOW).await.unwrap().state,
            MembershipState::Revoked { node_id: None }
        );
    }

    #[tokio::test]
    async fn revoking_unused_key_expires_it() {
        let svc = service();
        let req = request("bob-pc", vec![NodeRole::Worker]);
        let grant = svc.enroll(&req, NOW).await.unwrap();
        let out = svc
            .revoke(
                &RevocationRequest {
                    device_id: req.device_id.clone(),
                    auth_key_id: Some(grant.auth_key_id),
                    node_id: None,
                    requested_by: "owner".into(),
                    reason: "invite withdrawn".into(),
                },
                NOW,
            )
            .await
            .unwrap();
        assert!(out.key_expired);
        assert_eq!(out.node_deleted, None);
        let q = MembershipQuery {
            device_id: req.device_id,
            host_name: req.host_name,
            auth_key_id: Some(grant.auth_key_id),
            node_id: None,
        };
        assert!(matches!(
            svc.membership(&q, NOW).await.unwrap().state,
            MembershipState::KeyExpired { .. }
        ));
    }

    #[tokio::test]
    async fn duplicate_host_name_is_refused() {
        let svc = service();
        let grant = svc
            .enroll(&request("carol-pc", vec![NodeRole::Worker]), NOW)
            .await
            .unwrap();
        svc.hs.register(grant.auth_key_id, "carol-pc", "100.64.0.9");
        let err = svc
            .enroll(&request("carol-pc", vec![NodeRole::Worker]), NOW)
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::Conflict(_)));
    }

    #[tokio::test]
    async fn cannot_touch_non_dacs_nodes() {
        let svc = service();
        svc.hs.nodes.lock().unwrap().push(Node {
            id: 1,
            given_name: "headscale-infra".into(),
            tags: vec!["tag:dacs-infra".into()],
            ..Default::default()
        });
        let err = svc
            .revoke(
                &RevocationRequest {
                    device_id: DeviceId("x".into()),
                    auth_key_id: None,
                    node_id: Some(1),
                    requested_by: "owner".into(),
                    reason: "attempt".into(),
                },
                NOW,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::Forbidden(_)));
        assert_eq!(svc.hs.nodes.lock().unwrap().len(), 1);
        assert!(svc.nodes().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn mismatched_node_and_key_changes_nothing() {
        let svc = service();
        let a = svc
            .enroll(&request("alice-pc", vec![NodeRole::Worker]), NOW)
            .await
            .unwrap();
        let node_a = svc.hs.register(a.auth_key_id, "alice-pc", "100.64.0.5");
        let b = svc
            .enroll(&request("bob-pc", vec![NodeRole::Worker]), NOW)
            .await
            .unwrap();
        // Bob's key is still unused; the request pairs it with Alice's node.
        let req = RevocationRequest {
            device_id: DeviceId("dev-bob-pc".into()),
            auth_key_id: Some(b.auth_key_id),
            node_id: Some(node_a),
            requested_by: "owner".into(),
            reason: "stale node id".into(),
        };
        assert!(matches!(
            svc.revoke(&req, NOW).await,
            Err(ServiceError::Conflict(_))
        ));
        assert_eq!(svc.hs.nodes.lock().unwrap().len(), 1, "alice's node kept");
        let keys = svc.hs.keys.lock().unwrap();
        let bob_key = keys.iter().find(|k| k.id == b.auth_key_id).unwrap();
        assert!(bob_key.expiration > Some(NOW), "bob's key not expired");
    }

    #[tokio::test]
    async fn infra_role_is_forbidden() {
        let svc = service();
        let req = request("edge", vec![NodeRole::Infra]);
        assert!(matches!(
            svc.enroll(&req, NOW).await,
            Err(ServiceError::Forbidden(_))
        ));
        assert!(svc.hs.keys.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn enrollment_without_consent_issues_nothing() {
        let svc = service();
        let mut req = request("dave-pc", vec![NodeRole::Worker]);
        req.consent.accepted_host = HostName::parse("other-pc").unwrap();
        assert!(matches!(
            svc.enroll(&req, NOW).await,
            Err(ServiceError::BadRequest(_))
        ));
        assert!(svc.hs.keys.lock().unwrap().is_empty());
    }
}
