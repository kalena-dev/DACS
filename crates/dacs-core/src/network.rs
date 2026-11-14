//! Durable coordinator authorization and network-operation journal.
//! External calls belong in the application; no database guard crosses transport.
use super::*;
use dacs_net_contract::{
    CONTRACT_REVISION, DeviceId, EnrollmentGrant, EnrollmentRequest, HostApproval, HostConsent,
    HostKind, MembershipQuery, MembershipState, NetworkMembership, NodeRole, RevocationOutcome,
    RevocationRequest,
};
use serde::Deserialize;
use time::OffsetDateTime;

const KIND: &str = "network_device";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPhase {
    AwaitingApproval,
    IssueUncertain,
    GrantIssued,
    ReferencesRecovered,
    RevocationPending,
    Revoked,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct NetworkRecord {
    pub device_id: String,
    pub consent: Option<HostConsent>,
    pub phase: NetworkPhase,
    pub operation_id: Option<String>,
    pub auth_key_id: Option<u64>,
    pub node_id: Option<u64>,
    pub membership: Option<NetworkMembership>,
    pub membership_checked_at: Option<i64>,
    pub revocation: Option<RevocationOutcome>,
    #[serde(default)]
    pub refresh_operation_id: Option<String>,
    // Ciphertext only; never included in safe snapshots.
    encrypted_grant: Option<Vec<u8>>,
}

impl NetworkRecord {
    fn new(device_id: String) -> Self {
        Self {
            device_id,
            consent: None,
            phase: NetworkPhase::AwaitingApproval,
            operation_id: None,
            auth_key_id: None,
            node_id: None,
            membership: None,
            membership_checked_at: None,
            revocation: None,
            encrypted_grant: None,
            refresh_operation_id: None,
        }
    }

    pub fn membership_query(&self) -> Result<MembershipQuery> {
        if self.auth_key_id.is_none() && self.node_id.is_none() {
            return Err(conflict(
                "network references require operator reconciliation",
            ));
        }
        Ok(MembershipQuery {
            device_id: DeviceId(self.device_id.clone()),
            host_name: self
                .consent
                .as_ref()
                .ok_or_else(|| conflict("no network consent"))?
                .accepted_host
                .clone(),
            auth_key_id: self.auth_key_id,
            node_id: self.node_id,
        })
    }

    pub fn revocation_request(&self) -> Result<RevocationRequest> {
        if self.auth_key_id.is_none() && self.node_id.is_none() {
            return Err(conflict(
                "unknown issuance outcome: recover references from netadmin audit before revoking",
            ));
        }
        Ok(RevocationRequest {
            device_id: DeviceId(self.device_id.clone()),
            auth_key_id: self.auth_key_id,
            node_id: self.node_id,
            requested_by: "coordinator-administrator".into(),
            reason: "application authorization revoked".into(),
        })
    }
}

fn existing(db: &Connection, device: &str) -> Result<NetworkRecord> {
    match get(db, KIND, device) {
        Ok(record) => Ok(record),
        Err(e) if e.code == ErrorCode::NotFound => Ok(NetworkRecord::new(device.into())),
        Err(e) => Err(e),
    }
}

fn authorized(db: &Connection, principal: &Principal, device: &str) -> Result<WorkerRecord> {
    if principal.is_admin {
        admin(principal)?;
        // Administrative maintenance must retain access after app revocation.
        get(db, "worker", device)
    } else {
        let record = worker(db, principal, false)?;
        if record.id != device {
            return Err(err(
                ErrorCode::Forbidden,
                "network identity belongs to another device",
            ));
        }
        Ok(record)
    }
}

fn request(w: &WorkerRecord, consent: HostConsent) -> EnrollmentRequest {
    EnrollmentRequest {
        device_id: DeviceId(w.id.clone()),
        host_name: consent.accepted_host.clone(),
        // Guest/compiler/coordinator enrollment requires a separate operator flow.
        host_kind: HostKind::Physical,
        roles: vec![NodeRole::Worker],
        consent,
        approval: HostApproval {
            device_id: DeviceId(w.id.clone()),
            approved_by: "coordinator-administrator".into(),
            approved_at: OffsetDateTime::now_utc(),
            authorization_ref: format!("project:{}/worker:{}", w.project_id, w.id),
        },
    }
}

fn save(db: &Connection, w: &WorkerRecord, record: &NetworkRecord) -> Result<()> {
    put(db, KIND, &w.id, &w.project_id, record)
}

impl Store {
    pub fn network_consent(&self, principal: &Principal, consent: HostConsent) -> Result<()> {
        if principal.is_admin {
            return Err(err(
                ErrorCode::Forbidden,
                "network consent requires the device owner identity",
            ));
        }
        let mut db = self.lock()?;
        let tx = db.transaction().map_err(sql)?;
        let w = authorized(&tx, principal, &consent.device_id.0)?;
        if !w.registration.owner_consented {
            return Err(err(
                ErrorCode::Forbidden,
                "inviting coordinator consent is required",
            ));
        }
        bounded(&consent.accepted_by, 256, "consenting owner")?;
        request(&w, consent.clone())
            .validate(OffsetDateTime::now_utc())
            .map_err(|_| invalid("network consent does not satisfy the current disclosure"))?;
        if let Some(switch) = &consent.tailnet_switch {
            bounded(&switch.previous_control_url, 2048, "previous control URL")?;
            if switch.accepted_at > OffsetDateTime::now_utc() + time::Duration::minutes(5) {
                return Err(invalid("tailnet switch consent is in the future"));
            }
        }
        let mut record = existing(&tx, &w.id)?;
        if record.consent.as_ref() == Some(&consent) {
            return Ok(());
        }
        if record.phase != NetworkPhase::AwaitingApproval {
            return Err(conflict(
                "consent cannot change after an enrollment operation begins",
            ));
        }
        for other in all::<NetworkRecord>(&tx, KIND)? {
            if other.device_id != w.id
                && other.phase != NetworkPhase::Revoked
                && other
                    .consent
                    .as_ref()
                    .is_some_and(|c| c.accepted_host == consent.accepted_host)
            {
                return Err(conflict("network host label is reserved by another device"));
            }
        }
        record.consent = Some(consent);
        save(&tx, &w, &record)?;
        event(
            &tx,
            Some(&w.project_id),
            "network_consent_recorded",
            json!({"worker_id":w.id}),
        )?;
        tx.commit().map_err(sql)
    }

    pub fn network_snapshot(&self, principal: &Principal) -> Result<Value> {
        let db = self.lock()?;
        let workers = if principal.is_admin {
            admin(principal)?;
            all::<WorkerRecord>(&db, "worker")?
        } else {
            vec![worker(&db, principal, false)?]
        };
        let mut devices = Vec::with_capacity(workers.len());
        for w in workers {
            let record = existing(&db, &w.id)?;
            devices.push(json!({
                "device_id":w.id, "approved":w.approved, "application_revoked":w.revoked,
                "inviting_coordinator":w.registration.consent_host,
                "network_host":record.consent.as_ref().map(|c| c.accepted_host.as_str()),
                "owner_consented":record.consent.is_some(), "phase":record.phase,
                "auth_key_id":record.auth_key_id, "node_id":record.node_id,
                "membership":record.membership, "membership_checked_at":record.membership_checked_at,
                "refresh_pending":record.refresh_operation_id.is_some(),
                "revocation":record.revocation,
            }));
        }
        Ok(json!({"devices":devices}))
    }

    pub fn network_grant(&self, principal: &Principal) -> Result<EnrollmentGrant> {
        let db = self.lock()?;
        if principal.is_admin {
            return Err(err(
                ErrorCode::Forbidden,
                "grants are delivered only to their device",
            ));
        }
        let w = worker(&db, principal, false)?;
        if !w.approved {
            return Err(conflict("awaiting individual approval"));
        }
        let record = existing(&db, &w.id)?;
        if record.phase != NetworkPhase::GrantIssued {
            return Err(conflict("awaiting grant issuance or reconciliation"));
        }
        let bytes = record
            .encrypted_grant
            .as_ref()
            .filter(|b| b.len() >= 12)
            .ok_or_else(|| conflict("no deliverable grant"))?;
        let aad = format!("dacs-network-grant-v1:{}", w.id);
        let plaintext = ChaCha20Poly1305::new((&self.key).into())
            .decrypt(
                Nonce::from_slice(&bytes[..12]),
                Payload {
                    msg: &bytes[12..],
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| err(ErrorCode::Internal, "network grant authentication failed"))?;
        let grant: EnrollmentGrant = serde_json::from_slice(&plaintext).map_err(serialization)?;
        if grant.expires_at <= OffsetDateTime::now_utc() {
            return Err(conflict(
                "network grant expired; administrator reconciliation required",
            ));
        }
        Ok(grant)
    }

    pub fn network_records(&self) -> Result<Vec<(WorkerRecord, NetworkRecord)>> {
        let db = self.lock()?;
        all::<NetworkRecord>(&db, KIND)?
            .into_iter()
            .map(|r| Ok((get(&db, "worker", &r.device_id)?, r)))
            .collect()
    }

    pub fn network_record(&self, principal: &Principal, device: &str) -> Result<NetworkRecord> {
        let db = self.lock()?;
        authorized(&db, principal, device)?;
        existing(&db, device)
    }

    /// Commits uncertainty *before* calling netadmin. A crash cannot permit reissue.
    pub fn network_begin_issue(&self, device: &str) -> Result<Option<(String, EnrollmentRequest)>> {
        let mut db = self.lock()?;
        let tx = db.transaction().map_err(sql)?;
        let w: WorkerRecord = get(&tx, "worker", device)?;
        let mut record = existing(&tx, device)?;
        if !w.approved
            || w.revoked
            || !w.registration.owner_consented
            || record.phase != NetworkPhase::AwaitingApproval
        {
            return Ok(None);
        }
        let Some(consent) = record.consent.clone() else {
            return Ok(None);
        };
        let project: Project = get(&tx, "project", &w.project_id)?;
        if w.registration.policy_revision != project.policy_revision {
            return Ok(None);
        }
        let req = request(&w, consent);
        req.validate(OffsetDateTime::now_utc())
            .map_err(|_| invalid("invalid network enrollment consent"))?;
        let operation = id();
        record.operation_id = Some(operation.clone());
        record.phase = NetworkPhase::IssueUncertain;
        save(&tx, &w, &record)?;
        event(
            &tx,
            Some(&w.project_id),
            "network_issue_intent",
            json!({"worker_id":w.id,"operation_id":operation}),
        )?;
        tx.commit().map_err(sql)?;
        Ok(Some((operation, req)))
    }

    pub fn network_finish_issue(
        &self,
        device: &str,
        operation: &str,
        grant: EnrollmentGrant,
    ) -> Result<()> {
        let mut db = self.lock()?;
        let tx = db.transaction().map_err(sql)?;
        let w: WorkerRecord = get(&tx, "worker", device)?;
        let mut record = existing(&tx, device)?;
        if record.operation_id.as_deref() != Some(operation)
            || record.auth_key_id.is_some()
            || !matches!(
                record.phase,
                NetworkPhase::IssueUncertain | NetworkPhase::RevocationPending
            )
        {
            return Err(conflict("network issuance operation no longer matches"));
        }
        if grant.device_id.0 != device
            || grant.contract_revision != CONTRACT_REVISION
            || record
                .consent
                .as_ref()
                .is_none_or(|c| c.accepted_host != grant.host_name)
            || grant.tags.len() != 1
            || grant.tags[0] != NodeRole::Worker.tag()
            || grant.auth_key_id == 0
            || grant.auth_key.expose().is_empty()
        {
            return Err(conflict(
                "netadmin grant did not match authorized worker enrollment",
            ));
        }
        record.auth_key_id = Some(grant.auth_key_id);
        if w.revoked || record.phase == NetworkPhase::RevocationPending {
            record.phase = NetworkPhase::RevocationPending;
        } else {
            let nonce = random_bytes::<12>();
            let aad = format!("dacs-network-grant-v1:{device}");
            let plaintext = serde_json::to_vec(&grant).map_err(serialization)?;
            let encrypted = ChaCha20Poly1305::new((&self.key).into())
                .encrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: &plaintext,
                        aad: aad.as_bytes(),
                    },
                )
                .map_err(|_| err(ErrorCode::Internal, "network grant encryption failed"))?;
            let mut bytes = nonce.to_vec();
            bytes.extend_from_slice(&encrypted);
            record.encrypted_grant = Some(bytes);
            record.phase = NetworkPhase::GrantIssued;
        }
        save(&tx, &w, &record)?;
        tx.commit().map_err(sql)
    }

    pub fn network_begin_refresh(
        &self,
        principal: &Principal,
        device: &str,
    ) -> Result<(String, MembershipQuery)> {
        let mut db = self.lock()?;
        let tx = db.transaction().map_err(sql)?;
        let w = authorized(&tx, principal, device)?;
        let mut record = existing(&tx, device)?;
        let query = record.membership_query()?;
        let operation = id();
        record.refresh_operation_id = Some(operation.clone());
        save(&tx, &w, &record)?;
        tx.commit().map_err(sql)?;
        Ok((operation, query))
    }

    pub fn network_membership(
        &self,
        device: &str,
        operation: &str,
        membership: NetworkMembership,
    ) -> Result<()> {
        let mut db = self.lock()?;
        let tx = db.transaction().map_err(sql)?;
        let w: WorkerRecord = get(&tx, "worker", device)?;
        let mut record = existing(&tx, device)?;
        if record.refresh_operation_id.as_deref() != Some(operation) {
            return Err(conflict("network membership query was superseded"));
        }
        record.refresh_operation_id = None;
        if membership.device_id.0 != device
            || record
                .consent
                .as_ref()
                .is_none_or(|c| c.accepted_host != membership.host_name)
        {
            return Err(conflict("netadmin membership identity mismatch"));
        }
        match &membership.state {
            MembershipState::Joined { node_id, .. } => {
                if record.node_id.is_some_and(|old| old != *node_id) {
                    return Err(conflict("network node reference changed unexpectedly"));
                }
                record.node_id = Some(*node_id);
            }
            MembershipState::KeyIssued { auth_key_id, .. }
            | MembershipState::KeyExpired { auth_key_id } => {
                if record.auth_key_id != Some(*auth_key_id) {
                    return Err(conflict("network key reference changed unexpectedly"));
                }
            }
            _ => {}
        }
        record.membership = Some(membership);
        record.membership_checked_at = Some(now());
        save(&tx, &w, &record)?;
        tx.commit().map_err(sql)
    }

    /// App cancellation and durable network intent commit together, before I/O.
    pub fn network_revoke(&self, principal: &Principal, device: &str) -> Result<NetworkRecord> {
        admin(principal)?;
        let mut db = self.lock()?;
        let tx = db.transaction().map_err(sql)?;
        let w: WorkerRecord = get(&tx, "worker", device)?;
        if !w.revoked {
            engine::execute(
                &tx,
                principal,
                Command::RevokeWorker {
                    worker_id: device.into(),
                },
            )?;
        }
        let mut record = existing(&tx, device)?;
        record.refresh_operation_id = None;
        if record.phase != NetworkPhase::Revoked {
            // No issuance intent means there is no external outcome to guess.
            record.phase = if record.operation_id.is_none()
                && record.auth_key_id.is_none()
                && record.node_id.is_none()
            {
                NetworkPhase::Revoked
            } else {
                NetworkPhase::RevocationPending
            };
        }
        record.encrypted_grant = None;
        save(&tx, &w, &record)?;
        tx.commit().map_err(sql)?;
        Ok(record)
    }

    pub fn network_finish_revoke(&self, device: &str, outcome: RevocationOutcome) -> Result<()> {
        let mut db = self.lock()?;
        let tx = db.transaction().map_err(sql)?;
        let w: WorkerRecord = get(&tx, "worker", device)?;
        let mut record = existing(&tx, device)?;
        if !w.revoked
            || record.phase != NetworkPhase::RevocationPending
            || outcome.device_id.0 != device
        {
            return Err(conflict("network revocation does not match pending device"));
        }
        if outcome.verified_absent {
            record.phase = NetworkPhase::Revoked;
            if let Some(consent) = &record.consent {
                record.membership = Some(NetworkMembership {
                    device_id: DeviceId(device.into()),
                    host_name: consent.accepted_host.clone(),
                    state: MembershipState::Revoked {
                        node_id: record.node_id,
                    },
                    overlay_ips: Vec::new(),
                    tags: Vec::new(),
                    last_seen: None,
                });
                record.membership_checked_at = Some(outcome.at.unix_timestamp());
            }
        }
        record.revocation = Some(outcome);
        save(&tx, &w, &record)?;
        tx.commit().map_err(sql)
    }

    /// Explicit operator recovery from netadmin audit, never hostname guessing.
    pub fn network_reconcile(
        &self,
        principal: &Principal,
        device: &str,
        auth_key_id: Option<u64>,
        node_id: Option<u64>,
    ) -> Result<()> {
        admin(principal)?;
        if auth_key_id.is_none_or(|id| id == 0) || node_id == Some(0) {
            return Err(invalid(
                "recover the audit-verified issuance key ID; a node ID alone cannot reconcile an unknown key",
            ));
        }
        let mut db = self.lock()?;
        let tx = db.transaction().map_err(sql)?;
        let w: WorkerRecord = get(&tx, "worker", device)?;
        let mut record = existing(&tx, device)?;
        if record.operation_id.is_none()
            || record.auth_key_id.is_some()
            || record.node_id.is_some()
            || !matches!(
                record.phase,
                NetworkPhase::IssueUncertain | NetworkPhase::RevocationPending
            )
        {
            return Err(conflict("only an unknown issuance may recover references"));
        }
        for other in all::<NetworkRecord>(&tx, KIND)? {
            if other.device_id != device
                && (auth_key_id.is_some() && other.auth_key_id == auth_key_id
                    || node_id.is_some() && other.node_id == node_id)
            {
                return Err(conflict(
                    "network reference is already bound to another device",
                ));
            }
        }
        record.auth_key_id = auth_key_id;
        record.node_id = node_id;
        record.phase = if w.revoked {
            NetworkPhase::RevocationPending
        } else {
            NetworkPhase::ReferencesRecovered
        };
        save(&tx, &w, &record)?;
        event(
            &tx,
            Some(&w.project_id),
            "network_references_recovered",
            json!({"worker_id":device,"auth_key_id":auth_key_id,"node_id":node_id}),
        )?;
        tx.commit().map_err(sql)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dacs_net_contract::{
        ADMIN_ACCESS_DISCLOSURE_REVISION, CoordinatorEndpoints, HostName, Secret,
    };

    const ADMIN: &str = "network-tests-administrator-credential-32-bytes";
    fn admin_identity() -> Principal {
        Principal {
            is_admin: true,
            device_id: None,
        }
    }
    fn command(store: &Store, command: Command) -> Value {
        store
            .execute(
                &admin_identity(),
                CommandEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    command_id: id(),
                    command,
                },
            )
            .unwrap()
            .result
    }
    fn enrolled(store: &Store, approved: bool) -> (Principal, String) {
        let project: Project = serde_json::from_value(command(
            store,
            Command::CreateProject {
                name: "network test".into(),
                target_branch: "main".into(),
                max_tasks: 10,
                max_depth: 2,
            },
        ))
        .unwrap();
        let invitation: Invitation = serde_json::from_value(command(
            store,
            Command::CreateInvitation {
                project_id: project.id,
                host_name: "inviting-coordinator".into(),
                expires_in_seconds: 3600,
            },
        ))
        .unwrap();
        let enrollment = store
            .enroll(
                &invitation.token,
                WorkerRegistration {
                    name: "test-pc".into(),
                    physical_host_id: id(),
                    os: "linux".into(),
                    architecture: "x86_64".into(),
                    capabilities: vec!["coordinator".into(), "compile_scheduler".into()],
                    capacity: Resources::default(),
                    omp_version: "test".into(),
                    policy_revision: 1,
                    owner_consented: true,
                    consent_host: invitation.host_name,
                },
            )
            .unwrap();
        if approved {
            command(
                store,
                Command::ApproveWorker {
                    worker_id: enrollment.worker.id,
                },
            );
        }
        (
            store.authenticate(&enrollment.device_token).unwrap(),
            enrollment.device_token,
        )
    }
    fn consent(principal: &Principal) -> HostConsent {
        HostConsent {
            device_id: DeviceId(principal.device_id.clone().unwrap()),
            accepted_host: HostName::try_from(format!(
                "pc-{}",
                &principal.device_id.as_ref().unwrap()[..8]
            ))
            .unwrap(),
            accepted_by: "local-owner".into(),
            accepted_at: OffsetDateTime::now_utc(),
            disclosure_revision: ADMIN_ACCESS_DISCLOSURE_REVISION.into(),
            tailnet_switch: None,
        }
    }
    fn grant(request: &EnrollmentRequest) -> EnrollmentGrant {
        EnrollmentGrant {
            contract_revision: CONTRACT_REVISION.into(),
            device_id: request.device_id.clone(),
            host_name: request.host_name.clone(),
            login_server: "https://network.example.test".into(),
            auth_key: Secret::new("test-secret-never-in-snapshot"),
            auth_key_id: 42,
            tags: vec![NodeRole::Worker.tag().into()],
            expires_at: OffsetDateTime::now_utc() + time::Duration::minutes(10),
            coordinator: CoordinatorEndpoints {
                bootstrap_url: "https://bootstrap.example.test".into(),
                overlay_url: "https://coordinator.example.test".into(),
            },
        }
    }

    #[test]
    fn consent_and_grant_are_bound_to_authenticated_device() {
        let store = Store::open(":memory:", ADMIN).unwrap();
        let (first, _) = enrolled(&store, true);
        let (second, _) = enrolled(&store, true);
        assert_eq!(
            store
                .network_consent(&second, consent(&first))
                .unwrap_err()
                .code,
            ErrorCode::Forbidden
        );
        assert_eq!(
            store
                .network_consent(&admin_identity(), consent(&first))
                .unwrap_err()
                .code,
            ErrorCode::Forbidden
        );
        store.network_consent(&first, consent(&first)).unwrap();
        let device = first.device_id.as_deref().unwrap();
        let (operation, request) = store.network_begin_issue(device).unwrap().unwrap();
        // Client capabilities never select ACL roles.
        assert_eq!(request.roles, vec![NodeRole::Worker]);
        store
            .network_finish_issue(device, &operation, grant(&request))
            .unwrap();
        assert_eq!(store.network_grant(&first).unwrap().device_id.0, device);
        assert_eq!(
            store.network_grant(&second).unwrap_err().code,
            ErrorCode::Conflict
        );
        assert_eq!(
            store.network_grant(&admin_identity()).unwrap_err().code,
            ErrorCode::Forbidden
        );
        assert_eq!(
            store.network_record(&second, device).err().unwrap().code,
            ErrorCode::Forbidden
        );
        let snapshot = store.network_snapshot(&second).unwrap();
        assert_eq!(snapshot["devices"].as_array().unwrap().len(), 1);
        assert_eq!(
            snapshot["devices"][0]["device_id"],
            second.device_id.unwrap()
        );
    }

    #[test]
    fn consent_does_not_substitute_for_individual_approval() {
        let store = Store::open(":memory:", ADMIN).unwrap();
        let (principal, _) = enrolled(&store, false);
        store
            .network_consent(&principal, consent(&principal))
            .unwrap();
        let device = principal.device_id.as_deref().unwrap();
        assert!(store.network_begin_issue(device).unwrap().is_none());
        assert_eq!(
            store.network_grant(&principal).unwrap_err().code,
            ErrorCode::Conflict
        );
    }

    #[test]
    fn uncertain_issue_survives_restart_without_duplicate_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coordinator.sqlite");
        let store = Store::open(&path, ADMIN).unwrap();
        let (principal, _) = enrolled(&store, true);
        store
            .network_consent(&principal, consent(&principal))
            .unwrap();
        let device = principal.device_id.as_deref().unwrap();
        assert!(store.network_begin_issue(device).unwrap().is_some());
        drop(store);
        let reopened = Store::open(&path, ADMIN).unwrap();
        assert!(reopened.network_begin_issue(device).unwrap().is_none());
        assert_eq!(
            reopened.network_grant(&principal).unwrap_err().code,
            ErrorCode::Conflict
        );
        assert_eq!(
            reopened
                .network_record(&admin_identity(), device)
                .unwrap()
                .phase,
            NetworkPhase::IssueUncertain
        );
        assert!(
            reopened
                .network_record(&admin_identity(), device)
                .unwrap()
                .membership_query()
                .is_err()
        );
    }

    #[test]
    fn grant_is_encrypted_and_device_bound_at_rest() {
        let store = Store::open(":memory:", ADMIN).unwrap();
        let (first, _) = enrolled(&store, true);
        let (second, _) = enrolled(&store, true);
        store.network_consent(&first, consent(&first)).unwrap();
        let device = first.device_id.as_deref().unwrap();
        let (operation, request) = store.network_begin_issue(device).unwrap().unwrap();
        store
            .network_finish_issue(device, &operation, grant(&request))
            .unwrap();
        let snapshot = store
            .network_snapshot(&admin_identity())
            .unwrap()
            .to_string();
        assert!(!snapshot.contains("test-secret-never-in-snapshot"));
        assert!(!snapshot.contains("encrypted_grant"));
        let db = store.lock().unwrap();
        let stored: String = db
            .query_row(
                "SELECT body FROM records WHERE kind=?1 AND id=?2",
                params![KIND, device],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!stored.contains("test-secret-never-in-snapshot"));
        let mut copied: NetworkRecord = serde_json::from_str(&stored).unwrap();
        copied.device_id = second.device_id.clone().unwrap();
        let w: WorkerRecord = get(&db, "worker", &copied.device_id).unwrap();
        save(&db, &w, &copied).unwrap();
        drop(db);
        assert_eq!(
            store.network_grant(&second).unwrap_err().code,
            ErrorCode::Internal
        );
    }

    #[test]
    fn late_issue_response_is_retained_for_revocation_not_delivery() {
        let store = Store::open(":memory:", ADMIN).unwrap();
        let (principal, token) = enrolled(&store, true);
        store
            .network_consent(&principal, consent(&principal))
            .unwrap();
        let device = principal.device_id.as_deref().unwrap();
        let (operation, request) = store.network_begin_issue(device).unwrap().unwrap();
        store.network_revoke(&admin_identity(), device).unwrap();
        assert!(store.authenticate(&token).is_err());
        store
            .network_finish_issue(device, &operation, grant(&request))
            .unwrap();
        let record = store.network_record(&admin_identity(), device).unwrap();
        assert_eq!(record.phase, NetworkPhase::RevocationPending);
        assert_eq!(record.revocation_request().unwrap().auth_key_id, Some(42));
        assert!(record.encrypted_grant.is_none());
        assert!(store.network_grant(&principal).is_err());
    }

    #[test]
    fn superseded_membership_cannot_overwrite_revocation() {
        let store = Store::open(":memory:", ADMIN).unwrap();
        let (principal, _) = enrolled(&store, true);
        store
            .network_consent(&principal, consent(&principal))
            .unwrap();
        let device = principal.device_id.as_deref().unwrap();
        let (operation, request) = store.network_begin_issue(device).unwrap().unwrap();
        store
            .network_finish_issue(device, &operation, grant(&request))
            .unwrap();
        let (refresh, query) = store.network_begin_refresh(&principal, device).unwrap();
        store.network_revoke(&admin_identity(), device).unwrap();
        let stale = NetworkMembership {
            device_id: query.device_id,
            host_name: query.host_name,
            state: MembershipState::Joined {
                node_id: 99,
                online: true,
            },
            overlay_ips: Vec::new(),
            tags: vec![NodeRole::Worker.tag().into()],
            last_seen: None,
        };
        assert_eq!(
            store
                .network_membership(device, &refresh, stale)
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert_eq!(
            store
                .network_record(&admin_identity(), device)
                .unwrap()
                .phase,
            NetworkPhase::RevocationPending
        );
    }

    #[test]
    fn revoked_unknown_issue_remains_admin_reconcilable() {
        let store = Store::open(":memory:", ADMIN).unwrap();
        let (principal, _) = enrolled(&store, true);
        store
            .network_consent(&principal, consent(&principal))
            .unwrap();
        let device = principal.device_id.as_deref().unwrap();
        store.network_begin_issue(device).unwrap().unwrap();
        store.network_revoke(&admin_identity(), device).unwrap();
        let record = store.network_record(&admin_identity(), device).unwrap();
        assert_eq!(record.phase, NetworkPhase::RevocationPending);
        assert!(record.revocation_request().is_err());
        assert_eq!(
            store
                .network_reconcile(&principal, device, Some(42), None)
                .unwrap_err()
                .code,
            ErrorCode::Forbidden
        );
        store
            .network_reconcile(&admin_identity(), device, Some(42), None)
            .unwrap();
        store
            .network_finish_revoke(
                device,
                RevocationOutcome {
                    device_id: DeviceId(device.into()),
                    key_expired: false,
                    node_deleted: None,
                    verified_absent: false,
                    at: OffsetDateTime::now_utc(),
                },
            )
            .unwrap();
        assert_eq!(
            store
                .network_record(&admin_identity(), device)
                .unwrap()
                .phase,
            NetworkPhase::RevocationPending
        );
        store
            .network_finish_revoke(
                device,
                RevocationOutcome {
                    device_id: DeviceId(device.into()),
                    key_expired: true,
                    node_deleted: None,
                    verified_absent: true,
                    at: OffsetDateTime::now_utc(),
                },
            )
            .unwrap();
        assert_eq!(
            store
                .network_record(&admin_identity(), device)
                .unwrap()
                .phase,
            NetworkPhase::Revoked
        );
    }

    #[test]
    fn escalated_grant_never_becomes_deliverable() {
        let store = Store::open(":memory:", ADMIN).unwrap();
        let (principal, _) = enrolled(&store, true);
        store
            .network_consent(&principal, consent(&principal))
            .unwrap();
        let device = principal.device_id.as_deref().unwrap();
        let (operation, request) = store.network_begin_issue(device).unwrap().unwrap();
        let mut wrong = grant(&request);
        wrong.tags.push(NodeRole::Coordinator.tag().into());
        assert!(
            store
                .network_finish_issue(device, &operation, wrong)
                .is_err()
        );
        assert!(store.network_begin_issue(device).unwrap().is_none());
        assert!(store.network_grant(&principal).is_err());
    }
}
