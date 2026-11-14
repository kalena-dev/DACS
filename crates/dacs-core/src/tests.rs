use super::*;

const ADMIN: &str = "test-administrator-token-with-at-least-32-bytes";
const SHA: &str = "1111111111111111111111111111111111111111";
const MERGE: &str = "2222222222222222222222222222222222222222";
fn administrator() -> Principal {
    Principal {
        is_admin: true,
        device_id: None,
    }
}
fn command(store: &Store, p: &Principal, command: Command) -> Result<Value> {
    store
        .execute(
            p,
            CommandEnvelope {
                protocol_version: PROTOCOL_VERSION,
                command_id: id(),
                command,
            },
        )
        .map(|r| r.result)
}
fn create_project(store: &Store) -> Project {
    serde_json::from_value(
        command(
            store,
            &administrator(),
            Command::CreateProject {
                name: "test project".into(),
                target_branch: "main".into(),
                max_tasks: 100,
                max_depth: 8,
            },
        )
        .unwrap(),
    )
    .unwrap()
}
fn registration(host: &str, slots: u32) -> WorkerRegistration {
    WorkerRegistration {
        name: "local worker".into(),
        physical_host_id: host.into(),
        os: "linux".into(),
        architecture: "x86_64".into(),
        capabilities: vec!["rust".into()],
        capacity: Resources {
            agent_slots: slots,
            cpu_millis: 4000,
            memory_mb: 8192,
            disk_mb: 16384,
        },
        omp_version: "pinned-1".into(),
        policy_revision: 1,
        owner_consented: true,
        consent_host: "named coordinator".into(),
    }
}
fn enroll(store: &Store, project: &Project, host: &str, slots: u32) -> (WorkerRecord, Principal) {
    let invitation: Invitation = serde_json::from_value(
        command(
            store,
            &administrator(),
            Command::CreateInvitation {
                project_id: project.id.clone(),
                host_name: "named coordinator".into(),
                expires_in_seconds: 3600,
            },
        )
        .unwrap(),
    )
    .unwrap();
    let enrolled = store
        .enroll(&invitation.token, registration(host, slots))
        .unwrap();
    command(
        store,
        &administrator(),
        Command::ApproveWorker {
            worker_id: enrolled.worker.id.clone(),
        },
    )
    .unwrap();
    let p = store.authenticate(&enrolled.device_token).unwrap();
    command(
        store,
        &p,
        Command::ReportRuntimeStatus {
            status: WorkerRuntimeStatus {
                worker_id: "ignored-client-field".into(),
                provider_authenticated: Some(true),
                network_ready: None,
                desktop_session: None,
                desktop_active: false,
                desktop_paused: false,
                observed_at: 0,
            },
        },
    )
    .unwrap();
    (enrolled.worker, p)
}
fn spec(project: &Project, effect: EffectClass) -> TaskSpec {
    TaskSpec {
        project_id: project.id.clone(),
        title: "implement feature".into(),
        instructions: "Implement and submit explicit evidence".into(),
        source_commit: SHA.into(),
        execution: TaskExecution::Agent,
        dependencies: Vec::new(),
        required_capabilities: vec!["rust".into()],
        resources: Resources {
            agent_slots: 1,
            cpu_millis: 500,
            memory_mb: 256,
            disk_mb: 512,
        },
        effect_class: effect,
        lease_seconds: 300,
        max_runtime_seconds: 3600,
        contract_revision: project.contract_revision,
        policy_revision: project.policy_revision,
        max_retries: 2,
    }
}
fn task(store: &Store, project: &Project, effect: EffectClass) -> TaskRecord {
    serde_json::from_value(
        command(
            store,
            &administrator(),
            Command::CreateTask {
                spec: spec(project, effect),
            },
        )
        .unwrap(),
    )
    .unwrap()
}
fn admit(store: &Store, task: &TaskRecord, worker: &WorkerRecord) -> AttemptRecord {
    serde_json::from_value(
        command(
            store,
            &administrator(),
            Command::AdmitTask {
                task_id: task.id.clone(),
                worker_id: Some(worker.id.clone()),
            },
        )
        .unwrap(),
    )
    .unwrap()
}
fn submission(task: &TaskRecord, attempt: &AttemptRecord) -> CandidateSubmission {
    CandidateSubmission {
        task_id: task.id.clone(),
        attempt_id: attempt.id.clone(),
        generation: attempt.generation,
        commit_sha: SHA.into(),
        contract_revision: task.spec.contract_revision,
        policy_revision: task.spec.policy_revision,
        evidence: Vec::new(),
        unresolved: Vec::new(),
    }
}
fn candidate(
    store: &Store,
    worker: &Principal,
    task: &TaskRecord,
    attempt: &AttemptRecord,
) -> CandidateRecord {
    let c: CandidateRecord = serde_json::from_value(
        command(
            store,
            worker,
            Command::SubmitCandidate {
                submission: submission(task, attempt),
            },
        )
        .unwrap(),
    )
    .unwrap();
    let live = store
        .snapshot(&administrator())
        .unwrap()
        .attempts
        .into_iter()
        .find(|a| a.id == attempt.id)
        .unwrap();
    if !live.settled {
        command(
            store,
            worker,
            Command::ReportEvent {
                attempt_id: attempt.id.clone(),
                generation: attempt.generation,
                sequence: live.last_sequence + 1,
                event: WorkerEvent::Settled,
            },
        )
        .unwrap();
    }
    store
        .stage_candidate(&administrator(), &c.id, Some(SHA.into()), MERGE.into())
        .unwrap();
    command(
        store,
        &administrator(),
        Command::ReviewCandidate {
            candidate_id: c.id.clone(),
            decision: ReviewDecision {
                candidate_sha: MERGE.into(),
                approve: true,
                reason: "reviewed exact merge".into(),
                checks_run: vec![CheckEvidence {
                    name: "tests".into(),
                    command: "cargo test".into(),
                    source_sha: MERGE.into(),
                    passed: true,
                    artifact_digest: None,
                }],
                skipped_checks: Vec::new(),
            },
        },
    )
    .unwrap();
    store.candidate(&administrator(), &c.id).unwrap()
}
fn expire(store: &Store, attempt: &AttemptRecord, project: &Project) {
    let db = store.lock().unwrap();
    let mut a: AttemptRecord = get(&db, "attempt", &attempt.id).unwrap();
    a.lease_expires_at = now() - 1;
    put(&db, "attempt", &a.id, &project.id, &a).unwrap();
}

#[test]
fn store_is_clone_send_sync() {
    fn assert_traits<T: Clone + Send + Sync>() {}
    assert_traits::<Store>();
}

#[test]
fn durable_dedup_replays_without_storing_plaintext_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("core.sqlite");
    let store = Store::open(&path, ADMIN).unwrap();
    let project = create_project(&store);
    let envelope = CommandEnvelope {
        protocol_version: 1,
        command_id: "stable-command".into(),
        command: Command::CreateInvitation {
            project_id: project.id,
            host_name: "coordinator".into(),
            expires_in_seconds: 60,
        },
    };
    let original = store.execute(&administrator(), envelope.clone()).unwrap();
    let secret = original.result["token"].as_str().unwrap();
    {
        let db = store.lock().unwrap();
        let bodies: Vec<Invitation> = all(&db, "invitation").unwrap();
        assert!(bodies.iter().all(|i| i.token.is_empty()));
        let response: Vec<u8> = db
            .query_row(
                "SELECT response FROM requests WHERE id='stable-command'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !response
                .windows(secret.len())
                .any(|w| w == secret.as_bytes())
        );
    }
    drop(store);
    let store = Store::open(&path, ADMIN).unwrap();
    assert_eq!(
        store
            .execute(&administrator(), envelope.clone())
            .unwrap()
            .result,
        original.result
    );
    let mut changed = envelope;
    changed.command = Command::CreateProject {
        name: "different".into(),
        target_branch: "main".into(),
        max_tasks: 1,
        max_depth: 1,
    };
    assert_eq!(
        store.execute(&administrator(), changed).unwrap_err().code,
        ErrorCode::Conflict
    );
}

#[test]
fn failed_commands_are_deduplicated_and_effects_rolled_back() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 1);
    let t = task(&store, &project, EffectClass::Workspace);
    let a = admit(&store, &t, &w);
    let envelope = CommandEnvelope {
        protocol_version: 1,
        command_id: "no-child-capacity".into(),
        command: Command::Delegate {
            parent_attempt_id: a.id.clone(),
            generation: a.generation,
            spec: spec(&project, EffectClass::Workspace),
        },
    };
    assert_eq!(
        store.execute(&p, envelope.clone()).unwrap_err().code,
        ErrorCode::CapacityUnavailable
    );
    let snapshot = store.snapshot(&administrator()).unwrap();
    assert_eq!(snapshot.tasks.len(), 1);
    assert!(snapshot.attempts[0].pending_children.is_empty());
    let mut changed = envelope;
    changed.command = Command::Heartbeat {
        registration: w.registration,
    };
    assert_eq!(
        store.execute(&p, changed).unwrap_err().code,
        ErrorCode::Conflict
    );
}

#[test]
fn local_consent_and_project_boundaries_are_enforced() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let first = create_project(&store);
    let second = create_project(&store);
    let (_, p) = enroll(&store, &first, "host", 2);
    assert_eq!(
        store.authorize_project(&p, &second.id).unwrap_err().code,
        ErrorCode::Forbidden
    );
    assert_eq!(store.snapshot(&p).unwrap().projects.len(), 1);
    let invitation: Invitation = serde_json::from_value(
        command(
            &store,
            &administrator(),
            Command::CreateInvitation {
                project_id: first.id,
                host_name: "other host".into(),
                expires_in_seconds: 60,
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        store
            .enroll(&invitation.token, registration("host", 1))
            .unwrap_err()
            .code,
        ErrorCode::Forbidden
    );
    let mut r = registration("host", 1);
    r.consent_host = "other host".into();
    let e = store.enroll(&invitation.token, r).unwrap();
    let pending = store.authenticate(&e.device_token).unwrap();
    assert_eq!(
        store.snapshot(&pending).unwrap_err().code,
        ErrorCode::Forbidden
    );
}

#[test]
fn heartbeat_cannot_widen_permissions_and_reduction_drains() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 2);
    let mut r = w.registration.clone();
    r.capabilities.push("desktop".into());
    assert_eq!(
        command(&store, &p, Command::Heartbeat { registration: r })
            .unwrap_err()
            .code,
        ErrorCode::Forbidden
    );
    let mut r = w.registration;
    r.capacity.agent_slots = 1;
    let result = command(&store, &p, Command::Heartbeat { registration: r }).unwrap();
    assert_eq!(result["draining"], true);
    assert_eq!(
        command(
            &store,
            &administrator(),
            Command::SetDraining {
                worker_id: w.id.clone(),
                draining: false
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::Forbidden
    );
    command(
        &store,
        &administrator(),
        Command::ApproveWorker {
            worker_id: w.id.clone(),
        },
    )
    .unwrap();
    assert!(store.snapshot(&administrator()).unwrap().workers[0].draining);
    let t = task(&store, &project, EffectClass::Workspace);
    assert_eq!(
        command(
            &store,
            &administrator(),
            Command::AdmitTask {
                task_id: t.id,
                worker_id: Some(w.id),
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::CapacityUnavailable
    );
}

#[test]
fn virtual_workers_share_physical_capacity() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w1, _) = enroll(&store, &project, "physical-host", 1);
    let (w2, _) = enroll(&store, &project, "physical-host", 1);
    let t1 = task(&store, &project, EffectClass::Workspace);
    admit(&store, &t1, &w1);
    let t2 = task(&store, &project, EffectClass::Workspace);
    assert_eq!(
        command(
            &store,
            &administrator(),
            Command::AdmitTask {
                task_id: t2.id,
                worker_id: Some(w2.id)
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::CapacityUnavailable
    );
}

#[test]
fn pending_children_survive_restart_and_settled_is_not_finished() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite");
    let store = Store::open(&path, ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 2);
    let t = task(&store, &project, EffectClass::Workspace);
    let a = admit(&store, &t, &w);
    let child = command(
        &store,
        &p,
        Command::Delegate {
            parent_attempt_id: a.id.clone(),
            generation: a.generation,
            spec: spec(&project, EffectClass::Workspace),
        },
    )
    .unwrap();
    command(
        &store,
        &p,
        Command::ReportEvent {
            attempt_id: a.id.clone(),
            generation: a.generation,
            sequence: 1,
            event: WorkerEvent::Settled,
        },
    )
    .unwrap();
    drop(store);
    let store = Store::open(&path, ADMIN).unwrap();
    let snapshot = store.snapshot(&administrator()).unwrap();
    let parent = snapshot.attempts.iter().find(|v| v.id == a.id).unwrap();
    assert!(parent.settled);
    assert_eq!(
        parent.pending_children,
        vec![child["task"]["id"].as_str().unwrap()]
    );
    assert_eq!(snapshot.workers[0].reserved.agent_slots, 2);
    assert_eq!(
        command(
            &store,
            &p,
            Command::SubmitCandidate {
                submission: submission(&t, &a)
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::Conflict
    );
    command(
        &store,
        &administrator(),
        Command::CancelTask {
            task_id: child["task"]["id"].as_str().unwrap().into(),
        },
    )
    .unwrap();
    assert!(
        !store
            .commands(&p, 0)
            .unwrap()
            .iter()
            .any(|m| m.kind == "child_result")
    );
    command(
        &store,
        &p,
        Command::ReconcileWorker {
            stopped_attempt_ids: vec![child["attempt"]["id"].as_str().unwrap().into()],
        },
    )
    .unwrap();
    assert!(
        store
            .commands(&p, 0)
            .unwrap()
            .iter()
            .any(|m| m.kind == "child_result")
    );
}

#[test]
fn delegation_rejects_ancestor_cycle_without_allocating() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 2);
    let t = task(&store, &project, EffectClass::Workspace);
    let a = admit(&store, &t, &w);
    let mut child = spec(&project, EffectClass::Workspace);
    child.dependencies.push(t.id);
    assert_eq!(
        command(
            &store,
            &p,
            Command::Delegate {
                parent_attempt_id: a.id,
                generation: a.generation,
                spec: child
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::Invalid
    );
    assert_eq!(store.snapshot(&administrator()).unwrap().tasks.len(), 1);
}

#[test]
fn expired_workspace_is_fenced_but_reservation_is_held() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 1);
    let t = task(&store, &project, EffectClass::Workspace);
    let a = admit(&store, &t, &w);
    expire(&store, &a, &project);
    store.maintain().unwrap();
    let snapshot = store.snapshot(&administrator()).unwrap();
    assert_eq!(snapshot.tasks[0].state, TaskState::Ready);
    assert_eq!(snapshot.workers[0].reserved.agent_slots, 1);
    assert_eq!(
        command(
            &store,
            &p,
            Command::SubmitCandidate {
                submission: submission(&t, &a)
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::Conflict
    );
    assert_eq!(
        command(
            &store,
            &administrator(),
            Command::AdmitTask {
                task_id: t.id.clone(),
                worker_id: Some(w.id.clone())
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::CapacityUnavailable
    );
    command(
        &store,
        &p,
        Command::ReconcileWorker {
            stopped_attempt_ids: vec![a.id.clone()],
        },
    )
    .unwrap();
    let replacement = admit(&store, &t, &w);
    assert_eq!(replacement.generation, a.generation + 1);
    assert_eq!(
        command(
            &store,
            &p,
            Command::ReportEvent {
                attempt_id: a.id,
                generation: a.generation,
                sequence: 1,
                event: WorkerEvent::Started
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::Conflict
    );
}

#[test]
fn effectful_expiry_requires_explicit_abandonment() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w, _) = enroll(&store, &project, "host", 2);
    let t = task(&store, &project, EffectClass::External);
    let a = admit(&store, &t, &w);
    expire(&store, &a, &project);
    store.maintain().unwrap();
    assert_eq!(
        store.snapshot(&administrator()).unwrap().tasks[0].state,
        TaskState::Executing
    );
    command(
        &store,
        &administrator(),
        Command::AbandonAttempt {
            attempt_id: a.id,
            reason: "owner confirms external ownership abandoned".into(),
        },
    )
    .unwrap();
    let snapshot = store.snapshot(&administrator()).unwrap();
    assert_eq!(snapshot.tasks[0].state, TaskState::Ready);
    assert_eq!(snapshot.workers[0].reserved.agent_slots, 1);
}

#[test]
fn cost_cap_remains_distinct_and_never_auto_retries() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 1);
    let t = task(&store, &project, EffectClass::Workspace);
    let a = admit(&store, &t, &w);
    command(
        &store,
        &p,
        Command::ReportEvent {
            attempt_id: a.id.clone(),
            generation: a.generation,
            sequence: 1,
            event: WorkerEvent::CostCapped {
                message: "provider budget".into(),
            },
        },
    )
    .unwrap();
    expire(&store, &a, &project);
    store.maintain().unwrap();
    let snapshot = store.snapshot(&administrator()).unwrap();
    assert_eq!(snapshot.attempts[0].state, "cost_capped");
    assert_eq!(snapshot.tasks[0].state, TaskState::Executing);
}

#[test]
fn worker_sequence_and_acknowledgment_bounds_are_strict() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 1);
    let t = task(&store, &project, EffectClass::Workspace);
    let a = admit(&store, &t, &w);
    assert_eq!(
        command(
            &store,
            &p,
            Command::ReportEvent {
                attempt_id: a.id.clone(),
                generation: a.generation,
                sequence: 2,
                event: WorkerEvent::Started
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::Conflict
    );
    command(
        &store,
        &p,
        Command::ReportEvent {
            attempt_id: a.id,
            generation: a.generation,
            sequence: 1,
            event: WorkerEvent::Started,
        },
    )
    .unwrap();
    let messages = store.commands(&p, 0).unwrap();
    let max = messages.last().unwrap().sequence;
    assert_eq!(
        command(
            &store,
            &p,
            Command::AcknowledgeMessages {
                through_sequence: max + 1
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::Invalid
    );
    command(
        &store,
        &p,
        Command::AcknowledgeMessages {
            through_sequence: max,
        },
    )
    .unwrap();
    assert_eq!(
        command(
            &store,
            &p,
            Command::AcknowledgeMessages {
                through_sequence: 0
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::Invalid
    );
}

#[test]
fn staged_sha_changes_invalidate_review() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 1);
    let t = task(&store, &project, EffectClass::Workspace);
    let a = admit(&store, &t, &w);
    let c = candidate(&store, &p, &t, &a);
    let changed = store
        .stage_candidate(
            &administrator(),
            &c.id,
            Some(SHA.into()),
            "3333333333333333333333333333333333333333".into(),
        )
        .unwrap();
    assert!(changed.decision.is_none());
    assert_eq!(
        store
            .prepare_publication(&administrator(), &c.id)
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
}

#[test]
fn publication_reconciles_after_restart_before_releasing_dependencies() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite");
    let store = Store::open(&path, ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 2);
    let t = task(&store, &project, EffectClass::Workspace);
    let a = admit(&store, &t, &w);
    let c = candidate(&store, &p, &t, &a);
    let mut dependent = spec(&project, EffectClass::Workspace);
    dependent.dependencies.push(t.id.clone());
    let dep: TaskRecord = serde_json::from_value(
        command(
            &store,
            &administrator(),
            Command::CreateTask { spec: dependent },
        )
        .unwrap(),
    )
    .unwrap();
    let publication = store.prepare_publication(&administrator(), &c.id).unwrap();
    drop(store);
    let store = Store::open(&path, ADMIN).unwrap();
    assert_eq!(store.pending_publications().unwrap().len(), 1);
    store
        .finish_publication(&publication.id, Some(MERGE))
        .unwrap();
    let snapshot = store.snapshot(&administrator()).unwrap();
    assert_eq!(
        snapshot.tasks.iter().find(|v| v.id == t.id).unwrap().state,
        TaskState::Accepted
    );
    assert_eq!(
        snapshot
            .tasks
            .iter()
            .find(|v| v.id == dep.id)
            .unwrap()
            .state,
        TaskState::Ready
    );
    assert_eq!(snapshot.workers[0].reserved.agent_slots, 1);
    command(
        &store,
        &p,
        Command::ReportEvent {
            attempt_id: a.id.clone(),
            generation: a.generation,
            sequence: snapshot
                .attempts
                .iter()
                .find(|r| r.id == a.id)
                .unwrap()
                .last_sequence
                + 1,
            event: WorkerEvent::Stopped,
        },
    )
    .unwrap();
    let stopped = store.snapshot(&administrator()).unwrap();
    assert_eq!(stopped.workers[0].reserved.agent_slots, 0);
    let accepted = stopped.tasks.iter().find(|r| r.id == t.id).unwrap();
    assert_eq!(accepted.state, TaskState::Accepted);
    assert_eq!(accepted.integrated_sha.as_deref(), Some(MERGE));
    assert!(accepted.blocked_reason.is_none());
}

#[test]
fn divergent_publication_remains_unresolved_and_cancellation_is_deferred() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 1);
    let t = task(&store, &project, EffectClass::Workspace);
    let a = admit(&store, &t, &w);
    let c = candidate(&store, &p, &t, &a);
    let publication = store.prepare_publication(&administrator(), &c.id).unwrap();
    command(
        &store,
        &administrator(),
        Command::CancelTask {
            task_id: t.id.clone(),
        },
    )
    .unwrap();
    store
        .finish_publication(
            &publication.id,
            Some("3333333333333333333333333333333333333333"),
        )
        .unwrap();
    assert_eq!(
        store.snapshot(&administrator()).unwrap().tasks[0].state,
        TaskState::Publishing
    );
    assert_eq!(
        store.pending_publications().unwrap()[0].state,
        "reconciling"
    );
    store
        .finish_publication(&publication.id, Some(SHA))
        .unwrap();
    let pending = store.snapshot(&administrator()).unwrap();
    assert!(pending.tasks[0].cancel_requested);
    assert!(!scheduling::terminal(pending.tasks[0].state));
    command(
        &store,
        &p,
        Command::ReconcileWorker {
            stopped_attempt_ids: vec![a.id.clone()],
        },
    )
    .unwrap();
    assert_eq!(
        store.snapshot(&administrator()).unwrap().tasks[0].state,
        TaskState::Cancelled
    );
    assert!(store.pending_publications().unwrap().is_empty());
}

#[test]
fn publication_and_cancellation_have_one_transactional_order() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 1);
    let t = task(&store, &project, EffectClass::Workspace);
    let a = admit(&store, &t, &w);
    let c = candidate(&store, &p, &t, &a);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let other = store.clone();
    let b = barrier.clone();
    let task_id = t.id.clone();
    let thread = std::thread::spawn(move || {
        b.wait();
        command(&other, &administrator(), Command::CancelTask { task_id }).unwrap()
    });
    barrier.wait();
    let prepared = store.prepare_publication(&administrator(), &c.id);
    thread.join().unwrap();
    match prepared {
        Ok(publication) => {
            assert_eq!(
                store.snapshot(&administrator()).unwrap().tasks[0].state,
                TaskState::Publishing
            );
            store
                .finish_publication(&publication.id, Some(MERGE))
                .unwrap();
            assert_eq!(
                store.snapshot(&administrator()).unwrap().tasks[0].state,
                TaskState::Accepted
            );
        }
        Err(error) => {
            assert_eq!(error.code, ErrorCode::Conflict);
            assert!(store.snapshot(&administrator()).unwrap().tasks[0].cancel_requested);
            assert!(!scheduling::terminal(
                store.snapshot(&administrator()).unwrap().tasks[0].state
            ));
            command(
                &store,
                &p,
                Command::ReconcileWorker {
                    stopped_attempt_ids: vec![a.id.clone()],
                },
            )
            .unwrap();
            assert_eq!(
                store.snapshot(&administrator()).unwrap().tasks[0].state,
                TaskState::Cancelled
            );
        }
    }
}

#[test]
fn deferred_invalidation_applies_only_after_publication_outcome() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 2);
    let t = task(&store, &project, EffectClass::Workspace);
    let a = admit(&store, &t, &w);
    let c = candidate(&store, &p, &t, &a);
    let publication = store.prepare_publication(&administrator(), &c.id).unwrap();
    let second = task(&store, &project, EffectClass::Workspace);
    assert_eq!(
        command(
            &store,
            &administrator(),
            Command::InvalidateContract {
                project_id: project.id.clone()
            }
        )
        .unwrap()["deferred"],
        true
    );
    assert_eq!(
        store
            .authorize_project(&administrator(), &project.id)
            .unwrap()
            .contract_revision,
        1
    );
    store
        .finish_publication(&publication.id, Some(MERGE))
        .unwrap();
    let snapshot = store.snapshot(&administrator()).unwrap();
    assert_eq!(snapshot.projects[0].contract_revision, 2);
    assert_eq!(
        snapshot
            .tasks
            .iter()
            .find(|t| t.id == second.id)
            .unwrap()
            .state,
        TaskState::Cancelled
    );
}

#[test]
fn usage_and_desktop_reports_are_real_scoped_records() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 1);
    let t = task(&store, &project, EffectClass::Workspace);
    let a = admit(&store, &t, &w);
    command(
        &store,
        &p,
        Command::ReportEvent {
            attempt_id: a.id,
            generation: a.generation,
            sequence: 1,
            event: WorkerEvent::Usage {
                input_tokens: 100,
                output_tokens: 50,
                cost_usd: Some(0.25),
                estimated: false,
            },
        },
    )
    .unwrap();
    command(
        &store,
        &administrator(),
        Command::RequestDesktopPause {
            worker_id: w.id.clone(),
        },
    )
    .unwrap();
    let snapshot = store.snapshot(&p).unwrap();
    assert_eq!(snapshot.usage[0].worker_id, w.id);
    assert_eq!(snapshot.usage[0].input_tokens, 100);
    assert!(!snapshot.runtime_status[0].desktop_paused);
    assert!(
        snapshot
            .messages
            .iter()
            .any(|m| m.kind == "pause_desktop" && m.generation.is_none())
    );
}

#[test]
fn unchanged_status_reports_add_no_event_and_deliveries_track_acknowledgment() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 1);
    let report = |paused: bool| {
        command(
            &store,
            &p,
            Command::ReportRuntimeStatus {
                status: WorkerRuntimeStatus {
                    worker_id: w.id.clone(),
                    provider_authenticated: Some(true),
                    network_ready: Some(true),
                    desktop_session: None,
                    desktop_active: false,
                    desktop_paused: paused,
                    observed_at: 0,
                },
            },
        )
        .unwrap();
    };
    let status_events = || {
        store
            .snapshot(&administrator())
            .unwrap()
            .events
            .iter()
            .filter(|e| e.kind == "runtime_status")
            .count()
    };
    report(false);
    let baseline = status_events();
    report(false);
    assert_eq!(status_events(), baseline, "a repeated status is not an event");
    report(true);
    assert_eq!(status_events(), baseline + 1, "a changed status is an event");

    command(
        &store,
        &administrator(),
        Command::RequestDesktopPause {
            worker_id: w.id.clone(),
        },
    )
    .unwrap();
    let delivery = || {
        let snapshot = store.snapshot(&administrator()).unwrap();
        assert_eq!(snapshot.deliveries.len(), snapshot.messages.len());
        snapshot.deliveries.last().cloned().unwrap()
    };
    let queued = delivery();
    assert!(queued.queued_at > 0 && !queued.acknowledged);
    command(
        &store,
        &p,
        Command::AcknowledgeMessages {
            through_sequence: queued.sequence,
        },
    )
    .unwrap();
    assert!(delivery().acknowledged);
}

#[test]
fn application_messages_cannot_forge_worker_control_commands() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 1);
    let t = task(&store, &project, EffectClass::Workspace);
    admit(&store, &t, &w);
    command(
        &store,
        &p,
        Command::SendMessage {
            project_id: project.id,
            recipient_task_id: t.id,
            message_id: "caller-message".into(),
            kind: "stop".into(),
            body: json!({"reason":"untrusted user content"}),
        },
    )
    .unwrap();
    let messages = store.commands(&p, 0).unwrap();
    let sent = messages.iter().find(|m| m.id == "caller-message").unwrap();
    assert_eq!(sent.kind, "user_message");
    assert_eq!(sent.payload["kind"], "stop");
}

#[test]
fn pending_invalidation_blocks_new_admission() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 2);
    let first = task(&store, &project, EffectClass::Workspace);
    let second = task(&store, &project, EffectClass::Workspace);
    let a = admit(&store, &first, &w);
    let c = candidate(&store, &p, &first, &a);
    store.prepare_publication(&administrator(), &c.id).unwrap();
    command(
        &store,
        &administrator(),
        Command::InvalidateContract {
            project_id: project.id.clone(),
        },
    )
    .unwrap();
    assert_eq!(
        command(
            &store,
            &administrator(),
            Command::AdmitTask {
                task_id: second.id,
                worker_id: Some(w.id)
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::Conflict
    );
    assert_eq!(
        command(
            &store,
            &administrator(),
            Command::CreateTask {
                spec: spec(&project, EffectClass::Workspace)
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::Conflict
    );
}

#[test]
fn stopping_parent_with_pending_children_keeps_wait_capacity() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 2);
    let t = task(&store, &project, EffectClass::Workspace);
    let a = admit(&store, &t, &w);
    command(
        &store,
        &p,
        Command::Delegate {
            parent_attempt_id: a.id.clone(),
            generation: a.generation,
            spec: spec(&project, EffectClass::Workspace),
        },
    )
    .unwrap();
    command(
        &store,
        &p,
        Command::ReconcileWorker {
            stopped_attempt_ids: vec![a.id.clone()],
        },
    )
    .unwrap();
    assert_eq!(
        store.snapshot(&administrator()).unwrap().workers[0]
            .reserved
            .agent_slots,
        2
    );
    let snapshot = store.snapshot(&administrator()).unwrap();
    let child = snapshot
        .tasks
        .iter()
        .find(|child| child.parent_task_id.as_deref() == Some(&t.id))
        .unwrap();
    let child_attempt = snapshot
        .attempts
        .iter()
        .find(|attempt| attempt.task_id == child.id)
        .unwrap();
    let c = candidate(&store, &p, child, child_attempt);
    let publication = store.prepare_publication(&administrator(), &c.id).unwrap();
    store
        .finish_publication(&publication.id, Some(MERGE))
        .unwrap();
    command(
        &store,
        &p,
        Command::ReconcileWorker {
            stopped_attempt_ids: vec![child_attempt.id.clone()],
        },
    )
    .unwrap();
    command(
        &store,
        &p,
        Command::ReportEvent {
            attempt_id: a.id.clone(),
            generation: a.generation,
            sequence: 1,
            event: WorkerEvent::Started,
        },
    )
    .unwrap();
    command(
        &store,
        &p,
        Command::ReportEvent {
            attempt_id: a.id.clone(),
            generation: a.generation,
            sequence: 2,
            event: WorkerEvent::Progress {
                message: "resumed after durable child delivery".into(),
            },
        },
    )
    .unwrap();
    assert!(
        command(
            &store,
            &p,
            Command::Delegate {
                parent_attempt_id: a.id,
                generation: a.generation,
                spec: spec(&project, EffectClass::Workspace)
            }
        )
        .unwrap()["admitted"]
            .as_bool()
            .unwrap()
    );
}

#[test]
fn unapproved_and_revoked_host_declarations_cannot_reduce_authorized_capacity() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (approved, _) = enroll(&store, &project, "shared-host", 2);
    let invite: Invitation = serde_json::from_value(
        command(
            &store,
            &administrator(),
            Command::CreateInvitation {
                project_id: project.id.clone(),
                host_name: "named coordinator".into(),
                expires_in_seconds: 300,
            },
        )
        .unwrap(),
    )
    .unwrap();
    let pending = store
        .enroll(&invite.token, registration("shared-host", 0))
        .unwrap();
    let first = task(&store, &project, EffectClass::Workspace);
    admit(&store, &first, &approved);
    command(
        &store,
        &administrator(),
        Command::ApproveWorker {
            worker_id: pending.worker.id.clone(),
        },
    )
    .unwrap();
    command(
        &store,
        &administrator(),
        Command::RevokeWorker {
            worker_id: pending.worker.id,
        },
    )
    .unwrap();
    let second = task(&store, &project, EffectClass::Workspace);
    admit(&store, &second, &approved);
    assert_eq!(
        store
            .snapshot(&administrator())
            .unwrap()
            .workers
            .iter()
            .find(|w| w.id == approved.id)
            .unwrap()
            .reserved
            .agent_slots,
        2
    );
}

#[test]
fn revoked_workers_still_contribute_held_physical_reservations() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (first_worker, _) = enroll(&store, &project, "shared-host", 1);
    let (second_worker, _) = enroll(&store, &project, "shared-host", 1);
    let first = task(&store, &project, EffectClass::Workspace);
    let first_attempt = admit(&store, &first, &first_worker);
    command(
        &store,
        &administrator(),
        Command::RevokeWorker {
            worker_id: first_worker.id,
        },
    )
    .unwrap();
    let second = task(&store, &project, EffectClass::Workspace);
    assert_eq!(
        command(
            &store,
            &administrator(),
            Command::AdmitTask {
                task_id: second.id.clone(),
                worker_id: Some(second_worker.id.clone())
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::CapacityUnavailable
    );
    command(
        &store,
        &administrator(),
        Command::ReconcileWorker {
            stopped_attempt_ids: vec![first_attempt.id],
        },
    )
    .unwrap();
    admit(&store, &second, &second_worker);
}

#[test]
fn reviewed_candidate_cannot_publish_until_post_submission_settlement() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 1);
    let t = task(&store, &project, EffectClass::Workspace);
    let a = admit(&store, &t, &w);
    let c: CandidateRecord = serde_json::from_value(
        command(
            &store,
            &p,
            Command::SubmitCandidate {
                submission: submission(&t, &a),
            },
        )
        .unwrap(),
    )
    .unwrap();
    store
        .stage_candidate(&administrator(), &c.id, Some(SHA.into()), MERGE.into())
        .unwrap();
    command(
        &store,
        &administrator(),
        Command::ReviewCandidate {
            candidate_id: c.id.clone(),
            decision: ReviewDecision {
                candidate_sha: MERGE.into(),
                approve: true,
                reason: "explicit exact-SHA review".into(),
                checks_run: Vec::new(),
                skipped_checks: vec!["fixture exercises publication ordering, not a build".into()],
            },
        },
    )
    .unwrap();
    assert_eq!(
        store
            .prepare_publication(&administrator(), &c.id)
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    assert!(store.pending_publications().unwrap().is_empty());
    command(
        &store,
        &p,
        Command::ReportEvent {
            attempt_id: a.id.clone(),
            generation: a.generation,
            sequence: 1,
            event: WorkerEvent::Settled,
        },
    )
    .unwrap();
    let snapshot = store.snapshot(&administrator()).unwrap();
    assert_eq!(snapshot.attempts[0].state, "result_submitted");
    assert!(snapshot.attempts[0].settled);
    store.prepare_publication(&administrator(), &c.id).unwrap();
}

#[test]
fn cancelling_unreachable_external_child_retains_parent_obligation_until_stop() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 2);
    let t = task(&store, &project, EffectClass::External);
    let a = admit(&store, &t, &w);
    let delegated = command(
        &store,
        &p,
        Command::Delegate {
            parent_attempt_id: a.id.clone(),
            generation: a.generation,
            spec: spec(&project, EffectClass::External),
        },
    )
    .unwrap();
    let child: TaskRecord = serde_json::from_value(delegated["task"].clone()).unwrap();
    let child_attempt: AttemptRecord =
        serde_json::from_value(delegated["attempt"].clone()).unwrap();
    expire(&store, &child_attempt, &project);
    command(
        &store,
        &administrator(),
        Command::CancelTask {
            task_id: child.id.clone(),
        },
    )
    .unwrap();
    store.maintain().unwrap();
    let snapshot = store.snapshot(&administrator()).unwrap();
    let cancelled = snapshot.tasks.iter().find(|t| t.id == child.id).unwrap();
    assert!(cancelled.cancel_requested);
    assert!(!scheduling::terminal(cancelled.state));
    assert!(
        snapshot
            .attempts
            .iter()
            .find(|parent| parent.id == a.id)
            .unwrap()
            .pending_children
            .contains(&child.id)
    );
    assert!(!snapshot.messages.iter().any(|m| m.kind == "child_result"));
    assert_eq!(
        command(
            &store,
            &p,
            Command::SubmitCandidate {
                submission: submission(&t, &a)
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::Conflict
    );
    command(
        &store,
        &p,
        Command::ReconcileWorker {
            stopped_attempt_ids: vec![child_attempt.id],
        },
    )
    .unwrap();
    let snapshot = store.snapshot(&administrator()).unwrap();
    assert_eq!(
        snapshot
            .tasks
            .iter()
            .find(|t| t.id == child.id)
            .unwrap()
            .state,
        TaskState::Cancelled
    );
    assert!(
        snapshot
            .attempts
            .iter()
            .find(|parent| parent.id == a.id)
            .unwrap()
            .pending_children
            .is_empty()
    );
    assert!(snapshot.messages.iter().any(|m| m.kind == "child_result"));
}

#[test]
fn parent_cancellation_waits_for_child_publication_and_termination() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 2);
    let t = task(&store, &project, EffectClass::External);
    let a = admit(&store, &t, &w);
    let delegated = command(
        &store,
        &p,
        Command::Delegate {
            parent_attempt_id: a.id.clone(),
            generation: a.generation,
            spec: spec(&project, EffectClass::External),
        },
    )
    .unwrap();
    let child: TaskRecord = serde_json::from_value(delegated["task"].clone()).unwrap();
    let child_attempt: AttemptRecord =
        serde_json::from_value(delegated["attempt"].clone()).unwrap();
    let c = candidate(&store, &p, &child, &child_attempt);
    let publication = store.prepare_publication(&administrator(), &c.id).unwrap();
    command(
        &store,
        &administrator(),
        Command::CancelTask {
            task_id: t.id.clone(),
        },
    )
    .unwrap();
    command(
        &store,
        &p,
        Command::ReconcileWorker {
            stopped_attempt_ids: vec![a.id.clone()],
        },
    )
    .unwrap();
    assert!(!scheduling::terminal(
        store
            .snapshot(&administrator())
            .unwrap()
            .tasks
            .iter()
            .find(|r| r.id == t.id)
            .unwrap()
            .state
    ));
    store
        .finish_publication(&publication.id, Some(SHA))
        .unwrap();
    assert!(!scheduling::terminal(
        store
            .snapshot(&administrator())
            .unwrap()
            .tasks
            .iter()
            .find(|r| r.id == t.id)
            .unwrap()
            .state
    ));
    command(
        &store,
        &p,
        Command::ReconcileWorker {
            stopped_attempt_ids: vec![child_attempt.id],
        },
    )
    .unwrap();
    let snapshot = store.snapshot(&administrator()).unwrap();
    assert!(
        snapshot
            .tasks
            .iter()
            .all(|t| t.state == TaskState::Cancelled)
    );
    assert!(
        snapshot
            .attempts
            .iter()
            .find(|r| r.id == a.id)
            .unwrap()
            .pending_children
            .is_empty()
    );
}

#[test]
fn stopped_event_preserves_waiting_parent_resume_and_capacity() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 2);
    let t = task(&store, &project, EffectClass::Workspace);
    let a = admit(&store, &t, &w);
    let delegated = command(
        &store,
        &p,
        Command::Delegate {
            parent_attempt_id: a.id.clone(),
            generation: a.generation,
            spec: spec(&project, EffectClass::Workspace),
        },
    )
    .unwrap();
    let child: TaskRecord = serde_json::from_value(delegated["task"].clone()).unwrap();
    let child_attempt: AttemptRecord =
        serde_json::from_value(delegated["attempt"].clone()).unwrap();
    command(
        &store,
        &p,
        Command::ReportEvent {
            attempt_id: a.id.clone(),
            generation: a.generation,
            sequence: 1,
            event: WorkerEvent::Stopped,
        },
    )
    .unwrap();
    let snapshot = store.snapshot(&administrator()).unwrap();
    assert_eq!(
        snapshot
            .attempts
            .iter()
            .find(|r| r.id == a.id)
            .unwrap()
            .state,
        "waiting"
    );
    assert_eq!(snapshot.workers[0].reserved.agent_slots, 2);
    command(
        &store,
        &administrator(),
        Command::CancelTask { task_id: child.id },
    )
    .unwrap();
    command(
        &store,
        &p,
        Command::ReconcileWorker {
            stopped_attempt_ids: vec![child_attempt.id],
        },
    )
    .unwrap();
    command(
        &store,
        &p,
        Command::ReportEvent {
            attempt_id: a.id.clone(),
            generation: a.generation,
            sequence: 2,
            event: WorkerEvent::Progress {
                message: "resumed with retained reservation".into(),
            },
        },
    )
    .unwrap();
    command(
        &store,
        &p,
        Command::Delegate {
            parent_attempt_id: a.id.clone(),
            generation: a.generation,
            spec: spec(&project, EffectClass::Workspace),
        },
    )
    .unwrap();
    let snapshot = store.snapshot(&administrator()).unwrap();
    let resumed = snapshot.attempts.iter().find(|r| r.id == a.id).unwrap();
    assert_eq!(resumed.state, "executing");
    assert!(!resumed.settled);
    assert_eq!(snapshot.workers[0].reserved.agent_slots, 2);
}

#[test]
fn workspace_parent_cannot_delegate_external_effects() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 2);
    let t = task(&store, &project, EffectClass::Workspace);
    let a = admit(&store, &t, &w);
    assert_eq!(
        command(
            &store,
            &p,
            Command::Delegate {
                parent_attempt_id: a.id.clone(),
                generation: a.generation,
                spec: spec(&project, EffectClass::External)
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::Forbidden
    );
    let snapshot = store.snapshot(&administrator()).unwrap();
    assert_eq!(snapshot.tasks.len(), 1);
    assert!(snapshot.attempts[0].pending_children.is_empty());
    assert_eq!(snapshot.workers[0].reserved.agent_slots, 1);
    expire(&store, &a, &project);
    store.maintain().unwrap();
    assert_eq!(
        store.snapshot(&administrator()).unwrap().tasks[0].state,
        TaskState::Ready
    );
}

#[test]
fn expired_original_external_completion_is_accepted_only_after_explicit_reconciliation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("external.sqlite");
    let store = Store::open(&path, ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 1);
    let t = task(&store, &project, EffectClass::External);
    let a = admit(&store, &t, &w);
    expire(&store, &a, &project);
    store.maintain().unwrap();
    assert_eq!(
        command(
            &store,
            &p,
            Command::SubmitCandidate {
                submission: submission(&t, &a)
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::Conflict
    );
    command(
        &store,
        &p,
        Command::ReconcileWorker {
            stopped_attempt_ids: vec![a.id.clone()],
        },
    )
    .unwrap();
    drop(store);
    let store = Store::open(&path, ADMIN).unwrap();
    let c = candidate(&store, &p, &t, &a);
    let publication = store.prepare_publication(&administrator(), &c.id).unwrap();
    store
        .finish_publication(&publication.id, Some(MERGE))
        .unwrap();
    let snapshot = store.snapshot(&administrator()).unwrap();
    assert_eq!(snapshot.tasks[0].state, TaskState::Accepted);
    assert_eq!(snapshot.tasks[0].generation, a.generation);
    assert_eq!(snapshot.workers[0].reserved.agent_slots, 0);
}

#[test]
fn reconciled_fenced_workspace_cannot_submit_original_completion() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (w, p) = enroll(&store, &project, "host", 1);
    let t = task(&store, &project, EffectClass::Workspace);
    let a = admit(&store, &t, &w);
    expire(&store, &a, &project);
    store.maintain().unwrap();
    command(
        &store,
        &p,
        Command::ReconcileWorker {
            stopped_attempt_ids: vec![a.id.clone()],
        },
    )
    .unwrap();
    assert_eq!(
        command(
            &store,
            &p,
            Command::SubmitCandidate {
                submission: submission(&t, &a)
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::Conflict
    );
    assert!(
        store
            .snapshot(&administrator())
            .unwrap()
            .candidates
            .is_empty()
    );
}

#[test]
fn stopped_event_finishes_cancellation_without_reopening_the_task() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (worker, principal) = enroll(&store, &project, "host", 1);
    let task = task(&store, &project, EffectClass::External);
    let attempt = admit(&store, &task, &worker);
    command(
        &store,
        &administrator(),
        Command::CancelTask {
            task_id: task.id.clone(),
        },
    )
    .unwrap();
    command(
        &store,
        &principal,
        Command::ReportEvent {
            attempt_id: attempt.id.clone(),
            generation: attempt.generation,
            sequence: 1,
            event: WorkerEvent::Stopped,
        },
    )
    .unwrap();
    let snapshot = store.snapshot(&administrator()).unwrap();
    assert_eq!(snapshot.tasks[0].state, TaskState::Cancelled);
    assert_eq!(snapshot.workers[0].reserved.agent_slots, 0);
    assert!(
        command(
            &store,
            &principal,
            Command::ReportEvent {
                attempt_id: attempt.id,
                generation: attempt.generation,
                sequence: 2,
                event: WorkerEvent::Progress {
                    message: "late execution".into()
                },
            }
        )
        .is_err()
    );
}

#[test]
fn desktop_admission_requires_observation_and_excludes_reserved_desktops() {
    let store = Store::open(":memory:", ADMIN).unwrap();
    let project = create_project(&store);
    let (worker, principal) = enroll(&store, &project, "desktop-owner", 2);
    {
        let db = store.lock().unwrap();
        let mut record: WorkerRecord = get(&db, "worker", &worker.id).unwrap();
        record.registration.capabilities.push("desktop".into());
        put(&db, "worker", &worker.id, &project.id, &record).unwrap();
    }
    let make_task = || {
        let mut spec = spec(&project, EffectClass::External);
        spec.required_capabilities.push("desktop".into());
        serde_json::from_value::<TaskRecord>(
            command(&store, &administrator(), Command::CreateTask { spec }).unwrap(),
        )
        .unwrap()
    };
    let first = make_task();
    let second = make_task();
    let request = |task: &TaskRecord| {
        command(
            &store,
            &administrator(),
            Command::AdmitTask {
                task_id: task.id.clone(),
                worker_id: Some(worker.id.clone()),
            },
        )
    };
    assert_eq!(
        request(&first).unwrap_err().code,
        ErrorCode::CapacityUnavailable
    );
    let report = |paused| {
        command(
            &store,
            &principal,
            Command::ReportRuntimeStatus {
                status: WorkerRuntimeStatus {
                    worker_id: worker.id.clone(),
                    provider_authenticated: Some(true),
                    network_ready: None,
                    desktop_session: Some("observed-owner-session".into()),
                    desktop_active: false,
                    desktop_paused: paused,
                    observed_at: 0,
                },
            },
        )
        .unwrap()
    };
    report(true);
    assert_eq!(
        request(&first).unwrap_err().code,
        ErrorCode::CapacityUnavailable
    );
    report(false);
    let admitted: AttemptRecord = serde_json::from_value(request(&first).unwrap()).unwrap();
    assert_eq!(admitted.task_id, first.id);
    assert_eq!(
        request(&second).unwrap_err().code,
        ErrorCode::CapacityUnavailable
    );
}
