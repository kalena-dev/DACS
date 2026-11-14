use super::*;
use dacs_protocol::{
    AttemptRecord, CandidateSubmission, Command, CommandEnvelope, EffectClass, Enrollment,
    Invitation, PROTOCOL_VERSION, Resources, ReviewDecision, TaskRecord, TaskSpec, TaskState,
    WorkerRegistration,
};
use std::process::Command as ProcessCommand;

const ADMIN_TOKEN: &str = "source-fixture-admin-token-at-least-32-bytes";

struct Fixture {
    _temporary: tempfile::TempDir,
    path: PathBuf,
    work: PathBuf,
    store: Store,
    source: SourceService,
    admin: Principal,
    project: Project,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        // macOS system temporary paths may use a /var symlink. The service is
        // intentionally given the resolved private directory, not that alias.
        let path = fs::canonicalize(temporary.path()).unwrap();
        let store = Store::open(path.join("state.sqlite"), ADMIN_TOKEN).unwrap();
        let admin = store.authenticate(ADMIN_TOKEN).unwrap();
        let project: Project = serde_json::from_value(execute(
            &store,
            &admin,
            Command::CreateProject {
                name: "Source fixture".into(),
                target_branch: "main".into(),
                max_tasks: 100,
                max_depth: 4,
            },
        ))
        .unwrap();
        let source = SourceService::new(path.join("source"), store.clone()).unwrap();
        let work = path.join("work");
        fs::create_dir(&work).unwrap();
        let fixture = Self {
            _temporary: temporary,
            path,
            work,
            store,
            source,
            admin,
            project,
        };
        fixture.git(&["init", "--initial-branch=main", "--template="]);
        fixture.commit("shared.txt", "base\n", "base");
        fixture
    }

    fn git(&self, args: &[&str]) -> String {
        let output = ProcessCommand::new("git")
            .current_dir(&self.work)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@dacs.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@dacs.invalid")
            .env("GIT_AUTHOR_DATE", "@1700000000 +0000")
            .env("GIT_COMMITTER_DATE", "@1700000000 +0000")
            .args([
                "-c",
                "commit.gpgSign=false",
                "-c",
                "core.autocrlf=false",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.longpaths=true",
            ])
            .arg("-c")
            .arg(format!(
                "core.hooksPath={}",
                crate::git::argument(self.path.join("no-hooks").as_os_str()).to_string_lossy()
            ))
            .args(args.iter().map(|arg| crate::git::argument(std::ffi::OsStr::new(arg))))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn commit(&self, file: &str, content: &str, message: &str) -> String {
        fs::write(self.work.join(file), content).unwrap();
        self.git(&["add", "--", file]);
        self.git(&["commit", "-m", message]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn bundle(&self) -> Vec<u8> {
        let output = self.path.join(format!("{}.bundle", uuid::Uuid::new_v4()));
        self.git(&["bundle", "create", output.to_str().unwrap(), "--all"]);
        fs::read(output).unwrap()
    }

    fn seed(&self) -> String {
        self.source
            .import_bundle(&self.admin, &self.project.id, &self.bundle(), true)
            .unwrap();
        self.local()
            .head(&self.project.target_branch)
            .unwrap()
            .unwrap()
    }

    fn local(&self) -> Git {
        Git::new(self.source.project_path(&self.project.id).unwrap()).unwrap()
    }

    fn worker(&self, project: &Project) -> (Enrollment, Principal) {
        let invitation: Invitation = serde_json::from_value(execute(
            &self.store,
            &self.admin,
            Command::CreateInvitation {
                project_id: project.id.clone(),
                host_name: "Fixture coordinator".into(),
                expires_in_seconds: 3600,
            },
        ))
        .unwrap();
        let enrolled = self
            .store
            .enroll(
                &invitation.token,
                WorkerRegistration {
                    name: "Fixture worker".into(),
                    physical_host_id: "fixture-host".into(),
                    os: "test".into(),
                    architecture: "test".into(),
                    capabilities: vec![],
                    capacity: Resources {
                        agent_slots: 8,
                        cpu_millis: 0,
                        memory_mb: 0,
                        disk_mb: 0,
                    },
                    omp_version: "fixture-pinned-version".into(),
                    policy_revision: project.policy_revision,
                    owner_consented: true,
                    consent_host: invitation.host_name,
                },
            )
            .unwrap();
        execute(
            &self.store,
            &self.admin,
            Command::ApproveWorker {
                worker_id: enrolled.worker.id.clone(),
            },
        );
        let principal = self.store.authenticate(&enrolled.device_token).unwrap();
        execute(
            &self.store,
            &principal,
            Command::ReportRuntimeStatus {
                status: dacs_protocol::WorkerRuntimeStatus {
                    worker_id: enrolled.worker.id.clone(),
                    provider_authenticated: Some(true),
                    network_ready: None,
                    desktop_session: None,
                    desktop_active: false,
                    desktop_paused: false,
                    observed_at: 0,
                },
            },
        );
        (enrolled, principal)
    }

    fn candidate(&self, sha: &str, base: &str) -> CandidateRecord {
        self.candidate_with_settlement(sha, base, true).0
    }

    fn candidate_with_settlement(&self, sha: &str, base: &str, settled: bool) -> (CandidateRecord, Principal) {
        let (worker, principal) = self.worker(&self.project);
        let task: TaskRecord = serde_json::from_value(execute(
            &self.store,
            &self.admin,
            Command::CreateTask {
                spec: TaskSpec {
                    project_id: self.project.id.clone(),
                    title: "Integrate fixture".into(),
                    instructions: "Fixture source publication".into(),
                    source_commit: base.into(),
                    execution: dacs_protocol::TaskExecution::Agent,
                    dependencies: vec![],
                    required_capabilities: vec![],
                    resources: Resources {
                        agent_slots: 1,
                        cpu_millis: 0,
                        memory_mb: 0,
                        disk_mb: 0,
                    },
                    effect_class: EffectClass::Workspace,
                    lease_seconds: 3600,
                    max_runtime_seconds: 3600,
                    contract_revision: self.project.contract_revision,
                    policy_revision: self.project.policy_revision,
                    max_retries: 1,
                },
            },
        ))
        .unwrap();
        let attempt: AttemptRecord = serde_json::from_value(execute(
            &self.store,
            &self.admin,
            Command::AdmitTask {
                task_id: task.id.clone(),
                worker_id: Some(worker.worker.id),
            },
        ))
        .unwrap();
        if settled {
            execute(&self.store, &principal, Command::ReportEvent {
                attempt_id: attempt.id.clone(), generation: attempt.generation,
                sequence: 1, event: dacs_protocol::WorkerEvent::Settled,
            });
        }
        let candidate = serde_json::from_value(execute(
            &self.store,
            &principal,
            Command::SubmitCandidate {
                submission: CandidateSubmission {
                    task_id: task.id,
                    attempt_id: attempt.id,
                    generation: attempt.generation,
                    commit_sha: sha.into(),
                    contract_revision: self.project.contract_revision,
                    policy_revision: self.project.policy_revision,
                    evidence: vec![],
                    unresolved: vec![],
                },
            },
        ))
        .unwrap();
        (candidate, principal)
    }

    fn approve(&self, candidate: &CandidateRecord) {
        execute(
            &self.store,
            &self.admin,
            Command::ReviewCandidate {
                candidate_id: candidate.id.clone(),
                decision: ReviewDecision {
                    candidate_sha: candidate.integration_sha.clone().unwrap(),
                    approve: true,
                    reason: "Test fixture exact-commit review".into(),
                    checks_run: vec![],
                    skipped_checks: vec![
                        "Fixture intentionally tests publication, not application validation"
                            .into(),
                    ],
                },
            },
        );
    }
}

fn execute(store: &Store, principal: &Principal, command: Command) -> Value {
    store
        .execute(
            principal,
            CommandEnvelope {
                protocol_version: PROTOCOL_VERSION,
                command_id: uuid::Uuid::new_v4().to_string(),
                command,
            },
        )
        .unwrap()
        .result
}

#[test]
fn divergent_candidate_is_merged_staged_reviewed_and_published() {
    let fixture = Fixture::new();
    let base = fixture.seed();
    fixture.git(&["checkout", "-b", "candidate"]);
    let submitted = fixture.commit("candidate.txt", "candidate\n", "candidate");
    fixture.git(&["checkout", "main"]);
    let current = fixture.commit("current.txt", "current\n", "current");
    fixture
        .source
        .import_bundle(
            &fixture.admin,
            &fixture.project.id,
            &fixture.bundle(),
            false,
        )
        .unwrap();
    assert_eq!(
        fixture
            .local()
            .head(&fixture.project.target_branch)
            .unwrap()
            .as_deref(),
        Some(base.as_str())
    );
    fixture
        .local()
        .update(&fixture.project.target_branch, &current, Some(&base))
        .unwrap();
    let candidate = fixture.candidate(&submitted, &base);
    let staged = fixture
        .source
        .prepare(&fixture.admin, &candidate.id)
        .unwrap();
    let merged = staged.integration_sha.as_ref().unwrap();
    assert_ne!(merged, &submitted);
    assert_eq!(staged.expected_head.as_deref(), Some(current.as_str()));
    assert_eq!(
        fixture
            .local()
            .run(["show", "-s", "--format=%P", merged])
            .unwrap()
            .text()
            .unwrap()
            .trim(),
        format!("{current} {submitted}")
    );
    assert_eq!(
        fixture
            .local()
            .run(["show", &format!("{merged}:candidate.txt")])
            .unwrap()
            .text()
            .unwrap(),
        "candidate\n"
    );
    assert_eq!(
        fixture
            .local()
            .run(["show", &format!("{merged}:current.txt")])
            .unwrap()
            .text()
            .unwrap(),
        "current\n"
    );
    assert!(
        fixture
            .source
            .publish(&fixture.admin, &candidate.id)
            .is_err()
    );
    fixture.approve(&staged);
    let repeated = fixture
        .source
        .prepare(&fixture.admin, &candidate.id)
        .unwrap();
    assert_eq!(repeated.integration_sha, staged.integration_sha);
    assert!(repeated.decision.is_some());
    let result = fixture
        .source
        .publish(&fixture.admin, &candidate.id)
        .unwrap();
    assert_eq!(result["state"], "published");
    assert_eq!(
        fixture
            .source
            .publish(&fixture.admin, &candidate.id)
            .unwrap()["state"],
        "published"
    );
    assert_eq!(
        fixture
            .local()
            .head(&fixture.project.target_branch)
            .unwrap()
            .as_deref(),
        Some(merged.as_str())
    );
    let snapshot = fixture.store.snapshot(&fixture.admin).unwrap();
    assert!(
        snapshot
            .tasks
            .iter()
            .any(|task| task.id == candidate.submission.task_id
                && task.state == TaskState::Accepted
                && task.integrated_sha.as_deref() == Some(merged))
    );
    let exported = fixture
        .source
        .export_bundle(&fixture.admin, &fixture.project.id)
        .unwrap();
    assert!(!exported.is_empty());
}

#[test]
fn merge_conflict_does_not_stage_or_mutate_target() {
    let fixture = Fixture::new();
    let base = fixture.seed();
    fixture.git(&["checkout", "-b", "candidate"]);
    let submitted = fixture.commit("shared.txt", "candidate change\n", "candidate");
    fixture.git(&["checkout", "main"]);
    let current = fixture.commit("shared.txt", "current change\n", "current");
    fixture
        .source
        .import_bundle(
            &fixture.admin,
            &fixture.project.id,
            &fixture.bundle(),
            false,
        )
        .unwrap();
    fixture
        .local()
        .update(&fixture.project.target_branch, &current, Some(&base))
        .unwrap();
    let candidate = fixture.candidate(&submitted, &base);
    assert_eq!(
        fixture
            .source
            .prepare(&fixture.admin, &candidate.id)
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    assert!(
        fixture
            .store
            .candidate(&fixture.admin, &candidate.id)
            .unwrap()
            .integration_sha
            .is_none()
    );
    assert_eq!(
        fixture
            .local()
            .head(&fixture.project.target_branch)
            .unwrap()
            .as_deref(),
        Some(current.as_str())
    );
}

#[test]
fn changing_target_requires_new_exact_sha_approval() {
    let fixture = Fixture::new();
    let base = fixture.seed();
    fixture.git(&["checkout", "-b", "candidate"]);
    let submitted = fixture.commit("candidate.txt", "candidate\n", "candidate");
    fixture.git(&["checkout", "main"]);
    let current = fixture.commit("current.txt", "current\n", "current");
    fixture
        .source
        .import_bundle(
            &fixture.admin,
            &fixture.project.id,
            &fixture.bundle(),
            false,
        )
        .unwrap();
    let candidate = fixture.candidate(&submitted, &base);
    let initial = fixture
        .source
        .prepare(&fixture.admin, &candidate.id)
        .unwrap();
    fixture.approve(&initial);
    fixture
        .local()
        .update(&fixture.project.target_branch, &current, Some(&base))
        .unwrap();
    assert_eq!(
        fixture
            .source
            .publish(&fixture.admin, &candidate.id)
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    let updated = fixture
        .source
        .prepare(&fixture.admin, &candidate.id)
        .unwrap();
    assert_ne!(updated.integration_sha, initial.integration_sha);
    assert!(updated.decision.is_none());
    assert!(
        fixture
            .source
            .publish(&fixture.admin, &candidate.id)
            .is_err()
    );
}

#[test]
fn restart_observes_candidate_expected_or_ambiguous_ref_without_replay() {
    for outcome in ["candidate", "expected", "ambiguous"] {
        let fixture = Fixture::new();
        let base = fixture.seed();
        fixture.git(&["checkout", "-b", "candidate"]);
        let submitted = fixture.commit("candidate.txt", "candidate\n", "candidate");
        fixture.git(&["checkout", "main"]);
        let other = fixture.commit("other.txt", "other\n", "other");
        fixture
            .source
            .import_bundle(
                &fixture.admin,
                &fixture.project.id,
                &fixture.bundle(),
                false,
            )
            .unwrap();
        let candidate = fixture.candidate(&submitted, &base);
        let staged = fixture
            .source
            .prepare(&fixture.admin, &candidate.id)
            .unwrap();
        fixture.approve(&staged);
        let publication = fixture
            .store
            .prepare_publication(&fixture.admin, &candidate.id)
            .unwrap();
        match outcome {
            "candidate" => fixture
                .local()
                .update(
                    &fixture.project.target_branch,
                    &publication.candidate_sha,
                    Some(&base),
                )
                .unwrap(),
            "ambiguous" => fixture
                .local()
                .update(&fixture.project.target_branch, &other, Some(&base))
                .unwrap(),
            _ => {}
        }
        // Reopen SQLite and source service at the durable crash boundary, before
        // finish_publication; no SQL is written directly by the source crate.
        let reopened = Store::open(fixture.path.join("state.sqlite"), ADMIN_TOKEN).unwrap();
        let restarted = SourceService::new(fixture.path.join("source"), reopened.clone());
        let snapshot = reopened.snapshot(&fixture.admin).unwrap();
        let task = snapshot
            .tasks
            .iter()
            .find(|task| task.id == candidate.submission.task_id)
            .unwrap();
        match outcome {
            "candidate" => {
                assert!(restarted.is_ok());
                assert_eq!(task.state, TaskState::Accepted);
                assert_eq!(task.integrated_sha.as_deref(), Some(submitted.as_str()));
                assert!(reopened.pending_publications().unwrap().is_empty());
            }
            "expected" => {
                assert!(restarted.is_ok());
                assert_eq!(task.state, TaskState::Validating);
                assert!(reopened.pending_publications().unwrap().is_empty());
                assert_eq!(
                    fixture
                        .local()
                        .head(&fixture.project.target_branch)
                        .unwrap()
                        .as_deref(),
                    Some(base.as_str())
                );
                assert!(
                    reopened
                        .candidate(&fixture.admin, &candidate.id)
                        .unwrap()
                        .decision
                        .is_none()
                );
            }
            _ => {
                assert_eq!(restarted.err().unwrap().code, ErrorCode::Conflict);
                assert_eq!(task.state, TaskState::Publishing);
                assert_eq!(
                    reopened.pending_publications().unwrap()[0].state,
                    "reconciling"
                );
                assert_eq!(
                    fixture
                        .local()
                        .head(&fixture.project.target_branch)
                        .unwrap()
                        .as_deref(),
                    Some(other.as_str())
                );
            }
        }
    }
}

#[test]
fn artifacts_are_verified_immutable_and_project_authorized() {
    let fixture = Fixture::new();
    let (_, worker) = fixture.worker(&fixture.project);
    let other: Project = serde_json::from_value(execute(
        &fixture.store,
        &fixture.admin,
        Command::CreateProject {
            name: "Other project".into(),
            target_branch: "main".into(),
            max_tasks: 10,
            max_depth: 2,
        },
    ))
    .unwrap();
    let bytes = b"immutable evidence";
    let digest = hex::encode(Sha256::digest(bytes));
    assert_eq!(
        fixture
            .source
            .put_artifact(&worker, &fixture.project.id, bytes, Some(&digest))
            .unwrap()["created"],
        true
    );
    assert_eq!(
        fixture
            .source
            .put_artifact(&worker, &fixture.project.id, bytes, Some(&digest))
            .unwrap()["created"],
        false
    );
    assert_eq!(
        fixture
            .source
            .get_artifact(&worker, &fixture.project.id, &digest)
            .unwrap(),
        bytes
    );
    assert_eq!(
        fixture
            .source
            .get_artifact(&worker, &other.id, &digest)
            .unwrap_err()
            .code,
        ErrorCode::Forbidden
    );
    assert_eq!(
        fixture
            .source
            .get_artifact(&fixture.admin, &other.id, &digest)
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    assert_eq!(
        fixture
            .source
            .put_artifact(&worker, &other.id, bytes, Some(&digest))
            .unwrap_err()
            .code,
        ErrorCode::Forbidden
    );
    assert_eq!(
        fixture
            .source
            .put_artifact(&worker, &fixture.project.id, b"different", Some(&digest))
            .unwrap_err()
            .code,
        ErrorCode::Invalid
    );
    assert_eq!(
        fixture
            .source
            .get_artifact(&worker, &fixture.project.id, "../outside")
            .unwrap_err()
            .code,
        ErrorCode::Invalid
    );
    let object = fixture
        .source
        .project_path(&fixture.project.id)
        .unwrap()
        .join("artifacts")
        .join(&digest);
    fs::write(&object, b"corrupted").unwrap();
    assert_eq!(
        fixture
            .source
            .put_artifact(&worker, &fixture.project.id, bytes, Some(&digest))
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    assert_eq!(fs::read(&object).unwrap(), b"corrupted");
    assert_eq!(
        fixture
            .source
            .get_artifact(&worker, &fixture.project.id, &digest)
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
}

#[test]
fn only_admin_can_seed_and_existing_targets_cannot_be_replaced() {
    let fixture = Fixture::new();
    let (_, worker) = fixture.worker(&fixture.project);
    let bundle = fixture.bundle();
    assert_eq!(
        fixture
            .source
            .import_bundle(&worker, &fixture.project.id, &bundle, true)
            .unwrap_err()
            .code,
        ErrorCode::Forbidden
    );
    fixture
        .source
        .import_bundle(&fixture.admin, &fixture.project.id, &bundle, true)
        .unwrap();
    assert_eq!(
        fixture
            .source
            .import_bundle(&fixture.admin, &fixture.project.id, &bundle, true)
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    assert!(
        fixture
            .source
            .import_bundle(&worker, &fixture.project.id, &bundle, false)
            .is_ok()
    );
    let target = fixture
        .local()
        .head(&fixture.project.target_branch)
        .unwrap()
        .unwrap();
    assert!(
        fixture
            .local()
            .update(
                &fixture.project.target_branch,
                &target,
                Some(&"1".repeat(40))
            )
            .is_err()
    );
    assert_eq!(
        fixture
            .local()
            .head(&fixture.project.target_branch)
            .unwrap()
            .as_deref(),
        Some(target.as_str())
    );
}

#[cfg(unix)]
#[test]
fn artifact_symlink_cannot_escape_private_storage() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let digest = hex::encode(Sha256::digest(b"outside"));
    let directory = fixture
        .source
        .project_path(&fixture.project.id)
        .unwrap()
        .join("artifacts");
    storage::directory(&directory).unwrap();
    let outside = fixture.path.join("outside");
    fs::write(&outside, b"outside").unwrap();
    symlink(&outside, directory.join(&digest)).unwrap();
    assert_eq!(
        fixture
            .source
            .get_artifact(&fixture.admin, &fixture.project.id, &digest)
            .unwrap_err()
            .code,
        ErrorCode::Forbidden
    );
    assert_eq!(
        fixture
            .source
            .put_artifact(
                &fixture.admin,
                &fixture.project.id,
                b"outside",
                Some(&digest)
            )
            .unwrap_err()
            .code,
        ErrorCode::Forbidden
    );
    assert_eq!(fs::read(outside).unwrap(), b"outside");
}

#[test]
fn candidate_diff_uses_task_base_then_exact_staged_merge_and_checks_authorization() {
    let fixture = Fixture::new();
    let base = fixture.seed();
    fixture.git(&["checkout", "-b", "candidate"]);
    let submitted = fixture.commit("candidate.txt", "visible candidate change\n", "candidate");
    fixture.git(&["checkout", "main"]);
    let current = fixture.commit("current.txt", "already integrated\n", "current");
    fixture
        .source
        .import_bundle(
            &fixture.admin,
            &fixture.project.id,
            &fixture.bundle(),
            false,
        )
        .unwrap();
    let candidate = fixture.candidate(&submitted, &base);
    let before = fixture.source.diff(&fixture.admin, &candidate.id).unwrap();
    assert_eq!(before["base_sha"], base);
    assert_eq!(before["commit_sha"], submitted);
    assert!(before["integration_sha"].is_null());
    assert!(
        before["diff"]
            .as_str()
            .unwrap()
            .contains("+visible candidate change")
    );
    assert_eq!(before["truncated"], false);
    fixture
        .local()
        .update(&fixture.project.target_branch, &current, Some(&base))
        .unwrap();
    let staged = fixture
        .source
        .prepare(&fixture.admin, &candidate.id)
        .unwrap();
    let preview = fixture.source.diff(&fixture.admin, &candidate.id).unwrap();
    assert_eq!(preview["base_sha"], current);
    assert_eq!(
        preview["integration_sha"].as_str(),
        staged.integration_sha.as_deref()
    );
    assert!(
        preview["diff"]
            .as_str()
            .unwrap()
            .contains("+visible candidate change")
    );
    assert!(
        !preview["diff"]
            .as_str()
            .unwrap()
            .contains("already integrated")
    );
    assert!(preview["stat"].as_str().unwrap().contains("candidate.txt"));
    assert_eq!(
        fixture
            .local()
            .head(&fixture.project.target_branch)
            .unwrap()
            .as_deref(),
        Some(current.as_str())
    );
    let other: Project = serde_json::from_value(execute(
        &fixture.store,
        &fixture.admin,
        Command::CreateProject {
            name: "Other diff project".into(),
            target_branch: "main".into(),
            max_tasks: 10,
            max_depth: 2,
        },
    ))
    .unwrap();
    let (_, other_worker) = fixture.worker(&other);
    assert_eq!(
        fixture
            .source
            .diff(&other_worker, &candidate.id)
            .unwrap_err()
            .code,
        ErrorCode::Forbidden
    );
}

#[test]
fn candidate_diff_is_explicitly_truncated_at_bounded_output() {
    let fixture = Fixture::new();
    let base = fixture.seed();
    let submitted = fixture.commit(
        "large.txt",
        &"visible candidate line\n".repeat(60_000),
        "large candidate",
    );
    fixture
        .source
        .import_bundle(
            &fixture.admin,
            &fixture.project.id,
            &fixture.bundle(),
            false,
        )
        .unwrap();
    let candidate = fixture.candidate(&submitted, &base);
    let preview = fixture.source.diff(&fixture.admin, &candidate.id).unwrap();
    assert_eq!(preview["truncated"], true);
    assert_eq!(preview["diff"].as_str().unwrap().len(), 1024 * 1024);
    assert!(preview["stat"].as_str().unwrap().contains("large.txt"));
    assert_eq!(
        fixture
            .local()
            .head(&fixture.project.target_branch)
            .unwrap()
            .as_deref(),
        Some(base.as_str())
    );
}

#[test]
fn staged_empty_target_diff_includes_the_entire_published_tree() {
    let fixture = Fixture::new();
    let base = fixture.git(&["rev-parse", "HEAD"]);
    let submitted = fixture.commit("candidate.txt", "new candidate file\n", "candidate");
    fixture.source.import_bundle(&fixture.admin, &fixture.project.id, &fixture.bundle(), false).unwrap();
    let candidate = fixture.candidate(&submitted, &base);
    let before = fixture.source.diff(&fixture.admin, &candidate.id).unwrap();
    assert_eq!(before["base_sha"], base);
    assert!(!before["diff"].as_str().unwrap().contains("shared.txt"));
    let staged = fixture.source.prepare(&fixture.admin, &candidate.id).unwrap();
    assert!(staged.expected_head.is_none());
    assert_eq!(staged.integration_sha.as_deref(), Some(submitted.as_str()));
    let preview = fixture.source.diff(&fixture.admin, &candidate.id).unwrap();
    assert_eq!(preview["base_sha"], "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
    let diff = preview["diff"].as_str().unwrap();
    assert!(diff.contains("shared.txt"));
    assert!(diff.contains("+base"));
    assert!(diff.contains("+new candidate file"));
    assert!(fixture.local().head(&fixture.project.target_branch).unwrap().is_none());
}

#[test]
fn oversized_import_preserves_live_objects_export_and_pending_reconciliation() {
    for already_published in [false, true] {
        let fixture = Fixture::new();
        let base = fixture.seed();
        let submitted = fixture.commit("candidate.txt", "approved candidate\n", "candidate");
        fixture.source.import_bundle(&fixture.admin, &fixture.project.id, &fixture.bundle(), false).unwrap();
        let candidate = fixture.candidate(&submitted, &base);
        let staged = fixture.source.prepare(&fixture.admin, &candidate.id).unwrap();
        fixture.approve(&staged);
        let publication = fixture.store.prepare_publication(&fixture.admin, &candidate.id).unwrap();
        if already_published {
            fixture.local().update(&fixture.project.target_branch, &publication.candidate_sha, Some(&base)).unwrap();
        }
        let expected_head = fixture.local().head(&fixture.project.target_branch).unwrap();
        let mut content = String::with_capacity(256 * 1024);
        let mut state = 0x6a09e667f3bcc909u64;
        for _ in 0..256 * 1024 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            content.push((b'!' + (state % 90) as u8) as char);
        }
        fixture.commit("oversized.txt", &content, "must remain quarantined");
        let bundle = fixture.bundle();
        let local = fixture.local();
        let existing = storage::tree_size(&local.repo, storage::MAX_REPOSITORY).unwrap();
        // Sparse padding gives the actual production quota boundary without
        // allocating or writing half a gigabyte of fixture data.
        fs::File::create(local.repo.join("quota-fixture")).unwrap()
            .set_len(storage::MAX_REPOSITORY - existing - 8192).unwrap();
        let before = storage::tree_size(&local.repo, storage::MAX_REPOSITORY).unwrap();
        let rejected = fixture.source.import_bundle(&fixture.admin, &fixture.project.id, &bundle, false).unwrap_err();
        assert_eq!(rejected.code, ErrorCode::Invalid);
        assert!(rejected.message.contains("Import would exceed"));
        assert_eq!(storage::tree_size(&local.repo, storage::MAX_REPOSITORY).unwrap(), before);
        assert_eq!(local.head(&fixture.project.target_branch).unwrap(), expected_head);
        assert!(!fixture.source.export_bundle(&fixture.admin, &fixture.project.id).unwrap().is_empty());
        let reopened = Store::open(fixture.path.join("state.sqlite"), ADMIN_TOKEN).unwrap();
        let restarted = SourceService::new(fixture.path.join("source"), reopened.clone()).unwrap();
        assert!(reopened.pending_publications().unwrap().is_empty());
        let snapshot = reopened.snapshot(&fixture.admin).unwrap();
        let task = snapshot.tasks.iter().find(|task| task.id == candidate.submission.task_id).unwrap();
        assert_eq!(task.state, if already_published { TaskState::Accepted } else { TaskState::Validating });
        assert!(!restarted.export_bundle(&fixture.admin, &fixture.project.id).unwrap().is_empty());
        assert_eq!(local.head(&fixture.project.target_branch).unwrap(), expected_head);
    }
}

#[test]
fn incremental_bundle_uses_live_prerequisites_without_retaining_alternates() {
    let fixture = Fixture::new();
    let base = fixture.seed();
    let submitted = fixture.commit("incremental.txt", "incremental object\n", "incremental");
    let bundle_path = fixture.path.join("incremental.bundle");
    fixture.git(&["bundle", "create", bundle_path.to_str().unwrap(), "main", &format!("^{base}")]);
    fixture.source.import_bundle(&fixture.admin, &fixture.project.id, &fs::read(bundle_path).unwrap(), false).unwrap();
    let local = fixture.local();
    local.commit(&submitted).unwrap();
    assert_eq!(local.run(["show", &format!("{submitted}:incremental.txt")]).unwrap().text().unwrap(), "incremental object\n");
    assert!(!local.repo.join("objects/info/alternates").exists());
    assert_eq!(local.head(&fixture.project.target_branch).unwrap().as_deref(), Some(base.as_str()));
    assert_eq!(fs::read_dir(&local.temporary).unwrap().count(), 0);
}

#[test]
fn restart_cleanup_removes_over_budget_abandoned_quarantine() {
    let fixture = Fixture::new();
    let base = fixture.seed();
    let local = fixture.local();
    let abandoned = local.temporary.join("import-interrupted").join("repository.git/objects/pack");
    storage::directory(&abandoned).unwrap();
    fs::File::create(abandoned.join("interrupted.pack")).unwrap()
        .set_len(storage::MAX_TEMPORARY + 1).unwrap();
    assert!(!fixture.source.export_bundle(&fixture.admin, &fixture.project.id).unwrap().is_empty());
    assert!(!local.temporary.join("import-interrupted").exists());
    assert_eq!(local.head(&fixture.project.target_branch).unwrap().as_deref(), Some(base.as_str()));
}

#[test]
fn publication_of_live_candidate_waits_for_positive_settlement() {
    let fixture = Fixture::new();
    let base = fixture.seed();
    let submitted = fixture.commit("candidate.txt", "candidate still executing\n", "candidate");
    fixture.source.import_bundle(&fixture.admin, &fixture.project.id, &fixture.bundle(), false).unwrap();
    let (candidate, worker) = fixture.candidate_with_settlement(&submitted, &base, false);
    let staged = fixture.source.prepare(&fixture.admin, &candidate.id).unwrap();
    fixture.approve(&staged);
    assert_eq!(fixture.source.publish(&fixture.admin, &candidate.id).unwrap_err().code, ErrorCode::Conflict);
    assert!(fixture.store.pending_publications().unwrap().is_empty());
    assert_eq!(fixture.local().head(&fixture.project.target_branch).unwrap().as_deref(), Some(base.as_str()));
    execute(&fixture.store, &worker, Command::ReportEvent {
        attempt_id: candidate.submission.attempt_id.clone(), generation: candidate.submission.generation,
        sequence: 1, event: dacs_protocol::WorkerEvent::Settled,
    });
    assert_eq!(fixture.source.publish(&fixture.admin, &candidate.id).unwrap()["state"], "published");
}
