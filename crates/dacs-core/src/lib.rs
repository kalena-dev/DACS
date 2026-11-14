pub mod builds;
mod engine;
pub mod network;
mod publication;
mod scheduling;

use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use dacs_protocol::*;
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
pub struct Store {
    db: Arc<Mutex<Connection>>,
    key: [u8; 32],
}

fn err(code: ErrorCode, message: impl Into<String>) -> ApiError {
    ApiError::new(code, message)
}
fn invalid(message: impl Into<String>) -> ApiError {
    err(ErrorCode::Invalid, message)
}
fn conflict(message: impl Into<String>) -> ApiError {
    err(ErrorCode::Conflict, message)
}
fn sql(e: rusqlite::Error) -> ApiError {
    err(ErrorCode::Internal, format!("database: {e}"))
}
fn serialization(e: serde_json::Error) -> ApiError {
    err(ErrorCode::Internal, format!("record encoding: {e}"))
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    rand::rng().fill_bytes(&mut bytes);
    bytes
}
fn token() -> String {
    hex::encode(random_bytes::<32>())
}
fn hash(value: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(value))
}
fn value<T: Serialize>(v: &T) -> Result<Value> {
    serde_json::to_value(v).map_err(serialization)
}
fn bounded(s: &str, max: usize, name: &str) -> Result<()> {
    if s.trim().is_empty() || s.len() > max || s.contains('\0') {
        return Err(invalid(format!("invalid {name}")));
    }
    Ok(())
}
fn identifier(s: &str) -> Result<()> {
    bounded(s, 128, "identifier")?;
    if !s
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c))
    {
        return Err(invalid("invalid identifier characters"));
    }
    Ok(())
}
fn sha(s: &str) -> Result<()> {
    if !matches!(s.len(), 40 | 64)
        || !s
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return Err(invalid("expected lowercase Git SHA"));
    }
    Ok(())
}
fn admin(p: &Principal) -> Result<()> {
    if !p.is_admin || p.device_id.is_some() {
        return Err(err(
            ErrorCode::Forbidden,
            "coordinator administrator required",
        ));
    }
    Ok(())
}
fn put<T: Serialize>(
    db: &Connection,
    kind: &str,
    key: &str,
    project: &str,
    record: &T,
) -> Result<()> {
    let body = serde_json::to_string(record).map_err(serialization)?;
    db.execute("INSERT INTO records(kind,id,project,body) VALUES(?1,?2,?3,?4) ON CONFLICT(kind,id) DO UPDATE SET project=excluded.project,body=excluded.body", params![kind,key,project,body]).map_err(sql)?;
    Ok(())
}
fn get<T: DeserializeOwned>(db: &Connection, kind: &str, key: &str) -> Result<T> {
    let body: Option<String> = db
        .query_row(
            "SELECT body FROM records WHERE kind=?1 AND id=?2",
            params![kind, key],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql)?;
    serde_json::from_str(
        &body.ok_or_else(|| err(ErrorCode::NotFound, format!("{kind} not found")))?,
    )
    .map_err(serialization)
}
fn all<T: DeserializeOwned>(db: &Connection, kind: &str) -> Result<Vec<T>> {
    let mut stmt = db
        .prepare("SELECT body FROM records WHERE kind=?1 ORDER BY rowid")
        .map_err(sql)?;
    let rows = stmt
        .query_map([kind], |r| r.get::<_, String>(0))
        .map_err(sql)?;
    rows.map(|row| serde_json::from_str(&row.map_err(sql)?).map_err(serialization))
        .collect()
}
fn event(db: &Connection, project: Option<&str>, kind: &str, payload: Value) -> Result<()> {
    db.execute(
        "INSERT INTO events(at,project,kind,payload) VALUES(?1,?2,?3,?4)",
        params![now(), project, kind, payload.to_string()],
    )
    .map_err(sql)?;
    Ok(())
}
fn message(
    db: &Connection,
    worker: &str,
    attempt: Option<&AttemptRecord>,
    kind: &str,
    payload: Value,
    message_id: Option<&str>,
) -> Result<()> {
    let message_id = message_id.map(str::to_owned).unwrap_or_else(id);
    db.execute(
        "INSERT INTO messages(id,worker,attempt,generation,kind,payload,at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![
            message_id,
            worker,
            attempt.map(|a| a.id.as_str()),
            attempt.map(|a| a.generation as i64),
            kind,
            payload.to_string(),
            now()
        ],
    )
    .map_err(sql)?;
    Ok(())
}
fn worker(db: &Connection, p: &Principal, approved: bool) -> Result<WorkerRecord> {
    let w: WorkerRecord = get(
        db,
        "worker",
        p.device_id
            .as_deref()
            .ok_or_else(|| err(ErrorCode::Forbidden, "device identity required"))?,
    )?;
    if w.revoked || (approved && !w.approved) {
        return Err(err(
            ErrorCode::Forbidden,
            "device is revoked or awaiting approval",
        ));
    }
    Ok(w)
}
fn project(db: &Connection, p: &Principal, project_id: &str) -> Result<Project> {
    identifier(project_id)?;
    if !p.is_admin {
        let w = worker(db, p, true)?;
        if w.project_id != project_id {
            return Err(err(ErrorCode::Forbidden, "project access denied"));
        }
    } else {
        admin(p)?;
    }
    get(db, "project", project_id)
}

impl Store {
    pub fn open(path: impl AsRef<Path>, admin_token: &str) -> Result<Self> {
        if admin_token.len() < 32 || admin_token.len() > 4096 {
            return Err(invalid("administrator token must contain 32–4096 bytes"));
        }
        let mut db = Connection::open(path).map_err(sql)?;
        db.busy_timeout(std::time::Duration::from_secs(10))
            .map_err(sql)?;
        db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS records(kind TEXT NOT NULL,id TEXT NOT NULL,project TEXT NOT NULL,body TEXT NOT NULL,PRIMARY KEY(kind,id));
            CREATE INDEX IF NOT EXISTS record_project ON records(kind,project);
            CREATE TABLE IF NOT EXISTS credentials(hash TEXT PRIMARY KEY,worker TEXT NOT NULL UNIQUE);
            CREATE TABLE IF NOT EXISTS requests(principal TEXT NOT NULL,id TEXT NOT NULL,digest TEXT NOT NULL,response BLOB NOT NULL,PRIMARY KEY(principal,id));
            CREATE TABLE IF NOT EXISTS events(sequence INTEGER PRIMARY KEY AUTOINCREMENT,at INTEGER NOT NULL,project TEXT,kind TEXT NOT NULL,payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS messages(sequence INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT NOT NULL,worker TEXT NOT NULL,attempt TEXT,generation INTEGER,kind TEXT NOT NULL,payload TEXT NOT NULL,at INTEGER NOT NULL DEFAULT 0,UNIQUE(worker,id));
            CREATE TABLE IF NOT EXISTS acknowledgments(worker TEXT PRIMARY KEY,sequence INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS reservations(attempt TEXT PRIMARY KEY,worker TEXT NOT NULL,physical_host TEXT NOT NULL,resources TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS deferred(project TEXT PRIMARY KEY,contract INTEGER NOT NULL DEFAULT 0,requirements TEXT);
            PRAGMA user_version=1;").map_err(sql)?;
        // Databases created before messages recorded their queue time lack the column.
        let timed: bool = db
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name='at'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map_err(sql)?
            != 0;
        if !timed {
            db.execute(
                "ALTER TABLE messages ADD COLUMN at INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(sql)?;
        }
        let tx = db.transaction().map_err(sql)?;
        let existing: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key='admin_hash'", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(sql)?;
        let digest = hash(admin_token);
        if let Some(existing) = existing {
            if existing != digest {
                return Err(err(
                    ErrorCode::Unauthorized,
                    "administrator token does not match this store",
                ));
            }
        } else {
            tx.execute(
                "INSERT INTO meta(key,value) VALUES('admin_hash',?1)",
                [digest],
            )
            .map_err(sql)?;
        }
        tx.commit().map_err(sql)?;
        let key =
            Sha256::digest([b"dacs-dedup-v1:".as_slice(), admin_token.as_bytes()].concat()).into();
        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            key,
        })
    }
    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.db
            .lock()
            .map_err(|_| err(ErrorCode::Internal, "database mutex poisoned"))
    }
    pub fn authenticate(&self, token: &str) -> Result<Principal> {
        if token.len() > 4096 || token.len() < 32 {
            return Err(err(ErrorCode::Unauthorized, "invalid credential"));
        }
        let db = self.lock()?;
        let digest = hash(token);
        let is_admin: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM meta WHERE key='admin_hash' AND value=?1)",
                [&digest],
                |r| r.get(0),
            )
            .map_err(sql)?;
        if is_admin {
            return Ok(Principal {
                is_admin: true,
                device_id: None,
            });
        }
        let device_id: Option<String> = db
            .query_row(
                "SELECT worker FROM credentials WHERE hash=?1",
                [digest],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql)?;
        let p = Principal {
            is_admin: false,
            device_id: Some(
                device_id.ok_or_else(|| err(ErrorCode::Unauthorized, "invalid credential"))?,
            ),
        };
        worker(&db, &p, false)?;
        Ok(p)
    }
    pub fn execute(
        &self,
        principal: &Principal,
        envelope: CommandEnvelope,
    ) -> Result<CommandResponse> {
        identifier(&envelope.command_id)?;
        if envelope.protocol_version != PROTOCOL_VERSION {
            return Err(invalid("unsupported protocol version"));
        }
        let encoded = serde_json::to_vec(&envelope).map_err(serialization)?;
        if encoded.len() > 1024 * 1024 {
            return Err(invalid("command exceeds 1 MiB"));
        }
        let mut db = self.lock()?;
        let tx = db.transaction().map_err(sql)?;
        let principal_id = if principal.is_admin {
            admin(principal)?;
            "administrator".to_owned()
        } else {
            worker(&tx, principal, false)?.id
        };
        let digest = hash(&encoded);
        let aad = format!("{principal_id}:{}", envelope.command_id);
        let saved: Option<(String, Vec<u8>)> = tx
            .query_row(
                "SELECT digest,response FROM requests WHERE principal=?1 AND id=?2",
                params![principal_id, envelope.command_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql)?;
        let cipher = ChaCha20Poly1305::new((&self.key).into());
        if let Some((prior, bytes)) = saved {
            if prior != digest {
                return Err(conflict("command ID reused with different contents"));
            }
            if bytes.len() < 12 {
                return Err(err(ErrorCode::Internal, "corrupt dedup response"));
            }
            let plaintext = cipher
                .decrypt(
                    Nonce::from_slice(&bytes[..12]),
                    Payload {
                        msg: &bytes[12..],
                        aad: aad.as_bytes(),
                    },
                )
                .map_err(|_| err(ErrorCode::Internal, "dedup response authentication failed"))?;
            return serde_json::from_slice::<Result<CommandResponse>>(&plaintext)
                .map_err(serialization)?;
        }
        tx.execute_batch("SAVEPOINT command_effects").map_err(sql)?;
        let outcome =
            engine::execute(&tx, principal, envelope.command).map(|result| CommandResponse {
                command_id: envelope.command_id.clone(),
                result,
            });
        if outcome.is_err() {
            tx.execute_batch("ROLLBACK TO command_effects")
                .map_err(sql)?;
        }
        tx.execute_batch("RELEASE command_effects").map_err(sql)?;
        let nonce = random_bytes::<12>();
        let plaintext = serde_json::to_vec(&outcome).map_err(serialization)?;
        let encrypted = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &plaintext,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| err(ErrorCode::Internal, "dedup encryption failed"))?;
        let mut bytes = nonce.to_vec();
        bytes.extend_from_slice(&encrypted);
        tx.execute(
            "INSERT INTO requests(principal,id,digest,response) VALUES(?1,?2,?3,?4)",
            params![principal_id, envelope.command_id, digest, bytes],
        )
        .map_err(sql)?;
        tx.commit().map_err(sql)?;
        outcome
    }
    pub fn authorize_project(&self, principal: &Principal, project_id: &str) -> Result<Project> {
        let db = self.lock()?;
        project(&db, principal, project_id)
    }
    pub fn attempt_context(
        &self,
        principal: &Principal,
        attempt_id: &str,
        generation: u64,
    ) -> Result<(AttemptRecord, TaskRecord)> {
        let db = self.lock()?;
        scheduling::owned_attempt(&db, principal, attempt_id, generation)
    }
    pub fn snapshot(&self, principal: &Principal) -> Result<Snapshot> {
        let db = self.lock()?;
        let scope = if principal.is_admin {
            admin(principal)?;
            None
        } else {
            Some(worker(&db, principal, true)?.project_id)
        };
        let allows = |p: &str| scope.as_ref().is_none_or(|s| s == p);
        let projects = all::<Project>(&db, "project")?
            .into_iter()
            .filter(|v| allows(&v.id))
            .collect();
        let workers: Vec<WorkerRecord> = all::<WorkerRecord>(&db, "worker")?
            .into_iter()
            .filter(|v| allows(&v.project_id))
            .collect();
        let tasks: Vec<TaskRecord> = all::<TaskRecord>(&db, "task")?
            .into_iter()
            .filter(|v| allows(&v.spec.project_id))
            .collect();
        let attempts = all::<AttemptRecord>(&db, "attempt")?
            .into_iter()
            .filter(|v| tasks.iter().any(|t| t.id == v.task_id))
            .collect();
        let candidates = all::<CandidateRecord>(&db, "candidate")?
            .into_iter()
            .filter(|v| allows(&v.project_id))
            .collect();
        let publications = all::<Publication>(&db, "publication")?
            .into_iter()
            .filter(|v| allows(&v.project_id))
            .collect();
        let mut stmt=db.prepare("SELECT sequence,at,project,kind,payload FROM events WHERE (?1 IS NULL OR project=?1) ORDER BY sequence DESC LIMIT 1000").map_err(sql)?;
        let rows = stmt
            .query_map([scope.as_deref()], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })
            .map_err(sql)?;
        let mut events = Vec::new();
        for row in rows {
            let (s, at, p, k, b) = row.map_err(sql)?;
            events.push(Event {
                sequence: s,
                at,
                project_id: p,
                kind: k,
                payload: serde_json::from_str(&b).map_err(serialization)?,
            });
        }
        events.reverse();
        let usage = all::<UsageRecord>(&db, "usage")?
            .into_iter()
            .filter(|u| workers.iter().any(|w| w.id == u.worker_id))
            .collect();
        let runtime_status = all::<WorkerRuntimeStatus>(&db, "runtime")?
            .into_iter()
            .filter(|s| workers.iter().any(|w| w.id == s.worker_id))
            .collect();
        let mut stmt = db
            .prepare("SELECT worker,sequence FROM acknowledgments")
            .map_err(sql)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(sql)?;
        let mut acknowledged = std::collections::HashMap::new();
        for row in rows {
            let (worker_id, sequence) = row.map_err(sql)?;
            acknowledged.insert(worker_id, sequence);
        }
        let mut stmt=db.prepare("SELECT m.sequence,m.id,m.worker,m.attempt,m.generation,m.kind,m.payload,m.at FROM messages m JOIN records w ON w.kind='worker' AND w.id=m.worker WHERE (?1 IS NULL OR w.project=?1) ORDER BY m.sequence DESC LIMIT 1000").map_err(sql)?;
        let rows = stmt
            .query_map([scope.as_deref()], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<u64>>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, i64>(7)?,
                ))
            })
            .map_err(sql)?;
        let mut messages = Vec::new();
        let mut deliveries = Vec::new();
        for row in rows {
            let (sequence, id, worker_id, attempt_id, generation, kind, payload, at) =
                row.map_err(sql)?;
            deliveries.push(MessageDelivery {
                sequence,
                queued_at: at,
                acknowledged: acknowledged.get(&worker_id).is_some_and(|&through| through >= sequence),
            });
            messages.push(ControlMessage {
                sequence,
                id,
                worker_id,
                attempt_id,
                generation,
                kind,
                payload: serde_json::from_str(&payload).map_err(serialization)?,
            });
        }
        messages.reverse();
        deliveries.reverse();
        Ok(Snapshot {
            projects,
            workers,
            tasks,
            attempts,
            candidates,
            publications,
            events,
            messages,
            usage,
            runtime_status,
            deliveries,
        })
    }
    pub fn invitation(&self, token: &str) -> Result<Invitation> {
        let db = self.lock()?;
        invitation(&db, token)
    }
    pub fn enroll(
        &self,
        invitation_token: &str,
        registration: WorkerRegistration,
    ) -> Result<Enrollment> {
        let mut db = self.lock()?;
        let tx = db.transaction().map_err(sql)?;
        let invite = invitation(&tx, invitation_token)?;
        engine::validate_registration(&registration)?;
        let p: Project = get(&tx, "project", &invite.project_id)?;
        if !registration.owner_consented || registration.consent_host != invite.host_name {
            return Err(err(
                ErrorCode::Forbidden,
                "local owner must consent to the named inviting host",
            ));
        }
        if registration.policy_revision != p.policy_revision {
            return Err(conflict("local consent must cover current project policy"));
        }
        let w = WorkerRecord {
            id: id(),
            project_id: p.id,
            registration,
            approved: false,
            revoked: false,
            draining: true,
            last_seen: now(),
            reserved: Resources::default(),
        };
        put(&tx, "worker", &w.id, &w.project_id, &w)?;
        let device_token = token();
        tx.execute(
            "INSERT INTO credentials(hash,worker) VALUES(?1,?2)",
            params![hash(&device_token), w.id],
        )
        .map_err(sql)?;
        event(
            &tx,
            Some(&w.project_id),
            "enrollment_pending",
            json!({"worker_id":w.id,"network_membership":"unavailable_external_adapter"}),
        )?;
        tx.commit().map_err(sql)?;
        Ok(Enrollment {
            worker: w,
            device_token,
        })
    }
    pub fn commands(
        &self,
        principal: &Principal,
        after_sequence: i64,
    ) -> Result<Vec<ControlMessage>> {
        if after_sequence < 0 {
            return Err(invalid("negative message cursor"));
        }
        let db = self.lock()?;
        let w = worker(&db, principal, false)?;
        let max: i64 = db
            .query_row(
                "SELECT COALESCE(MAX(sequence),0) FROM messages WHERE worker=?1",
                [&w.id],
                |r| r.get(0),
            )
            .map_err(sql)?;
        if after_sequence > max {
            return Err(invalid("message cursor exceeds recipient journal"));
        }
        let mut stmt=db.prepare("SELECT sequence,id,attempt,generation,kind,payload FROM messages WHERE worker=?1 AND sequence>?2 ORDER BY sequence LIMIT 1000").map_err(sql)?;
        let rows = stmt
            .query_map(params![w.id, after_sequence], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<u64>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })
            .map_err(sql)?;
        rows.map(|r| {
            let (s, id, a, g, k, p) = r.map_err(sql)?;
            Ok(ControlMessage {
                sequence: s,
                id,
                worker_id: w.id.clone(),
                attempt_id: a,
                generation: g,
                kind: k,
                payload: serde_json::from_str(&p).map_err(serialization)?,
            })
        })
        .collect()
    }
    pub fn maintain(&self) -> Result<()> {
        let mut db = self.lock()?;
        let tx = db.transaction().map_err(sql)?;
        scheduling::maintain(&tx)?;
        tx.commit().map_err(sql)
    }
}
fn invitation(db: &Connection, token: &str) -> Result<Invitation> {
    if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("invalid invitation token"));
    }
    let mut invitation: Invitation = get(db, "invitation", &hash(token))?;
    if invitation.revoked || invitation.expires_at <= now() {
        return Err(err(ErrorCode::Forbidden, "invitation expired or revoked"));
    }
    let p: Project = get(db, "project", &invitation.project_id)?;
    invitation.requirements = p.requirements;
    invitation.token = token.to_owned();
    Ok(invitation)
}

#[cfg(test)]
mod tests;
