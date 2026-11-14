//! Crash/reconnect of the real worker loop (`runtime::run`) with live processes.
//!
//! The worker runs in a separate process (this test binary re-executed) against
//! an in-process fake coordinator and a scripted fake OMP executable. The fake
//! OMP speaks the JSONL RPC frames the runtime needs and never contacts a model
//! provider. Each OMP attempt process leads a process group with a long-lived
//! descendant, and outlives its RPC transport after stdin closes, so a crash
//! (SIGKILL of the worker) really leaves supervised work running.
#![cfg(unix)]

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
};
use dacs_protocol::*;
use dacs_worker::runtime::WorkerConfig;
use nix::{errno::Errno, sys::signal::kill, unistd::Pid};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Child, Command as Process, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const CONFIG_ENV: &str = "DACS_CRASH_RECONNECT_CONFIG";
const WORKER: &str = "worker-1";
const PROJECT: &str = "project-1";
const TASK: &str = "task-1";
const ATTEMPT: &str = "attempt-1";

/// Runs only when re-executed by a test below; a plain `--ignored` run returns.
#[test]
#[ignore = "worker process for crash_reconnect tests"]
fn worker_process_helper() {
    let Some(path) = std::env::var_os(CONFIG_ENV) else {
        return;
    };
    let config: WorkerConfig = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(dacs_worker::runtime::run(config))
        .unwrap();
}

fn alive(pid: i32) -> bool {
    match kill(Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        Err(error) => panic!("cannot inspect {pid}: {error}"),
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[derive(Clone, Debug)]
struct Logged {
    phase: u32,
    id: String,
    command: Value,
    status: u16,
    /// Liveness of the watched pids when a stop/reconciliation arrived.
    watched_alive: Vec<bool>,
}

impl Logged {
    fn kind(&self) -> &str {
        self.command["type"].as_str().unwrap_or("")
    }
    fn event(&self) -> &str {
        self.command["event"]["type"].as_str().unwrap_or("")
    }
    fn releases(&self, attempt: &str) -> bool {
        (self.kind() == "report_event"
            && self.event() == "stopped"
            && self.command["attempt_id"] == attempt)
            || (self.kind() == "reconcile_worker"
                && self.command["stopped_attempt_ids"]
                    .as_array()
                    .is_some_and(|ids| ids.iter().any(|id| id == attempt)))
    }
}

#[derive(Default)]
struct Fake {
    phase: u32,
    snapshot: Snapshot,
    messages: Vec<ControlMessage>,
    log: Vec<Logged>,
    refuse_usage: bool,
    watch: Vec<i32>,
    bundle: Vec<u8>,
}

type Shared = Arc<Mutex<Fake>>;

impl Fake {
    fn attempt(&mut self, id: &str) -> Option<&mut AttemptRecord> {
        self.snapshot.attempts.iter_mut().find(|a| a.id == id)
    }

    fn apply(&mut self, command: &Command) -> (StatusCode, Value) {
        if let Command::ReportEvent {
            attempt_id,
            sequence,
            event,
            ..
        } = command
        {
            if self.refuse_usage && matches!(event, WorkerEvent::Usage { .. }) {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    json!({"error":"temporarily unavailable"}),
                );
            }
            let Some(attempt) = self.attempt(attempt_id) else {
                return (StatusCode::NOT_FOUND, json!({"error":"unknown attempt"}));
            };
            let stopped = matches!(event, WorkerEvent::Stopped);
            if matches!(attempt.state.as_str(), "fenced" | "abandoned") && !stopped {
                return (
                    StatusCode::CONFLICT,
                    json!({"error":"stale or terminal attempt generation"}),
                );
            }
            attempt.last_sequence = *sequence;
            match event {
                WorkerEvent::Started | WorkerEvent::Progress { .. } => {
                    attempt.state = "executing".into();
                    attempt.settled = false;
                }
                WorkerEvent::Settled => attempt.settled = true,
                WorkerEvent::Failed { .. } => attempt.state = "failed".into(),
                WorkerEvent::Stopped if attempt.state != "fenced" => {
                    attempt.state = "stopped".into()
                }
                _ => (),
            }
        }
        (StatusCode::OK, json!({}))
    }
}

async fn command(
    State(shared): State<Shared>,
    Json(envelope): Json<CommandEnvelope>,
) -> (StatusCode, Json<Value>) {
    let mut fake = shared.lock().unwrap();
    let (status, result) = fake.apply(&envelope.command);
    let mut logged = Logged {
        phase: fake.phase,
        id: envelope.command_id.clone(),
        command: serde_json::to_value(&envelope.command).unwrap(),
        status: status.as_u16(),
        watched_alive: Vec::new(),
    };
    if logged.releases(ATTEMPT) {
        logged.watched_alive = fake.watch.iter().map(|pid| alive(*pid)).collect();
    }
    fake.log.push(logged);
    if status.is_success() {
        (
            status,
            Json(json!({"command_id":envelope.command_id,"result":result})),
        )
    } else {
        (status, Json(result))
    }
}

async fn commands(
    State(shared): State<Shared>,
    Query(query): Query<BTreeMap<String, i64>>,
) -> Json<Vec<ControlMessage>> {
    let after = query.get("after").copied().unwrap_or(0);
    let fake = shared.lock().unwrap();
    Json(
        fake.messages
            .iter()
            .filter(|m| m.sequence > after)
            .cloned()
            .collect(),
    )
}

async fn state(State(shared): State<Shared>) -> Json<Snapshot> {
    Json(shared.lock().unwrap().snapshot.clone())
}

async fn source_bundle(State(shared): State<Shared>) -> Vec<u8> {
    shared.lock().unwrap().bundle.clone()
}

async fn accept(_body: Bytes) -> Json<Value> {
    Json(json!({"ok":true}))
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Process::new("git")
        .args([
            "-c",
            "user.name=DACS crash fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Scripted stand-in for the pinned OMP executable. Records launches, process
/// trees and every received RPC frame under `log`.
fn fake_omp(directory: &Path, log: &Path) -> PathBuf {
    let script = r#"#!/bin/sh
LOG='__LOG__'
if [ "$1" = "--version" ]; then echo "omp/18.20.0"; exit 0; fi
mode=attempt
for argument in "$@"; do [ "$argument" = "--no-tools" ] && mode=probe; done
if [ "$mode" = attempt ]; then
  # Restart policy check: no earlier tree may still be alive at relaunch.
  for old in $(cat "$LOG/old-pids" 2>/dev/null); do
    kill -0 "$old" 2>/dev/null && echo "$$ $old" >> "$LOG/overlap"
  done
  sleep 600 &
  echo "$$ $!" >> "$LOG/trees"
fi
echo "$$ $mode" >> "$LOG/launches"
printf '%s\n' '{"type":"ready","protocolVersion":1}'
awk -v logf="$LOG/frames-$$" -v mode="$mode" '
function out(s) { print s; fflush() }
{
  print $0 >> logf; fflush(logf)
  id = ""
  if (match($0, /"id":"[^"]*"/)) id = substr($0, RSTART + 6, RLENGTH - 7)
  if ($0 ~ /"type":"host_tool_result"/) next
  if ($0 ~ /"type":"get_session_stats"/) {
    out("{\"type\":\"response\",\"id\":\"" id "\",\"success\":true,\"data\":{\"tokens\":{\"input\":11,\"output\":7}}}")
    next
  }
  if (id != "") out("{\"type\":\"response\",\"id\":\"" id "\",\"success\":true,\"data\":{}}")
  if ($0 ~ /"type":"prompt"/) {
    if (mode == "attempt") out("{\"type\":\"agent_end\"}")
    out("{\"type\":\"prompt_result\",\"id\":\"" id "\",\"status\":\"completed\",\"sessionSettled\":true}")
  }
}'
# An attempt process outlives its closed RPC transport, like a busy tool.
[ "$mode" = attempt ] && wait
exit 0
"#
    .replace("__LOG__", log.to_str().unwrap());
    let path = directory.join("fake-omp");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

struct Harness {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    log: PathBuf,
    config: PathBuf,
    data_dir: PathBuf,
    shared: Shared,
    _runtime: tokio::runtime::Runtime,
    workers: Vec<Child>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        for worker in &mut self.workers {
            let _ = worker.kill();
            let _ = worker.wait();
        }
        // Never leak fixture trees, even when an assertion failed.
        for (leader, _) in self.trees() {
            let _ =
                nix::sys::signal::killpg(Pid::from_raw(leader), nix::sys::signal::Signal::SIGKILL);
        }
    }
}

impl Harness {
    fn new(effect_class: EffectClass) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let log = root.join("omp-log");
        std::fs::create_dir_all(&log).unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();

        let source = root.join("source");
        std::fs::create_dir_all(&source).unwrap();
        git(&source, &["init", "--initial-branch=main"]);
        std::fs::write(source.join("README.md"), "crash fixture\n").unwrap();
        git(&source, &["add", "README.md"]);
        git(&source, &["commit", "-m", "fixture"]);
        let commit = git(&source, &["rev-parse", "HEAD"]);
        let bundle_path = root.join("source.bundle");
        git(
            &source,
            &["bundle", "create", bundle_path.to_str().unwrap(), "--all"],
        );

        let resources = Resources {
            agent_slots: 1,
            cpu_millis: 1000,
            memory_mb: 512,
            disk_mb: 512,
        };
        let registration = WorkerRegistration {
            name: "crash fixture".into(),
            physical_host_id: format!("host-{}", root.file_name().unwrap().to_string_lossy()),
            os: std::env::consts::OS.into(),
            architecture: std::env::consts::ARCH.into(),
            capabilities: vec![],
            capacity: Resources {
                agent_slots: 2,
                cpu_millis: 2000,
                memory_mb: 1024,
                disk_mb: 1024,
            },
            omp_version: "18.20.0".into(),
            policy_revision: 1,
            owner_consented: true,
            consent_host: "crash-fixture".into(),
        };
        let spec = TaskSpec {
            project_id: PROJECT.into(),
            title: "crash fixture".into(),
            instructions: "Do nothing; this OMP is scripted.".into(),
            source_commit: commit,
            execution: TaskExecution::Agent,
            dependencies: vec![],
            required_capabilities: vec![],
            resources: resources.clone(),
            effect_class,
            lease_seconds: 3600,
            max_runtime_seconds: 3600,
            contract_revision: 1,
            policy_revision: 1,
            max_retries: 1,
        };
        let task = TaskRecord {
            id: TASK.into(),
            spec,
            state: TaskState::Executing,
            generation: 1,
            parent_task_id: None,
            attempt_id: Some(ATTEMPT.into()),
            integrated_sha: None,
            workflow_result: None,
            blocked_reason: None,
            cancel_requested: false,
        };
        let attempt = AttemptRecord {
            id: ATTEMPT.into(),
            task_id: TASK.into(),
            worker_id: WORKER.into(),
            generation: 1,
            state: "admitted".into(),
            lease_expires_at: now() + 3600,
            started_at: now(),
            settled: false,
            pending_children: vec![],
            last_sequence: 0,
            stop_reason: None,
            resources: resources.clone(),
        };
        let fake = Fake {
            phase: 1,
            snapshot: Snapshot {
                projects: vec![Project {
                    id: PROJECT.into(),
                    name: "crash fixture".into(),
                    target_branch: "main".into(),
                    contract_revision: 1,
                    policy_revision: 1,
                    max_tasks: 4,
                    max_depth: 1,
                    requirements: json!({}),
                }],
                workers: vec![WorkerRecord {
                    id: WORKER.into(),
                    project_id: PROJECT.into(),
                    registration: registration.clone(),
                    approved: true,
                    revoked: false,
                    draining: false,
                    last_seen: now(),
                    reserved: resources,
                }],
                tasks: vec![task.clone()],
                attempts: vec![attempt.clone()],
                ..Snapshot::default()
            },
            messages: vec![ControlMessage {
                sequence: 1,
                id: "message-assignment".into(),
                worker_id: WORKER.into(),
                attempt_id: Some(ATTEMPT.into()),
                generation: Some(1),
                kind: "assignment".into(),
                payload: json!({"task":task,"attempt":attempt}),
            }],
            bundle: std::fs::read(&bundle_path).unwrap(),
            ..Fake::default()
        };
        let shared = Arc::new(Mutex::new(fake));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new()
            .route("/api/command", post(command))
            .route("/api/commands", get(commands))
            .route("/api/state", get(state))
            .route(
                "/api/projects/{project}/source",
                get(source_bundle).post(accept),
            )
            .route("/api/projects/{project}/artifacts/{digest}", post(accept))
            .with_state(shared.clone());
        runtime.spawn(async move { axum::serve(listener, app).await.unwrap() });

        let credential = root.join("device.token");
        std::fs::write(&credential, "fixture-device-token\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600)).unwrap();
        let data_dir = root.join("worker-data");
        let config = WorkerConfig {
            coordinator_url: format!("http://127.0.0.1:{port}"),
            credential_file: credential,
            worker_id: WORKER.into(),
            registration,
            omp_executable: fake_omp(&root, &log),
            omp_version: "18.20.0".into(),
            omp_profile: None,
            data_dir: data_dir.clone(),
            workspaces_dir: root.join("workspaces"),
            ca_certificate: None,
            poll_interval_ms: 100,
        };
        let config_path = root.join("worker.json");
        std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        Self {
            _temporary: temporary,
            root,
            log,
            config: config_path,
            data_dir,
            shared,
            _runtime: runtime,
            workers: Vec::new(),
        }
    }

    fn start_worker(&mut self) {
        let index = self.workers.len();
        let stderr =
            std::fs::File::create(self.root.join(format!("worker-{index}.stderr"))).unwrap();
        let child = Process::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "worker_process_helper",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CONFIG_ENV, &self.config)
            .env("HOME", self.root.join("home"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .unwrap();
        self.workers.push(child);
    }

    fn worker_log(&self) -> String {
        (0..self.workers.len())
            .map(|i| {
                std::fs::read_to_string(self.root.join(format!("worker-{i}.stderr")))
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join("\n---\n")
    }

    fn crash_worker(&mut self) {
        let worker = self.workers.last_mut().unwrap();
        worker.kill().unwrap(); // SIGKILL: no destructor or cleanup path runs.
        worker.wait().unwrap();
    }

    fn drain_and_wait(&mut self) {
        dacs_worker::local_control::request(
            &self.data_dir,
            dacs_worker::local_control::ControlAction::Drain,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        let worker = self.workers.last_mut().unwrap();
        loop {
            if let Some(status) = worker.try_wait().unwrap() {
                let log = self.worker_log();
                assert!(status.success(), "worker exited with {status}:\n{log}");
                return;
            }
            assert!(Instant::now() < deadline, "worker did not drain");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn lines(&self, name: &str) -> Vec<String> {
        std::fs::read_to_string(self.log.join(name))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// (leader, descendant) of every attempt-mode OMP launch, in order.
    fn trees(&self) -> Vec<(i32, i32)> {
        self.lines("trees")
            .iter()
            .map(|line| {
                let mut parts = line.split_whitespace().map(|p| p.parse().unwrap());
                (parts.next().unwrap(), parts.next().unwrap())
            })
            .collect()
    }

    fn frames(&self, leader: i32) -> String {
        std::fs::read_to_string(self.log.join(format!("frames-{leader}"))).unwrap_or_default()
    }

    fn log(&self) -> Vec<Logged> {
        self.shared.lock().unwrap().log.clone()
    }

    fn accepted(&self, phase: u32, event: &str) -> bool {
        self.log().iter().any(|l| {
            l.phase == phase && l.kind() == "report_event" && l.event() == event && l.status == 200
        })
    }

    fn wait_for(&self, what: &str, ready: impl FnMut() -> bool) {
        let started = Instant::now();
        let mut ready = ready;
        while !ready() {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "timed out waiting for {what}\n{}\ncoordinator log: {:#?}",
                self.worker_log(),
                self.log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn receipts(&self) -> usize {
        std::fs::read_dir(self.data_dir.join("processes"))
            .map(|entries| entries.count())
            .unwrap_or(0)
    }

    fn update(&self, change: impl FnOnce(&mut Fake)) {
        change(&mut self.shared.lock().unwrap());
    }
}

/// Phase 1: run an attempt until its OMP tree is live, then SIGKILL the worker
/// and prove the tree survived the crash.
fn run_until_crash(harness: &mut Harness) -> (i32, i32) {
    harness.start_worker();
    harness.wait_for("attempt OMP tree and Started", || {
        harness.trees().len() == 1 && harness.accepted(1, "started")
    });
    harness.wait_for("first OMP turn reported", || {
        harness
            .log()
            .iter()
            .any(|l| l.phase == 1 && l.kind() == "report_event" && l.event() == "usage")
    });
    harness.crash_worker();
    let (leader, descendant) = harness.trees()[0];
    assert!(
        alive(leader) && alive(descendant),
        "crash must leave the supervised tree running for this test to mean anything"
    );
    assert!(
        harness.receipts() >= 1,
        "process receipt must survive the crash"
    );
    std::fs::write(
        harness.log.join("old-pids"),
        format!("{leader} {descendant}\n"),
    )
    .unwrap();
    (leader, descendant)
}

#[test]
fn crash_then_fenced_attempt_is_killed_replayed_and_reconciled_before_release() {
    let mut harness = Harness::new(EffectClass::Workspace);
    // Usage reports are refused during phase 1, so they stay in the outbox.
    harness.update(|fake| fake.refuse_usage = true);
    let (leader, descendant) = run_until_crash(&mut harness);
    let undelivered: Vec<String> = harness
        .log()
        .iter()
        .filter(|l| l.phase == 1 && l.status == 503)
        .map(|l| l.id.clone())
        .collect();
    assert!(!undelivered.is_empty());

    // While the worker was down the coordinator fenced the replayable attempt
    // after lease expiry and queued its stop.
    harness.update(|fake| {
        fake.phase = 2;
        fake.refuse_usage = false;
        fake.watch = vec![leader, descendant];
        let attempt = fake.attempt(ATTEMPT).unwrap();
        attempt.state = "fenced".into();
        attempt.stop_reason = Some("workspace lease expired".into());
        let task = &mut fake.snapshot.tasks[0];
        task.state = TaskState::Ready;
        task.attempt_id = None;
        fake.messages.push(ControlMessage {
            sequence: 2,
            id: "message-stop".into(),
            worker_id: WORKER.into(),
            attempt_id: Some(ATTEMPT.into()),
            generation: Some(1),
            kind: "stop".into(),
            payload: json!({"reason":"lease expired","reservation_held_until_reconciliation":true}),
        });
    });
    harness.start_worker();
    harness.wait_for("fenced attempt reconciliation", || {
        harness
            .log()
            .iter()
            .any(|l| l.phase == 2 && l.releases(ATTEMPT))
    });
    harness.drain_and_wait();

    let log = harness.log();
    let phase2: Vec<&Logged> = log.iter().filter(|l| l.phase == 2).collect();
    for l in phase2.iter().filter(|l| {
        !matches!(
            l.kind(),
            "heartbeat" | "report_runtime_status" | "acknowledge_messages"
        )
    }) {
        eprintln!(
            "restart trace: {} {} {} -> {} watched_alive={:?}",
            l.id,
            l.kind(),
            l.event(),
            l.status,
            l.watched_alive
        );
    }
    // Results retained across the crash are replayed with their original ids.
    for id in &undelivered {
        assert!(
            phase2.iter().any(|l| &l.id == id),
            "result {id} was not replayed after restart"
        );
    }
    // Release is requested only after the replay and only once the old tree is
    // verifiably gone (checked by kill(2) when the request arrived).
    let release = phase2
        .iter()
        .position(|l| l.releases(ATTEMPT))
        .expect("release request");
    let last_replay = phase2
        .iter()
        .rposition(|l| undelivered.contains(&l.id))
        .unwrap();
    assert!(
        last_replay < release,
        "allocations released before results reconciled"
    );
    assert_eq!(phase2[release].watched_alive, vec![false, false]);
    // The fenced attempt was not resumed: no new attempt OMP was launched.
    assert_eq!(harness.trees().len(), 1);
    assert!(harness.lines("overlap").is_empty());
    assert!(!alive(leader) && !alive(descendant));
    assert_eq!(harness.receipts(), 0);
}

#[test]
fn crash_then_valid_attempt_relaunches_only_after_old_tree_is_gone() {
    let mut harness = Harness::new(EffectClass::Workspace);
    let (leader, descendant) = run_until_crash(&mut harness);
    harness.update(|fake| fake.phase = 2);
    harness.start_worker();
    harness.wait_for("resumed attempt OMP", || {
        harness.trees().len() == 2 && harness.accepted(2, "progress")
    });
    let (new_leader, new_descendant) = harness.trees()[1];
    // The fake OMP checked the old pids with kill -0 at its own launch.
    assert!(
        harness.lines("overlap").is_empty(),
        "new OMP launched while the crashed tree was alive: {:?}",
        harness.lines("overlap")
    );
    assert!(!alive(leader) && !alive(descendant));
    harness.wait_for("session reattached", || {
        harness
            .frames(new_leader)
            .contains(r#""type":"open_session""#)
    });
    // Old receipt cleared; only the relaunched OMP is accounted.
    assert_eq!(harness.receipts(), 1);

    // Teardown through the coordinator: the relaunched tree must be dead before
    // Stopped is reported.
    harness.update(|fake| {
        fake.watch = vec![new_leader, new_descendant];
        let attempt = fake.attempt(ATTEMPT).unwrap();
        attempt.state = "fenced".into();
        fake.snapshot.tasks[0].cancel_requested = true;
        fake.messages.push(ControlMessage {
            sequence: 2,
            id: "message-stop".into(),
            worker_id: WORKER.into(),
            attempt_id: Some(ATTEMPT.into()),
            generation: Some(1),
            kind: "stop".into(),
            payload: json!({"reason":"administrator cancelled task"}),
        });
    });
    harness.wait_for("stop reconciliation", || {
        harness
            .log()
            .iter()
            .any(|l| l.phase == 2 && l.releases(ATTEMPT))
    });
    harness.drain_and_wait();
    let log = harness.log();
    let release = log
        .iter()
        .find(|l| l.phase == 2 && l.releases(ATTEMPT))
        .unwrap();
    assert_eq!(release.watched_alive, vec![false, false]);
    assert!(!alive(new_leader) && !alive(new_descendant));
    assert_eq!(harness.receipts(), 0);
}
