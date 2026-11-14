use super::*;
use std::collections::HashSet;

pub(crate) fn terminal(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Accepted | TaskState::Cancelled | TaskState::Failed
    )
}
fn resources_valid(r: &Resources) -> Result<()> {
    if r.agent_slots > 1024
        || r.cpu_millis > 1_024_000
        || r.memory_mb > 1_000_000_000
        || r.disk_mb > 1_000_000_000
    {
        return Err(invalid("resource request exceeds operational bounds"));
    }
    Ok(())
}
pub(crate) fn fits(used: &Resources, additional: &Resources, capacity: &Resources) -> bool {
    used.agent_slots
        .checked_add(additional.agent_slots)
        .is_some_and(|v| v <= capacity.agent_slots)
        && used
            .cpu_millis
            .checked_add(additional.cpu_millis)
            .is_some_and(|v| v <= capacity.cpu_millis)
        && used
            .memory_mb
            .checked_add(additional.memory_mb)
            .is_some_and(|v| v <= capacity.memory_mb)
        && used
            .disk_mb
            .checked_add(additional.disk_mb)
            .is_some_and(|v| v <= capacity.disk_mb)
}
fn add(a: &mut Resources, b: &Resources) -> Result<()> {
    a.agent_slots = a
        .agent_slots
        .checked_add(b.agent_slots)
        .ok_or_else(|| conflict("reservation overflow"))?;
    a.cpu_millis = a
        .cpu_millis
        .checked_add(b.cpu_millis)
        .ok_or_else(|| conflict("reservation overflow"))?;
    a.memory_mb = a
        .memory_mb
        .checked_add(b.memory_mb)
        .ok_or_else(|| conflict("reservation overflow"))?;
    a.disk_mb = a
        .disk_mb
        .checked_add(b.disk_mb)
        .ok_or_else(|| conflict("reservation overflow"))?;
    Ok(())
}
fn reserved(db: &Connection, column: &str, key: &str) -> Result<Resources> {
    let query = match column {
        "worker" => "SELECT resources FROM reservations WHERE worker=?1",
        _ => "SELECT resources FROM reservations WHERE physical_host=?1",
    };
    let mut stmt = db.prepare(query).map_err(sql)?;
    let rows = stmt
        .query_map([key], |r| r.get::<_, String>(0))
        .map_err(sql)?;
    let mut total = Resources::default();
    for row in rows {
        add(
            &mut total,
            &serde_json::from_str::<Resources>(&row.map_err(sql)?).map_err(serialization)?,
        )?;
    }
    Ok(total)
}
pub(crate) fn release(db: &Connection, attempt: &AttemptRecord) -> Result<bool> {
    let task: TaskRecord = get(db, "task", &attempt.task_id)?;
    if !attempt.pending_children.is_empty()
        && !terminal(task.state)
        && !matches!(attempt.state.as_str(), "fenced" | "abandoned")
    {
        // A stopped/settled RPC process is not the end of a parent's durable wait.
        return Ok(false);
    }
    db.execute("DELETE FROM reservations WHERE attempt=?1", [&attempt.id])
        .map_err(sql)?;
    let mut w: WorkerRecord = get(db, "worker", &attempt.worker_id)?;
    w.reserved = reserved(db, "worker", &w.id)?;
    put(db, "worker", &w.id, &w.project_id, &w)?;
    Ok(true)
}

pub(crate) fn result_eligible(db: &Connection, a: &AttemptRecord, t: &TaskRecord) -> Result<bool> {
    if a.lease_expires_at > now() {
        return Ok(true);
    }
    if t.spec.effect_class != EffectClass::External || !a.settled {
        return Ok(false);
    }
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM records WHERE kind='execution_stopped' AND id=?1)",
        [&a.id],
        |r| r.get(0),
    )
    .map_err(sql)
}

pub(crate) fn reconcile_stop(
    db: &Connection,
    a: &mut AttemptRecord,
    t: &TaskRecord,
) -> Result<bool> {
    put(db, "execution_stopped", &a.id, &t.spec.project_id, &now())?;
    crate::builds::reconcile_stopped(db, a)?;
    let released = release(db, a)?;
    if !released
        && !matches!(
            a.state.as_str(),
            "fenced" | "abandoned" | "failed" | "cost_capped"
        )
    {
        a.state = "waiting".into();
    } else if !matches!(
        a.state.as_str(),
        "fenced" | "abandoned" | "failed" | "accepted" | "result_submitted" | "cost_capped"
    ) {
        a.state = "stopped".into();
    }
    a.settled = true;
    put(db, "attempt", &a.id, &t.spec.project_id, a)?;
    finalize_cancellation(db, &t.id)?;
    Ok(released)
}

pub(crate) fn resume_waiting(db: &Connection, a: &mut AttemptRecord, t: &TaskRecord) -> Result<()> {
    let reserved: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM reservations WHERE attempt=?1)",
            [&a.id],
            |r| r.get(0),
        )
        .map_err(sql)?;
    if !reserved || a.started_at + t.spec.max_runtime_seconds as i64 <= now() {
        return Err(conflict(
            "waiting attempt has no resumable reservation or exceeded runtime",
        ));
    }
    db.execute(
        "DELETE FROM records WHERE kind='execution_stopped' AND id=?1",
        [&a.id],
    )
    .map_err(sql)?;
    a.state = "executing".into();
    a.settled = false;
    a.lease_expires_at =
        (now() + t.spec.lease_seconds as i64).min(a.started_at + t.spec.max_runtime_seconds as i64);
    Ok(())
}

pub(crate) fn finalize_cancellation(db: &Connection, task_id: &str) -> Result<()> {
    let mut t: TaskRecord = get(db, "task", task_id)?;
    if !t.cancel_requested || terminal(t.state) || t.state == TaskState::Publishing {
        return Ok(());
    }
    if all::<TaskRecord>(db, "task")?
        .iter()
        .any(|child| child.parent_task_id.as_deref() == Some(task_id) && !terminal(child.state))
    {
        return Ok(());
    }
    for a in all::<AttemptRecord>(db, "attempt")?
        .iter()
        .filter(|a| a.task_id == task_id)
    {
        let held: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM reservations WHERE attempt=?1)",
                [&a.id],
                |r| r.get(0),
            )
            .map_err(sql)?;
        if held || !a.pending_children.is_empty() {
            return Ok(());
        }
    }
    t.state = TaskState::Cancelled;
    t.blocked_reason = Some("cancellation completed after owned execution reconciliation".into());
    put(db, "task", &t.id, &t.spec.project_id, &t)?;
    notify_parent(db, &t)?;
    Ok(())
}
pub(crate) fn validate_spec(
    db: &Connection,
    spec: &TaskSpec,
    parent: Option<&TaskRecord>,
) -> Result<()> {
    bounded(&spec.title, 512, "task title")?;
    bounded(&spec.instructions, 131072, "task instructions")?;
    sha(&spec.source_commit)?;
    resources_valid(&spec.resources)?;
    if spec.resources.agent_slots == 0 && spec.resources.cpu_millis == 0 {
        return Err(invalid("task must reserve agent or CPU capacity"));
    }
    if spec
        .required_capabilities
        .iter()
        .any(|cap| cap == "desktop")
        && (spec.effect_class != EffectClass::External || spec.resources.agent_slots == 0)
    {
        return Err(invalid(
            "desktop tasks require external effects and an agent reservation",
        ));
    }
    if let TaskExecution::Workflow { request } = &spec.execution {
        if spec.effect_class != EffectClass::External || spec.resources.cpu_millis == 0 {
            return Err(invalid(
                "workflow execution requires external effects and CPU reservation",
            ));
        }
        if !request.is_object() {
            return Err(invalid("workflow request must be a typed object"));
        }
    }
    if spec.lease_seconds < 1
        || spec.lease_seconds > 86400
        || spec.max_runtime_seconds < spec.lease_seconds
        || spec.max_runtime_seconds > 604800
        || spec.max_retries > 100
    {
        return Err(invalid("invalid lease, runtime, or retry bound"));
    }
    if spec.dependencies.len() > 256 || spec.required_capabilities.len() > 128 {
        return Err(invalid("task graph or capability limit exceeded"));
    }
    let p: Project = get(db, "project", &spec.project_id)?;
    let deferred: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM deferred WHERE project=?1)",
            [&p.id],
            |r| r.get(0),
        )
        .map_err(sql)?;
    if deferred {
        return Err(conflict(
            "new work blocked pending project invalidation reconciliation",
        ));
    }
    if p.contract_revision != spec.contract_revision || p.policy_revision != spec.policy_revision {
        return Err(conflict("task pins stale project revisions"));
    }
    let tasks: Vec<TaskRecord> = all(db, "task")?;
    if tasks.iter().filter(|t| t.spec.project_id == p.id).count() >= p.max_tasks as usize {
        return Err(conflict("project task limit reached"));
    }
    let mut unique = HashSet::new();
    for dep in &spec.dependencies {
        identifier(dep)?;
        if !unique.insert(dep) {
            return Err(invalid("duplicate dependency"));
        }
        let t: TaskRecord = get(db, "task", dep)?;
        if t.spec.project_id != p.id {
            return Err(invalid("cross-project dependency"));
        }
        if matches!(t.state, TaskState::Failed | TaskState::Cancelled) {
            return Err(conflict("dependency cannot complete"));
        }
        // Every insertion references existing nodes, so dependency-only cycles cannot be introduced.
        // A parent's completion also waits on its children; dependencies on ancestors are cycles.
        let mut ancestor = parent.cloned();
        let mut visited = HashSet::new();
        while let Some(a) = ancestor {
            if !visited.insert(a.id.clone()) || a.id == *dep {
                return Err(invalid("delegation dependency cycle"));
            }
            ancestor = a
                .parent_task_id
                .as_deref()
                .map(|key| get(db, "task", key))
                .transpose()?;
        }
    }
    for cap in &spec.required_capabilities {
        identifier(cap)?;
    }
    let mut depth = 0;
    let mut ancestor = parent.cloned();
    let mut visited = HashSet::new();
    while let Some(a) = ancestor {
        depth += 1;
        if depth > p.max_depth || !visited.insert(a.id.clone()) {
            return Err(invalid("delegation depth exceeded or cycle detected"));
        }
        ancestor = a
            .parent_task_id
            .as_deref()
            .map(|key| get(db, "task", key))
            .transpose()?;
    }
    Ok(())
}
pub(crate) fn create_task(
    db: &Connection,
    spec: TaskSpec,
    parent: Option<&TaskRecord>,
) -> Result<TaskRecord> {
    validate_spec(db, &spec, parent)?;
    let ready = spec.dependencies.iter().try_fold(true, |ready, id| {
        Ok::<_, ApiError>(ready && get::<TaskRecord>(db, "task", id)?.state == TaskState::Accepted)
    })?;
    let t = TaskRecord {
        id: id(),
        spec,
        state: if ready {
            TaskState::Ready
        } else {
            TaskState::Waiting
        },
        generation: 0,
        parent_task_id: parent.map(|p| p.id.clone()),
        attempt_id: None,
        integrated_sha: None,
        workflow_result: None,
        blocked_reason: if ready {
            None
        } else {
            Some("waiting for accepted dependencies".into())
        },
        cancel_requested: false,
    };
    put(db, "task", &t.id, &t.spec.project_id, &t)?;
    event(db, Some(&t.spec.project_id), "task_created", value(&t)?)?;
    Ok(t)
}
pub(crate) fn admit(db: &Connection, task_id: &str, chosen: Option<&str>) -> Result<AttemptRecord> {
    let mut t: TaskRecord = get(db, "task", task_id)?;
    if !matches!(t.state, TaskState::Ready) || t.cancel_requested {
        return Err(conflict("task is not ready for admission"));
    }
    let p: Project = get(db, "project", &t.spec.project_id)?;
    let deferred: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM deferred WHERE project=?1)",
            [&p.id],
            |r| r.get(0),
        )
        .map_err(sql)?;
    if deferred {
        return Err(conflict(
            "admission blocked pending project invalidation reconciliation",
        ));
    }
    if t.spec.contract_revision != p.contract_revision
        || t.spec.policy_revision != p.policy_revision
    {
        return Err(conflict("task revisions invalidated"));
    }
    if t.generation > u64::from(t.spec.max_retries) {
        return Err(conflict("task retry bound reached"));
    }
    let workers: Vec<WorkerRecord> = all(db, "worker")?;
    let desktop = t
        .spec
        .required_capabilities
        .iter()
        .any(|cap| cap == "desktop");
    let mut selected = None;
    for w in &workers {
        if chosen.is_some_and(|id| id != w.id)
            || w.project_id != p.id
            || !w.approved
            || w.revoked
            || w.draining
            || now() - w.last_seen > 90
            || w.registration.policy_revision != p.policy_revision
        {
            continue;
        }
        if !t
            .spec
            .required_capabilities
            .iter()
            .all(|cap| w.registration.capabilities.contains(cap))
        {
            continue;
        }
        if t.spec.resources.agent_slots > 0 || desktop {
            match get::<WorkerRuntimeStatus>(db, "runtime", &w.id) {
                Ok(status)
                    if status.provider_authenticated == Some(true)
                        && now() - status.observed_at <= 90
                        && (!desktop
                            || (status.desktop_session.is_some()
                                && !status.desktop_paused
                                && !status.desktop_active)) => {}
                Ok(_) => continue,
                Err(e) if e.code == ErrorCode::NotFound => continue,
                Err(e) => return Err(e),
            }
        }
        if desktop {
            // Observation alone cannot fence two admissions before the next heartbeat.
            let mut query = db
                .prepare("SELECT attempt FROM reservations WHERE physical_host=?1")
                .map_err(sql)?;
            let held = query
                .query_map([&w.registration.physical_host_id], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(sql)?;
            let mut occupied = false;
            for held in held {
                let attempt: AttemptRecord = get(db, "attempt", &held.map_err(sql)?)?;
                let task: TaskRecord = get(db, "task", &attempt.task_id)?;
                if task
                    .spec
                    .required_capabilities
                    .iter()
                    .any(|cap| cap == "desktop")
                {
                    occupied = true;
                    break;
                }
            }
            if occupied {
                continue;
            }
        }
        let own = reserved(db, "worker", &w.id)?;
        if !fits(&own, &t.spec.resources, &w.registration.capacity) {
            continue;
        }
        // Multiple agents/VMs declaring one physical host never multiply its budget.
        let mut physical_cap = w.registration.capacity.clone();
        for other in workers.iter().filter(|o| {
            o.approved
                && !o.revoked
                && o.registration.physical_host_id == w.registration.physical_host_id
        }) {
            physical_cap.agent_slots = physical_cap
                .agent_slots
                .min(other.registration.capacity.agent_slots);
            physical_cap.cpu_millis = physical_cap
                .cpu_millis
                .min(other.registration.capacity.cpu_millis);
            physical_cap.memory_mb = physical_cap
                .memory_mb
                .min(other.registration.capacity.memory_mb);
            physical_cap.disk_mb = physical_cap
                .disk_mb
                .min(other.registration.capacity.disk_mb);
        }
        if fits(
            &reserved(db, "physical_host", &w.registration.physical_host_id)?,
            &t.spec.resources,
            &physical_cap,
        ) {
            selected = Some(w.clone());
            break;
        }
    }
    let mut w = selected.ok_or_else(|| {
        err(
            ErrorCode::CapacityUnavailable,
            "no approved ready worker has physical-host capacity and capabilities",
        )
    })?;
    t.generation = t
        .generation
        .checked_add(1)
        .ok_or_else(|| conflict("generation exhausted"))?;
    let a = AttemptRecord {
        id: id(),
        task_id: t.id.clone(),
        worker_id: w.id.clone(),
        generation: t.generation,
        state: "admitted".into(),
        lease_expires_at: now() + t.spec.lease_seconds as i64,
        started_at: now(),
        settled: false,
        pending_children: Vec::new(),
        last_sequence: 0,
        stop_reason: None,
        resources: t.spec.resources.clone(),
    };
    db.execute(
        "INSERT INTO reservations(attempt,worker,physical_host,resources) VALUES(?1,?2,?3,?4)",
        params![
            a.id,
            w.id,
            w.registration.physical_host_id,
            serde_json::to_string(&a.resources).map_err(serialization)?
        ],
    )
    .map_err(sql)?;
    w.reserved = reserved(db, "worker", &w.id)?;
    put(db, "worker", &w.id, &w.project_id, &w)?;
    t.state = TaskState::Executing;
    t.attempt_id = Some(a.id.clone());
    t.blocked_reason = None;
    put(db, "attempt", &a.id, &p.id, &a)?;
    put(db, "task", &t.id, &p.id, &t)?;
    crate::builds::acquire_for_workflow(db, &t, &a)?;
    message(
        db,
        &w.id,
        Some(&a),
        "assignment",
        json!({"task":t,"attempt":a}),
        None,
    )?;
    event(db, Some(&p.id), "attempt_admitted", value(&a)?)?;
    Ok(a)
}
pub(crate) fn owned_attempt(
    db: &Connection,
    p: &Principal,
    attempt_id: &str,
    generation: u64,
) -> Result<(AttemptRecord, TaskRecord)> {
    let a: AttemptRecord = get(db, "attempt", attempt_id)?;
    let t: TaskRecord = get(db, "task", &a.task_id)?;
    project(db, p, &t.spec.project_id)?;
    if !p.is_admin && p.device_id.as_deref() != Some(&a.worker_id) {
        return Err(err(
            ErrorCode::Forbidden,
            "attempt belongs to another worker",
        ));
    }
    if a.generation != generation {
        return Err(conflict("attempt generation does not match"));
    }
    Ok((a, t))
}

pub(crate) fn current(
    db: &Connection,
    p: &Principal,
    attempt_id: &str,
    generation: u64,
) -> Result<(AttemptRecord, TaskRecord)> {
    let (a, t) = owned_attempt(db, p, attempt_id, generation)?;
    if t.generation != generation
        || t.attempt_id.as_deref() != Some(&a.id)
        || matches!(a.state.as_str(), "fenced" | "abandoned")
        || terminal(t.state)
        || t.cancel_requested
    {
        return Err(conflict("stale or terminal attempt generation"));
    }
    Ok((a, t))
}
pub(crate) fn notify_parent(db: &Connection, t: &TaskRecord) -> Result<()> {
    if let Some(parent) = &t.parent_task_id {
        let parent: TaskRecord = get(db, "task", parent)?;
        // Notify the attempt that actually owns the durable pending child, not a replacement.
        for mut a in all::<AttemptRecord>(db, "attempt")?
            .into_iter()
            .filter(|a| a.task_id == parent.id && a.pending_children.contains(&t.id))
        {
            a.pending_children.retain(|c| c != &t.id);
            put(db, "attempt", &a.id, &parent.spec.project_id, &a)?;
            message(
                db,
                &a.worker_id,
                Some(&a),
                "child_result",
                json!({"task_id":t.id,"state":t.state,"integrated_sha":t.integrated_sha,"workflow_result":t.workflow_result}),
                None,
            )?;
        }
        finalize_cancellation(db, &parent.id)?;
    }
    Ok(())
}
pub(crate) fn refresh_dependencies(db: &Connection) -> Result<()> {
    loop {
        let mut changed = false;
        for mut t in all::<TaskRecord>(db, "task")? {
            if t.state != TaskState::Waiting {
                continue;
            }
            let deps = t
                .spec
                .dependencies
                .iter()
                .map(|id| get::<TaskRecord>(db, "task", id))
                .collect::<Result<Vec<_>>>()?;
            if deps
                .iter()
                .any(|d| matches!(d.state, TaskState::Failed | TaskState::Cancelled))
            {
                t.state = TaskState::Failed;
                t.blocked_reason = Some("dependency did not produce an accepted result".into());
                notify_parent(db, &t)?;
                changed = true;
            } else if deps.iter().all(|d| d.state == TaskState::Accepted) {
                t.state = TaskState::Ready;
                t.blocked_reason = None;
                changed = true;
            } else {
                continue;
            }
            put(db, "task", &t.id, &t.spec.project_id, &t)?;
        }
        if !changed {
            break;
        }
    }
    Ok(())
}
pub(crate) fn cancel(db: &Connection, t: &mut TaskRecord, reason: &str) -> Result<()> {
    if terminal(t.state) {
        return Ok(());
    }
    t.cancel_requested = true;
    if t.state == TaskState::Publishing {
        t.blocked_reason = Some("cancellation deferred until publication reconciliation".into());
        put(db, "task", &t.id, &t.spec.project_id, t)?;
        return Ok(());
    }
    if let Some(aid) = &t.attempt_id {
        let mut a: AttemptRecord = get(db, "attempt", aid)?;
        a.state = "fenced".into();
        a.stop_reason = Some(reason.into());
        put(db, "attempt", &a.id, &t.spec.project_id, &a)?;
        message(
            db,
            &a.worker_id,
            Some(&a),
            "stop",
            json!({"reason":reason}),
            None,
        )?;
    }
    t.blocked_reason = Some(format!(
        "{reason}; awaiting owned execution and child reconciliation"
    ));
    put(db, "task", &t.id, &t.spec.project_id, t)?;
    for mut child in all::<TaskRecord>(db, "task")?
        .into_iter()
        .filter(|c| c.parent_task_id.as_deref() == Some(&t.id))
    {
        cancel(db, &mut child, "parent cancelled")?;
    }
    finalize_cancellation(db, &t.id)?;
    *t = get(db, "task", &t.id)?;
    refresh_dependencies(db)
}
pub(crate) fn invalidate(db: &Connection, project_id: &str, policy: bool) -> Result<()> {
    for mut t in all::<TaskRecord>(db, "task")?
        .into_iter()
        .filter(|t| t.spec.project_id == project_id && !terminal(t.state))
    {
        cancel(
            db,
            &mut t,
            if policy {
                "project policy invalidated"
            } else {
                "project contract invalidated"
            },
        )?;
    }
    Ok(())
}
pub(crate) fn maintain(db: &Connection) -> Result<()> {
    for mut t in all::<TaskRecord>(db, "task")? {
        if t.cancel_requested {
            finalize_cancellation(db, &t.id)?;
            continue;
        }
        if !matches!(t.state, TaskState::Executing | TaskState::Validating) {
            continue;
        }
        let Some(aid) = &t.attempt_id else {
            continue;
        };
        let mut a: AttemptRecord = get(db, "attempt", aid)?;
        if a.lease_expires_at > now() {
            continue;
        }
        if a.state == "cost_capped" {
            t.blocked_reason =
                Some("OMP cost cap reached; explicit owner disposition required".into());
            put(db, "task", &t.id, &t.spec.project_id, &t)?;
            continue;
        }
        if t.spec.effect_class == EffectClass::External {
            t.blocked_reason =
                Some("effectful lease expired: reconcile original completion or explicitly abandon before retry".into());
            put(db, "task", &t.id, &t.spec.project_id, &t)?;
            continue;
        }
        if !a.pending_children.is_empty() {
            t.blocked_reason = Some(
                "lease expired with pending children: reconcile or abandon before replacement"
                    .into(),
            );
            put(db, "task", &t.id, &t.spec.project_id, &t)?;
            continue;
        }
        a.state = "fenced".into();
        a.stop_reason = Some("workspace lease expired".into());
        put(db, "attempt", &a.id, &t.spec.project_id, &a)?;
        message(
            db,
            &a.worker_id,
            Some(&a),
            "stop",
            json!({"reason":"lease expired","reservation_held_until_reconciliation":true}),
            None,
        )?;
        t.state = if t.generation <= u64::from(t.spec.max_retries) {
            TaskState::Ready
        } else {
            TaskState::Failed
        };
        t.attempt_id = None;
        t.blocked_reason =
            Some("expired attempt fenced; unreachable reservation remains held".into());
        put(db, "task", &t.id, &t.spec.project_id, &t)?;
        if terminal(t.state) {
            notify_parent(db, &t)?;
        }
        event(
            db,
            Some(&t.spec.project_id),
            "lease_fenced",
            json!({"attempt_id":a.id,"task_id":t.id}),
        )?;
    }
    refresh_dependencies(db)
}
