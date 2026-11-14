//! Transactional compiler-pool ownership layered over ordinary service reservations.
use super::*;
use dacs_protocol::workflow::{PoolClaim, PoolOwnership};
use std::collections::BTreeSet;

/// Call inside the same transaction as admission, before delivering its assignment.
pub(crate) fn acquire_for_workflow(
    db: &Connection,
    task: &TaskRecord,
    attempt: &AttemptRecord,
) -> Result<()> {
    let TaskExecution::Workflow { request } = &task.spec.execution else {
        return Ok(());
    };
    if request["operation"]["kind"].as_str() == Some("compiler_service") {
        return admit_service(db, task, attempt, &request["operation"]);
    }
    let Some(distributed) = request.get("distributed").filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let initiating_worker: WorkerRecord = get(db, "worker", &attempt.worker_id)?;
    if distributed["initiator_physical_host"].as_str()
        != Some(initiating_worker.registration.physical_host_id.as_str())
    {
        return Err(invalid(
            "distributed initiator identity does not match enrolled physical host",
        ));
    }
    let claim: PoolClaim = serde_json::from_value(
        distributed
            .get("claim")
            .cloned()
            .ok_or_else(|| invalid("distributed workflow requires pool claim"))?,
    )
    .map_err(serialization)?;
    identifier(&claim.pool_id)?;
    if claim.service_attempt_ids.is_empty() {
        return Err(invalid(
            "pool requires admitted scheduler and compiler services",
        ));
    }
    let declared_pool = &distributed["pool"];
    if declared_pool["pool_id"].as_str() != Some(claim.pool_id.as_str()) {
        return Err(invalid("pool claim identity mismatch"));
    }
    let servers = declared_pool["servers"]
        .as_array()
        .ok_or_else(|| invalid("compiler servers missing"))?;
    let mut members = BTreeSet::new();
    let mut hosts = BTreeSet::new();
    let mut scheduler_count = 0;
    for service_id in &claim.service_attempt_ids {
        if !members.insert(service_id.clone()) {
            return Err(invalid("duplicate service attempt"));
        }
        let service: AttemptRecord = get(db, "attempt", service_id)?;
        let service_task: TaskRecord = get(db, "task", &service.task_id)?;
        let worker: WorkerRecord = get(db, "worker", &service.worker_id)?;
        if service_task.spec.project_id != task.spec.project_id
            || service_task.cancel_requested
            || service_task.generation != service.generation
            || service.lease_expires_at <= now()
            || service.settled
            || worker.revoked
            || !worker.approved
            || now() - worker.last_seen > 90
        {
            return Err(conflict(
                "compiler service is not live, authorized and reserved",
            ));
        }
        let reserved: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM reservations WHERE attempt=?1)",
                [service_id],
                |r| r.get(0),
            )
            .map_err(sql)?;
        if !reserved {
            return Err(conflict("compiler service reservation missing"));
        }
        let TaskExecution::Workflow {
            request: service_request,
        } = &service_task.spec.execution
        else {
            return Err(invalid("pool member is not a service workflow"));
        };
        let operation = &service_request["operation"];
        if operation["kind"].as_str() != Some("compiler_service")
            || operation["pool"]["pool_id"].as_str() != Some(claim.pool_id.as_str())
            || operation["pool"] != *declared_pool
        {
            return Err(invalid(
                "service pool configuration differs from build pool",
            ));
        }
        match operation["role"].as_str() {
            Some("scheduler") => scheduler_count += 1,
            Some("server") => {
                let server_id = operation["server_id"]
                    .as_str()
                    .ok_or_else(|| invalid("server identity missing"))?;
                let member = servers
                    .iter()
                    .find(|s| s["device_id"].as_str() == Some(server_id))
                    .ok_or_else(|| invalid("unregistered compiler server"))?;
                if server_id != worker.id
                    || member["physical_host_fingerprint"].as_str()
                        != Some(worker.registration.physical_host_id.as_str())
                    || !hosts.insert(worker.registration.physical_host_id.clone())
                {
                    return Err(invalid(
                        "compiler service does not match enrolled physical host",
                    ));
                }
                let units = member["capacity_units"]
                    .as_u64()
                    .ok_or_else(|| invalid("compiler capacity missing"))?;
                if u64::from(service.resources.cpu_millis) < units.saturating_mul(1000)
                    || service.resources.memory_mb == 0
                {
                    return Err(invalid("compiler service resources are insufficient"));
                }
            }
            _ => return Err(invalid("invalid compiler service role")),
        }
    }
    if scheduler_count != 1 || hosts.len() != servers.len() {
        return Err(invalid(
            "pool must contain exactly its scheduler and all registered servers",
        ));
    }
    for existing in all::<PoolOwnership>(db, "compiler_pool")? {
        if !existing.released
            && (existing.pool_id == claim.pool_id
                || existing.physical_hosts.iter().any(|h| hosts.contains(h)))
        {
            return Err(err(
                ErrorCode::CapacityUnavailable,
                "compiler pool or physical compiler host already held; queued task remains ready",
            ));
        }
    }
    let ownership = PoolOwnership {
        pool_id: claim.pool_id.clone(),
        project_id: task.spec.project_id.clone(),
        holder_attempt_id: attempt.id.clone(),
        service_attempt_ids: claim.service_attempt_ids,
        physical_hosts: hosts.into_iter().collect(),
        released: false,
    };
    put(
        db,
        "compiler_pool",
        &claim.pool_id,
        &task.spec.project_id,
        &ownership,
    )?;
    event(
        db,
        Some(&task.spec.project_id),
        "compiler_pool_acquired",
        value(&ownership)?,
    )
}

/// Call on every confirmed execution stop. Never expire ownership by heartbeat.
/// Build completion requests service drain; release waits for all actual stop reports.
pub(crate) fn reconcile_stopped(db: &Connection, attempt: &AttemptRecord) -> Result<()> {
    for mut ownership in all::<PoolOwnership>(db, "compiler_pool")? {
        if ownership.released {
            continue;
        }
        if ownership.holder_attempt_id == attempt.id {
            for service_id in &ownership.service_attempt_ids {
                let service: AttemptRecord = get(db, "attempt", service_id)?;
                let mut task: TaskRecord = get(db, "task", &service.task_id)?;
                if !scheduling::terminal(task.state) {
                    scheduling::cancel(db, &mut task, "compiler build ended; drain owned service")?;
                }
            }
        }
        let mut stopped = true;
        for id in
            std::iter::once(&ownership.holder_attempt_id).chain(&ownership.service_attempt_ids)
        {
            let exists: bool = db
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM records WHERE kind='execution_stopped' AND id=?1)",
                    [id],
                    |r| r.get(0),
                )
                .map_err(sql)?;
            stopped &= exists;
        }
        if stopped {
            ownership.released = true;
            put(
                db,
                "compiler_pool",
                &ownership.pool_id,
                &ownership.project_id,
                &ownership,
            )?;
            event(
                db,
                Some(&ownership.project_id),
                "compiler_pool_reconciled",
                value(&ownership)?,
            )?;
        }
    }
    Ok(())
}

impl Store {
    pub fn compiler_pools(&self, principal: &Principal) -> Result<Vec<PoolOwnership>> {
        admin(principal)?;
        let db = self.lock()?;
        all(&db, "compiler_pool")
    }
}

fn admit_service(
    db: &Connection,
    task: &TaskRecord,
    attempt: &AttemptRecord,
    operation: &Value,
) -> Result<()> {
    let pool_id = operation["pool"]["pool_id"]
        .as_str()
        .ok_or_else(|| invalid("compiler service pool identity missing"))?;
    identifier(pool_id)?;
    let worker: WorkerRecord = get(db, "worker", &attempt.worker_id)?;
    if worker.registration.os != "linux" {
        return Err(invalid(
            "compiler services require enrolled Linux environments",
        ));
    }
    let is_server = operation["role"].as_str() == Some("server");
    if !is_server && operation["role"].as_str() != Some("scheduler") {
        return Err(invalid("compiler service role is invalid"));
    }
    if is_server && operation["server_id"].as_str() != Some(worker.id.as_str()) {
        return Err(invalid(
            "compiler service server must be the admitted worker",
        ));
    }
    for ownership in all::<PoolOwnership>(db, "compiler_pool")? {
        if !ownership.released
            && (ownership.pool_id == pool_id
                || (is_server
                    && ownership
                        .physical_hosts
                        .contains(&worker.registration.physical_host_id)))
        {
            return Err(conflict(
                "cannot change service membership before pool reconciliation",
            ));
        }
    }
    for other in all::<AttemptRecord>(db, "attempt")? {
        if other.id == attempt.id {
            continue;
        }
        let reserved: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM reservations WHERE attempt=?1)",
                [&other.id],
                |r| r.get(0),
            )
            .map_err(sql)?;
        if !reserved {
            continue;
        }
        let other_task: TaskRecord = get(db, "task", &other.task_id)?;
        let TaskExecution::Workflow { request } = &other_task.spec.execution else {
            continue;
        };
        let other_operation = &request["operation"];
        if other_operation["kind"].as_str() != Some("compiler_service") {
            continue;
        }
        let other_worker: WorkerRecord = get(db, "worker", &other.worker_id)?;
        let same_role = other_operation["role"] == operation["role"];
        let same_pool = other_operation["pool"]["pool_id"].as_str() == Some(pool_id);
        if (is_server
            && other_operation["role"].as_str() == Some("server")
            && other_worker.registration.physical_host_id == worker.registration.physical_host_id)
            || (!is_server && same_role && same_pool)
        {
            return Err(err(
                ErrorCode::CapacityUnavailable,
                "physical compiler allocation or scheduler already reserved; reconcile before reuse",
            ));
        }
        if same_pool
            && (other_task.spec.project_id != task.spec.project_id
                || other_operation["pool"] != operation["pool"])
        {
            return Err(conflict(
                "pool service membership or project changed before drain",
            ));
        }
    }
    Ok(())
}

pub(crate) fn record_telemetry(
    db: &Connection,
    principal: &Principal,
    attempt: &AttemptRecord,
    task: &TaskRecord,
    records: &[Value],
) -> Result<()> {
    if principal.device_id.as_deref() != Some(attempt.worker_id.as_str())
        || records.is_empty()
        || records.len() > 128
    {
        return Err(invalid(
            "compiler telemetry must come from its service worker in bounded batches",
        ));
    }
    let TaskExecution::Workflow { request } = &task.spec.execution else {
        return Err(invalid("telemetry requires a service workflow"));
    };
    if request["operation"]["kind"] != "compiler_service"
        || request["operation"]["role"] != "server"
    {
        return Err(invalid("only compiler servers can attest remote execution"));
    }
    let count: u64 = db
        .query_row(
            "SELECT COUNT(*) FROM records WHERE kind='compiler_telemetry' AND project=?1",
            [&task.spec.project_id],
            |r| r.get(0),
        )
        .map_err(sql)?;
    if count > 100_000 {
        return Err(conflict(
            "compiler telemetry project retention bound reached",
        ));
    }
    for record in records {
        if serde_json::to_vec(&record).map_err(serialization)?.len() > 65536
            || record["schema"] != 1
            || record["engine"] != "sccache-0.10.0-dacs-1"
            || record["kind"] != "server_complete"
        {
            return Err(invalid(
                "unsupported or oversized compiler engine telemetry",
            ));
        }
        let job = record["job_id"]
            .as_str()
            .ok_or_else(|| invalid("compiler job identity missing"))?;
        if job.is_empty() || job.len() > 32 || !job.bytes().all(|c| c.is_ascii_digit()) {
            return Err(invalid("invalid compiler job identity"));
        }
        let outputs = record["outputs"]
            .as_array()
            .ok_or_else(|| invalid("compiler output evidence missing"))?;
        if outputs.len() > 64 {
            return Err(invalid("too many compiler outputs"));
        }
        for output in outputs {
            let digest = output["sha256"]
                .as_str()
                .ok_or_else(|| invalid("output digest missing"))?;
            if digest.len() != 64
                || !digest.bytes().all(|c| c.is_ascii_hexdigit())
                || output["bytes"].as_u64().is_none()
            {
                return Err(invalid("invalid compiler output digest/length"));
            }
        }
        let key = format!("{}:{job}", attempt.id);
        let observation = dacs_protocol::workflow::CompilerTelemetryRecord {
            worker_id: attempt.worker_id.clone(),
            service_attempt_id: attempt.id.clone(),
            record: record.clone(),
        };
        match get::<dacs_protocol::workflow::CompilerTelemetryRecord>(
            db,
            "compiler_telemetry",
            &key,
        ) {
            Ok(existing) if existing.record == observation.record => continue,
            Ok(_) => return Err(conflict("conflicting repeated compiler job observation")),
            Err(error) if error.code == ErrorCode::NotFound => {}
            Err(error) => return Err(error),
        }
        put(
            db,
            "compiler_telemetry",
            &key,
            &task.spec.project_id,
            &observation,
        )?;
    }
    Ok(())
}

impl Store {
    pub fn compiler_telemetry(
        &self,
        principal: &Principal,
        attempt_id: &str,
    ) -> Result<Vec<dacs_protocol::workflow::CompilerTelemetryRecord>> {
        let db = self.lock()?;
        let attempt: AttemptRecord = get(&db, "attempt", attempt_id)?;
        if !principal.is_admin && principal.device_id.as_deref() != Some(attempt.worker_id.as_str())
        {
            return Err(err(
                ErrorCode::Forbidden,
                "compiler evidence belongs to another build worker",
            ));
        }
        let ownership = all::<PoolOwnership>(&db, "compiler_pool")?
            .into_iter()
            .find(|pool| pool.holder_attempt_id == attempt_id)
            .ok_or_else(|| conflict("build does not hold a compiler pool"))?;
        Ok(
            all::<dacs_protocol::workflow::CompilerTelemetryRecord>(&db, "compiler_telemetry")?
                .into_iter()
                .filter(|record| {
                    ownership
                        .service_attempt_ids
                        .contains(&record.service_attempt_id)
                })
                .collect(),
        )
    }
}
