use super::*;
use crate::scheduling as sched;

pub(crate) fn validate_registration(r: &WorkerRegistration) -> Result<()> {
    bounded(&r.name, 128, "worker name")?;
    identifier(&r.physical_host_id)?;
    identifier(&r.os)?;
    identifier(&r.architecture)?;
    bounded(&r.omp_version, 128, "pinned OMP version")?;
    bounded(&r.consent_host, 256, "consent host")?;
    if r.capabilities.len() > 128
        || r.capacity.agent_slots > 1024
        || r.capacity.cpu_millis > 1_024_000
        || r.capacity.memory_mb > 1_000_000_000
        || r.capacity.disk_mb > 1_000_000_000
    {
        return Err(invalid("registration exceeds limits"));
    }
    let mut caps = std::collections::HashSet::new();
    for cap in &r.capabilities {
        identifier(cap)?;
        if !caps.insert(cap) {
            return Err(invalid("duplicate capability"));
        }
    }
    Ok(())
}
fn branch(name: &str) -> Result<String> {
    bounded(name, 240, "target branch")?;
    let name = name.strip_prefix("refs/heads/").unwrap_or(name);
    if name.is_empty()
        || name.starts_with('/')
        || name.ends_with('/')
        || name.contains("..")
        || name.contains("@{")
        || name == "@"
        || name
            .bytes()
            .any(|b| b <= 32 || b == 127 || b"~^:?*[\\".contains(&b))
        || name
            .split('/')
            .any(|p| p.is_empty() || p.starts_with('.') || p.ends_with('.') || p.ends_with(".lock"))
    {
        return Err(invalid("invalid Git target branch"));
    }
    Ok(format!("refs/heads/{name}"))
}
fn publishing(db: &Connection, project_id: &str) -> Result<bool> {
    Ok(all::<Publication>(db, "publication")?.iter().any(|p| {
        p.project_id == project_id && matches!(p.state.as_str(), "pending" | "reconciling")
    }))
}
fn evidence(checks: &[CheckEvidence], expected: &str) -> Result<()> {
    if checks.len() > 256 {
        return Err(invalid("too many checks"));
    }
    for check in checks {
        bounded(&check.name, 256, "check name")?;
        bounded(&check.command, 4096, "check command")?;
        sha(&check.source_sha)?;
        if check.source_sha != expected {
            return Err(conflict(
                "check evidence is not bound to exact candidate SHA",
            ));
        }
        if let Some(d) = &check.artifact_digest {
            if d.len() != 64
                || !d
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            {
                return Err(invalid("invalid artifact digest"));
            }
        }
    }
    Ok(())
}
pub(crate) fn execute(db: &Connection, p: &Principal, command: Command) -> Result<Value> {
    match command {
        Command::ReportRuntimeStatus { mut status } => {
            let w = worker(db, p, true)?;
            if let Some(session) = &status.desktop_session {
                bounded(session, 256, "desktop session")?;
            }
            if status.desktop_active && status.desktop_paused {
                return Err(invalid("paused desktop cannot simultaneously be active"));
            }
            status.worker_id = w.id.clone();
            status.observed_at = now();
            put(db, "runtime", &w.id, &w.project_id, &status)?;
            event(db, Some(&w.project_id), "runtime_status", value(&status)?)?;
            value(&status)
        }
        Command::RequestDesktopPause { worker_id } => {
            let w: WorkerRecord = get(db, "worker", &worker_id)?;
            project(db, p, &w.project_id)?;
            if !p.is_admin && p.device_id.as_deref() != Some(&w.id) {
                return Err(err(
                    ErrorCode::Forbidden,
                    "only administrator or local device may request desktop pause",
                ));
            }
            if w.revoked {
                return Err(conflict("worker revoked"));
            }
            message(
                db,
                &w.id,
                None,
                "pause_desktop",
                json!({"requested_at":now()}),
                None,
            )?;
            event(
                db,
                Some(&w.project_id),
                "desktop_pause_requested",
                json!({"worker_id":w.id}),
            )?;
            Ok(json!({"queued":true,"worker_id":w.id,"paused":null}))
        }
        Command::CreateProject {
            name,
            target_branch,
            max_tasks,
            max_depth,
        } => {
            admin(p)?;
            bounded(&name, 256, "project name")?;
            if max_tasks == 0 || max_tasks > 100000 || max_depth == 0 || max_depth > 64 {
                return Err(invalid("invalid project operational limits"));
            }
            let project = Project {
                id: id(),
                name,
                target_branch: branch(&target_branch)?,
                contract_revision: 1,
                policy_revision: 1,
                max_tasks,
                max_depth,
                requirements: json!({}),
            };
            put(db, "project", &project.id, &project.id, &project)?;
            event(db, Some(&project.id), "project_created", value(&project)?)?;
            value(&project)
        }
        Command::SetRequirements {
            project_id,
            requirements,
        } => {
            admin(p)?;
            let mut project = project(db, p, &project_id)?;
            if !requirements.is_object() || requirements.to_string().len() > 262144 {
                return Err(invalid("requirements must be a bounded JSON object"));
            }
            if publishing(db, &project_id)? {
                db.execute("INSERT INTO deferred(project,requirements) VALUES(?1,?2) ON CONFLICT(project) DO UPDATE SET requirements=excluded.requirements",params![project_id,requirements.to_string()]).map_err(sql)?;
                return Ok(json!({"deferred":true,"reason":"publication reconciliation required"}));
            }
            project.requirements = requirements;
            project.policy_revision = project
                .policy_revision
                .checked_add(1)
                .ok_or_else(|| conflict("policy revision exhausted"))?;
            put(db, "project", &project.id, &project.id, &project)?;
            sched::invalidate(db, &project_id, true)?;
            event(
                db,
                Some(&project_id),
                "requirements_changed",
                value(&project)?,
            )?;
            value(&project)
        }
        Command::CreateInvitation {
            project_id,
            host_name,
            expires_in_seconds,
        } => {
            admin(p)?;
            let project = project(db, p, &project_id)?;
            bounded(&host_name, 256, "inviting host")?;
            if expires_in_seconds == 0 || expires_in_seconds > 2592000 {
                return Err(invalid("invitation expiry must be within 30 days"));
            }
            let secret = token();
            let invitation = Invitation {
                token: String::new(),
                project_id,
                host_name,
                expires_at: now() + expires_in_seconds as i64,
                requirements: project.requirements,
                revoked: false,
            };
            put(
                db,
                "invitation",
                &hash(&secret),
                &invitation.project_id,
                &invitation,
            )?;
            event(
                db,
                Some(&invitation.project_id),
                "invitation_created",
                json!({"expires_at":invitation.expires_at,"host_name":invitation.host_name}),
            )?;
            value(&Invitation {
                token: secret,
                ..invitation
            })
        }
        Command::RevokeInvitation { token } => {
            admin(p)?;
            let mut i: Invitation = get(db, "invitation", &hash(&token))?;
            i.revoked = true;
            put(db, "invitation", &hash(token), &i.project_id, &i)?;
            event(db, Some(&i.project_id), "invitation_revoked", json!({}))?;
            Ok(json!({"revoked":true}))
        }
        Command::ApproveWorker { worker_id } => {
            admin(p)?;
            let mut w: WorkerRecord = get(db, "worker", &worker_id)?;
            let project: Project = get(db, "project", &w.project_id)?;
            if w.revoked
                || !w.registration.owner_consented
                || w.registration.policy_revision != project.policy_revision
            {
                return Err(conflict("revoked device or stale local consent"));
            }
            if !w.approved { w.draining = false; }
            w.approved = true;
            put(db, "worker", &w.id, &w.project_id, &w)?;
            event(db, Some(&w.project_id), "worker_approved", value(&w)?)?;
            value(&w)
        }
        Command::RevokeWorker { worker_id } => {
            admin(p)?;
            let mut w: WorkerRecord = get(db, "worker", &worker_id)?;
            w.revoked = true;
            w.draining = true;
            put(db, "worker", &w.id, &w.project_id, &w)?;
            for mut t in all::<TaskRecord>(db, "task")? {
                if let Some(aid) = &t.attempt_id {
                    let a: AttemptRecord = get(db, "attempt", aid)?;
                    if a.worker_id == w.id {
                        sched::cancel(db, &mut t, "worker revoked")?;
                    }
                }
            }
            event(
                db,
                Some(&w.project_id),
                "worker_revoked",
                json!({"worker_id":w.id,"reservations":"held_pending_reconciliation"}),
            )?;
            value(&w)
        }
        Command::SetDraining {
            worker_id,
            draining,
        } => {
            let mut w: WorkerRecord = get(db, "worker", &worker_id)?;
            project(db, p, &w.project_id)?;
            if !p.is_admin && p.device_id.as_deref() != Some(&w.id) {
                return Err(err(
                    ErrorCode::Forbidden,
                    "only device owner may change local drain state",
                ));
            }
            if w.revoked || !w.approved {
                return Err(conflict("worker is not approved"));
            }
            // A coordinator may reduce availability, never override a local pause.
            if p.is_admin && !draining {
                return Err(err(
                    ErrorCode::Forbidden,
                    "resuming requires local device action",
                ));
            }
            w.draining = draining;
            put(db, "worker", &w.id, &w.project_id, &w)?;
            event(db, Some(&w.project_id), "worker_draining", value(&w)?)?;
            value(&w)
        }
        Command::Heartbeat { registration } => {
            let mut w = worker(db, p, false)?;
            validate_registration(&registration)?;
            if registration.physical_host_id != w.registration.physical_host_id
                || registration.os != w.registration.os
                || registration.architecture != w.registration.architecture
                || registration.consent_host != w.registration.consent_host
                || registration.policy_revision != w.registration.policy_revision
                || !registration.owner_consented
            {
                return Err(err(
                    ErrorCode::Forbidden,
                    "heartbeat cannot replace owner consent or identity",
                ));
            }
            if !registration
                .capabilities
                .iter()
                .all(|c| w.registration.capabilities.contains(c))
                || !sched::fits(
                    &Resources::default(),
                    &registration.capacity,
                    &w.registration.capacity,
                )
            {
                return Err(err(
                    ErrorCode::Forbidden,
                    "heartbeat cannot widen approved capabilities or capacity",
                ));
            }
            if registration.capabilities != w.registration.capabilities
                || registration.capacity != w.registration.capacity
            {
                w.draining = true;
            }
            w.registration = registration;
            w.last_seen = now();
            put(db, "worker", &w.id, &w.project_id, &w)?;
            for mut a in all::<AttemptRecord>(db, "attempt")?
                .into_iter()
                .filter(|a| a.worker_id == w.id)
            {
                let t: TaskRecord = get(db, "task", &a.task_id)?;
                if t.attempt_id.as_deref() == Some(&a.id)
                    && matches!(t.state, TaskState::Executing | TaskState::Validating)
                    && !matches!(
                        a.state.as_str(),
                        "fenced" | "abandoned" | "stopped" | "cost_capped"
                    )
                    && a.lease_expires_at > now()
                {
                    a.lease_expires_at = (now() + t.spec.lease_seconds as i64)
                        .min(a.started_at + t.spec.max_runtime_seconds as i64);
                    put(db, "attempt", &a.id, &w.project_id, &a)?;
                }
            }
            value(&w)
        }
        Command::CreateTask { spec } => {
            admin(p)?;
            project(db, p, &spec.project_id)?;
            value(&sched::create_task(db, spec, None)?)
        }
        Command::AdmitTask { task_id, worker_id } => {
            admin(p)?;
            value(&sched::admit(db, &task_id, worker_id.as_deref())?)
        }
        Command::Delegate {
            parent_attempt_id,
            generation,
            spec,
        } => {
            worker(db, p, true)?;
            let (mut a, t) = sched::current(db, p, &parent_attempt_id, generation)?;
            if t.spec.effect_class == EffectClass::Workspace && spec.effect_class == EffectClass::External {
                return Err(err(ErrorCode::Forbidden, "replayable workspace parent cannot delegate external effects"));
            }
            if a.state == "waiting" { sched::resume_waiting(db, &mut a, &t)?; }
            if t.state != TaskState::Executing
                || a.lease_expires_at <= now()
                || matches!(a.state.as_str(), "cost_capped" | "stopped")
                || spec.project_id != t.spec.project_id
            {
                return Err(conflict("parent cannot admit child in current state"));
            }
            if a.pending_children.len() >= 256 {
                return Err(conflict("pending child limit reached"));
            }
            let child = sched::create_task(db, spec, Some(&t))?;
            // Any refusal rolls back both child insertion and parent pending state.
            let child_attempt = sched::admit(db, &child.id, None)?;
            a.pending_children.push(child.id.clone());
            a.settled = false;
            put(db, "attempt", &a.id, &t.spec.project_id, &a)?;
            let child: TaskRecord = get(db, "task", &child.id)?;
            event(
                db,
                Some(&t.spec.project_id),
                "child_admitted",
                json!({"parent_attempt_id":a.id,"child_task_id":child.id}),
            )?;
            Ok(json!({"admitted":true,"task":child,"attempt":child_attempt}))
        }
        Command::ReportEvent {
            attempt_id,
            generation,
            sequence,
            event: reported,
        } => {
            worker(db, p, true)?;
            let (mut a, mut t) = sched::current(db, p, &attempt_id, generation)?;
            if sequence
                != a.last_sequence
                    .checked_add(1)
                    .ok_or_else(|| conflict("sequence exhausted"))?
            {
                return Err(conflict(
                    "worker event sequence must be strictly contiguous",
                ));
            }
            if t.state == TaskState::Publishing {
                return Err(conflict(
                    "worker events cannot change an unresolved publication",
                ));
            }
            if a.lease_expires_at <= now() && !matches!(reported, WorkerEvent::Stopped)
                && !(a.state == "waiting" && matches!(reported, WorkerEvent::Started | WorkerEvent::Progress { .. }))
            {
                return Err(conflict("attempt lease expired"));
            }
            match &reported {
                WorkerEvent::Started => {
                    if a.state == "waiting" { sched::resume_waiting(db, &mut a, &t)?; }
                    else if a.state == "admitted" { a.state = "executing".into(); a.settled = false; }
                    else { return Err(conflict("attempt already started")); }
                }
                WorkerEvent::Progress { message } => {
                    bounded(message, 16384, "progress message")?;
                    if a.state == "waiting" { sched::resume_waiting(db, &mut a, &t)?; }
                    if !matches!(a.state.as_str(), "admitted" | "executing" | "settled") {
                        return Err(conflict("attempt does not accept progress"));
                    }
                    a.settled = false;
                    a.state = "executing".into();
                }
                WorkerEvent::Usage {
                    input_tokens,
                    output_tokens,
                    cost_usd,
                    estimated,
                } => {
                    if cost_usd.is_some_and(|cost| !cost.is_finite() || cost < 0.0) {
                        return Err(invalid("usage cost must be finite and nonnegative"));
                    }
                    let usage = UsageRecord {
                        attempt_id: a.id.clone(),
                        worker_id: a.worker_id.clone(),
                        input_tokens: *input_tokens,
                        output_tokens: *output_tokens,
                        cost_usd: *cost_usd,
                        estimated: *estimated,
                        at: now(),
                    };
                    put(db, "usage", &a.id, &t.spec.project_id, &usage)?;
                }
                WorkerEvent::Settled => {
                    if !matches!(a.state.as_str(), "admitted" | "executing" | "settled" | "waiting" | "result_submitted") {
                        return Err(conflict("attempt cannot settle in current state"));
                    }
                    a.settled = true;
                    if a.state != "result_submitted" {
                        if a.state != "waiting" { a.state = "settled".into(); }
                        t.blocked_reason = Some(if a.pending_children.is_empty() {
                            "OMP settled; explicit result still required"
                        } else {
                            "OMP settled with unfinished child tasks"
                        }.into());
                    }
                }
                WorkerEvent::CostCapped { message } => {
                    bounded(message, 16384, "cost cap reason")?;
                    a.state = "cost_capped".into();
                    a.stop_reason = Some(message.clone());
                    a.settled = true;
                    t.blocked_reason = Some("OMP cost cap reached; task unfinished".into());
                }
                WorkerEvent::Failed { message } => {
                    bounded(message, 16384, "failure reason")?;
                    a.state = "failed".into();
                    a.stop_reason = Some(message.clone());
                    t.state = TaskState::Failed;
                    t.blocked_reason = Some(message.clone());
                }
                WorkerEvent::Stopped => {
                    let released = sched::reconcile_stop(db, &mut a, &t)?;
                    if !released {
                        t.blocked_reason = Some("worker stopped while waiting; parent reservation retained for resume".into());
                    } else if t.state != TaskState::Validating && a.state != "cost_capped" {
                        t.blocked_reason = Some("worker stopped; explicit result or abandonment required".into());
                    }
                }
            }
            a.last_sequence = sequence;
            put(db, "attempt", &a.id, &t.spec.project_id, &a)?;
            put(db, "task", &t.id, &t.spec.project_id, &t)?;
            if sched::terminal(t.state) {
                sched::notify_parent(db, &t)?;
                sched::refresh_dependencies(db)?;
            }
            event(
                db,
                Some(&t.spec.project_id),
                "worker_event",
                json!({"attempt_id":a.id,"generation":generation,"sequence":sequence,"event":reported}),
            )?;
            value(&a)
        }
        Command::SubmitCandidate { submission } => {
            worker(db, p, true)?;
            let (mut a, mut t) =
                sched::current(db, p, &submission.attempt_id, submission.generation)?;
            if t.id != submission.task_id
                || t.state != TaskState::Executing
                || !sched::result_eligible(db, &a, &t)?
                || !a.pending_children.is_empty()
                || matches!(a.state.as_str(), "failed" | "cost_capped")
            {
                return Err(conflict(
                    "attempt cannot submit a result while fenced, expired, capped, or unfinished",
                ));
            }
            let project: Project = get(db, "project", &t.spec.project_id)?;
            if submission.contract_revision != project.contract_revision
                || submission.policy_revision != project.policy_revision
                || submission.contract_revision != t.spec.contract_revision
                || submission.policy_revision != t.spec.policy_revision
            {
                return Err(conflict("candidate pins stale revisions"));
            }
            sha(&submission.commit_sha)?;
            evidence(&submission.evidence, &submission.commit_sha)?;
            if submission.unresolved.len() > 256 {
                return Err(invalid("too many unresolved items"));
            }
            for item in &submission.unresolved {
                bounded(item, 4096, "unresolved item")?;
            }
            let candidate = CandidateRecord {
                id: id(),
                project_id: project.id,
                submission,
                integration_sha: None,
                expected_head: None,
                decision: None,
                publication_id: None,
            };
            t.state = TaskState::Validating;
            t.blocked_reason = Some("explicit candidate awaiting integration and review".into());
            a.state = "result_submitted".into();
            put(
                db,
                "candidate",
                &candidate.id,
                &candidate.project_id,
                &candidate,
            )?;
            put(db, "task", &t.id, &t.spec.project_id, &t)?;
            put(db, "attempt", &a.id, &t.spec.project_id, &a)?;
            event(
                db,
                Some(&t.spec.project_id),
                "candidate_submitted",
                value(&candidate)?,
            )?;
            value(&candidate)
        }
        Command::ReviewCandidate {
            candidate_id,
            decision,
        } => {
            admin(p)?;
            let mut c: CandidateRecord = get(db, "candidate", &candidate_id)?;
            let (_, t) = sched::current(db, p, &c.submission.attempt_id, c.submission.generation)?;
            if t.state != TaskState::Validating || c.publication_id.is_some() {
                return Err(conflict("candidate is not reviewable"));
            }
            if c.integration_sha.as_deref() != Some(&decision.candidate_sha) {
                return Err(conflict("review must identify the staged integration SHA"));
            }
            bounded(&decision.reason, 16384, "review reason")?;
            evidence(&decision.checks_run, &decision.candidate_sha)?;
            if decision.skipped_checks.len() > 256 {
                return Err(invalid("too many skipped checks"));
            }
            for check in &decision.skipped_checks {
                bounded(check, 4096, "skipped check reason")?;
            }
            if decision.approve
                && (!c.submission.unresolved.is_empty()
                    || decision.checks_run.iter().any(|c| !c.passed)
                    || (decision.checks_run.is_empty() && decision.skipped_checks.is_empty()))
            {
                return Err(conflict(
                    "approval requires passing exact-SHA evidence or explicit skipped-check rationale, and no unresolved items",
                ));
            }
            c.decision = Some(decision);
            put(db, "candidate", &c.id, &c.project_id, &c)?;
            event(db, Some(&c.project_id), "candidate_reviewed", value(&c)?)?;
            value(&c)
        }
        Command::CancelTask { task_id } => {
            admin(p)?;
            let mut t: TaskRecord = get(db, "task", &task_id)?;
            sched::cancel(db, &mut t, "administrator cancelled task")?;
            event(
                db,
                Some(&t.spec.project_id),
                "task_cancellation_requested",
                value(&t)?,
            )?;
            value(&t)
        }
        Command::AbandonAttempt { attempt_id, reason } => {
            admin(p)?;
            bounded(&reason, 16384, "abandonment reason")?;
            let mut a: AttemptRecord = get(db, "attempt", &attempt_id)?;
            let mut t: TaskRecord = get(db, "task", &a.task_id)?;
            if t.state == TaskState::Publishing {
                return Err(conflict("publication must reconcile before abandonment"));
            }
            if t.attempt_id.as_deref() != Some(&a.id) || sched::terminal(t.state) || t.cancel_requested {
                return Err(conflict(
                    "only current unfinished attempts can be abandoned",
                ));
            }
            if !a.pending_children.is_empty() {
                return Err(conflict("cancel and reconcile pending children before abandoning their parent"));
            }
            a.state = "abandoned".into();
            a.stop_reason = Some(reason.clone());
            put(db, "attempt", &a.id, &t.spec.project_id, &a)?;
            message(
                db,
                &a.worker_id,
                Some(&a),
                "stop",
                json!({"reason":reason,"explicit_abandonment":true}),
                None,
            )?;
            t.attempt_id = None;
            t.state = if t.generation <= u64::from(t.spec.max_retries) {
                TaskState::Ready
            } else {
                TaskState::Failed
            };
            t.blocked_reason =
                Some("explicitly abandoned; reservation held until stop reconciliation".into());
            put(db, "task", &t.id, &t.spec.project_id, &t)?;
            if sched::terminal(t.state) {
                sched::notify_parent(db, &t)?;
                sched::refresh_dependencies(db)?;
            }
            event(
                db,
                Some(&t.spec.project_id),
                "attempt_abandoned",
                value(&a)?,
            )?;
            value(&t)
        }
        Command::ReconcileWorker {
            stopped_attempt_ids,
        } => {
            if stopped_attempt_ids.len() > 1024 {
                return Err(invalid("reconciliation batch too large"));
            }
            if !p.is_admin {
                worker(db, p, false)?;
            }
            for aid in &stopped_attempt_ids {
                let mut a: AttemptRecord = get(db, "attempt", aid)?;
                let t: TaskRecord = get(db, "task", &a.task_id)?;
                if !p.is_admin && p.device_id.as_deref() != Some(&a.worker_id) {
                    return Err(err(ErrorCode::Forbidden, "cannot reconcile another worker"));
                }
                if t.state == TaskState::Publishing {
                    return Err(conflict(
                        "reconcile Git publication before releasing reservation",
                    ));
                }
                let released = sched::reconcile_stop(db, &mut a, &t)?;
                sched::refresh_dependencies(db)?;
                event(
                    db,
                    Some(&t.spec.project_id),
                    "worker_stop_reconciled",
                    json!({"attempt_id":a.id,"reservation_released":released}),
                )?;
            }
            Ok(json!({"reconciled":stopped_attempt_ids}))
        }
        Command::InvalidateContract { project_id } => {
            admin(p)?;
            let mut project = project(db, p, &project_id)?;
            if publishing(db, &project_id)? {
                db.execute("INSERT INTO deferred(project,contract) VALUES(?1,1) ON CONFLICT(project) DO UPDATE SET contract=1",[&project_id]).map_err(sql)?;
                return Ok(json!({"deferred":true,"reason":"publication reconciliation required"}));
            }
            project.contract_revision = project
                .contract_revision
                .checked_add(1)
                .ok_or_else(|| conflict("contract revision exhausted"))?;
            put(db, "project", &project.id, &project.id, &project)?;
            sched::invalidate(db, &project_id, false)?;
            event(
                db,
                Some(&project_id),
                "contract_invalidated",
                value(&project)?,
            )?;
            value(&project)
        }
        Command::SendMessage {
            project_id,
            recipient_task_id,
            message_id,
            kind,
            body,
        } => {
            project(db, p, &project_id)?;
            identifier(&message_id)?;
            identifier(&kind)?;
            if body.to_string().len() > 65536 {
                return Err(invalid("message exceeds 64 KiB"));
            }
            let t: TaskRecord = get(db, "task", &recipient_task_id)?;
            if t.spec.project_id != project_id {
                return Err(err(
                    ErrorCode::Forbidden,
                    "message crosses project boundary",
                ));
            }
            if sched::terminal(t.state) {
                return Err(conflict("recipient is terminal"));
            }
            let aid = t
                .attempt_id
                .as_deref()
                .ok_or_else(|| conflict("recipient has no admitted attempt"))?;
            let a: AttemptRecord = get(db, "attempt", aid)?;
            let payload = json!({"sender":p.device_id,"body":body,"kind":kind,"project_id":project_id,"recipient_task_id":t.id,"attempt_id":a.id,"generation":a.generation});
            let old: Option<(String, String)> = db
                .query_row(
                    "SELECT kind,payload FROM messages WHERE worker=?1 AND id=?2",
                    params![a.worker_id, message_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(sql)?;
            if let Some((old_kind, old_payload)) = old {
                if old_kind != "user_message" || old_payload != payload.to_string() {
                    return Err(conflict("message ID reused with different contents"));
                }
            } else {
                message(
                    db,
                    &a.worker_id,
                    Some(&a),
                    "user_message",
                    payload,
                    Some(&message_id),
                )?;
            }
            Ok(json!({"message_id":message_id,"persisted":true}))
        }
        Command::AcknowledgeMessages { through_sequence } => {
            let w = worker(db, p, false)?;
            let max: i64 = db
                .query_row(
                    "SELECT COALESCE(MAX(sequence),0) FROM messages WHERE worker=?1",
                    [&w.id],
                    |r| r.get(0),
                )
                .map_err(sql)?;
            let prior: i64 = db
                .query_row(
                    "SELECT sequence FROM acknowledgments WHERE worker=?1",
                    [&w.id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql)?
                .unwrap_or(0);
            if through_sequence < prior || through_sequence > max || through_sequence < 0 {
                return Err(invalid(
                    "acknowledgment must be monotonic and within recipient journal",
                ));
            }
            if through_sequence != 0 {
                let exists: bool = db
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM messages WHERE worker=?1 AND sequence=?2)",
                        params![w.id, through_sequence],
                        |r| r.get(0),
                    )
                    .map_err(sql)?;
                if !exists {
                    return Err(invalid("acknowledgment does not name a recipient message"));
                }
            }
            db.execute("INSERT INTO acknowledgments(worker,sequence) VALUES(?1,?2) ON CONFLICT(worker) DO UPDATE SET sequence=excluded.sequence",params![w.id,through_sequence]).map_err(sql)?;
            Ok(json!({"acknowledged_through":through_sequence}))
        }
    }
}
