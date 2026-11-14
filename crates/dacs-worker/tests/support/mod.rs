//! Shared harness: the real worker loop (`runtime::run`) in a separate process
//! (the test binary re-executed) against an in-process fake coordinator and a
//! scripted fake OMP executable. The fake OMP speaks the JSONL RPC frames the
//! runtime needs and never contacts a model provider. Each OMP attempt process
//! leads a process group with a long-lived descendant and outlives its RPC
//! transport after stdin closes, so a SIGKILLed worker really leaves supervised
//! work running.
#![allow(dead_code)] // each test binary uses a different subset

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
pub const CONFIG_ENV: &str = "DACS_WORKER_HARNESS_CONFIG";
/// Full libtest name of the re-executed worker helper below.
pub const HELPER_TEST: &str = "support::worker_process_helper";
pub const WORKER: &str = "worker-1";
pub const PROJECT: &str = "project-1";
pub const TASK: &str = "task-1";
pub const ATTEMPT: &str = "attempt-1";

/// Runs only when re-executed by a test below; a plain `--ignored` run returns.
#[test]
#[ignore = "worker process re-executed by harness tests"]
pub fn worker_process_helper() {
    let Some(path) = std::env::var_os(CONFIG_ENV) else {
        return;
    };
    let config: WorkerConfig = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(dacs_worker::runtime::run(config))
        .unwrap();
}

pub fn alive(pid: i32) -> bool {
    match kill(Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        Err(error) => panic!("cannot inspect {pid}: {error}"),
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

#[derive(Clone, Debug)]
pub struct Logged {
    pub phase: u32,
    pub id: String,
    pub command: Value,
    pub status: u16,
    /// Liveness of the watched pids when a stop/reconciliation arrived.
    pub watched_alive: Vec<bool>,
}

impl Logged {
    pub fn kind(&self) -> &str {
        self.command["type"].as_str().unwrap_or("")
    }
    pub fn event(&self) -> &str {
        self.command["event"]["type"].as_str().unwrap_or("")
    }
    pub fn releases(&self, attempt: &str) -> bool {
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
pub struct Fake {
    pub phase: u32,
    pub snapshot: Snapshot,
    pub messages: Vec<ControlMessage>,
    pub log: Vec<Logged>,
    pub refuse_usage: bool,
    pub watch: Vec<i32>,
    pub bundle: Vec<u8>,
}

pub type Shared = Arc<Mutex<Fake>>;

impl Fake {
    pub fn attempt(&mut self, id: &str) -> Option<&mut AttemptRecord> {
        self.snapshot.attempts.iter_mut().find(|a| a.id == id)
    }

    pub fn apply(&mut self, command: &Command) -> (StatusCode, Value) {
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

pub async fn command(
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

pub async fn commands(
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

pub async fn state(State(shared): State<Shared>) -> Json<Snapshot> {
    Json(shared.lock().unwrap().snapshot.clone())
}

pub async fn source_bundle(State(shared): State<Shared>) -> Vec<u8> {
    shared.lock().unwrap().bundle.clone()
}

pub async fn accept(_body: Bytes) -> Json<Value> {
    Json(json!({"ok":true}))
}

pub fn git(cwd: &Path, args: &[&str]) -> String {
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
pub fn fake_omp(directory: &Path, log: &Path) -> PathBuf {
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
# mawk (Debian/Ubuntu's default awk) block-buffers piped input unless run
# interactive, so it would never answer the worker's first RPC frame.
case "$(awk -W version 2>&1 </dev/null)" in *mawk*) line_input='-W interactive' ;; *) line_input= ;; esac
awk $line_input -v logf="$LOG/frames-$$" -v mode="$mode" '
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

pub struct Harness {
    pub _temporary: tempfile::TempDir,
    pub root: PathBuf,
    pub log: PathBuf,
    pub config: PathBuf,
    pub data_dir: PathBuf,
    pub shared: Shared,
    pub _runtime: tokio::runtime::Runtime,
    pub workers: Vec<Child>,
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
    pub fn new(effect_class: EffectClass) -> Self {
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

    pub fn start_worker(&mut self) {
        let index = self.workers.len();
        let stderr =
            std::fs::File::create(self.root.join(format!("worker-{index}.stderr"))).unwrap();
        let child = Process::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                HELPER_TEST,
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

    pub fn worker_log(&self) -> String {
        (0..self.workers.len())
            .map(|i| {
                std::fs::read_to_string(self.root.join(format!("worker-{i}.stderr")))
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join("\n---\n")
    }

    pub fn crash_worker(&mut self) {
        let worker = self.workers.last_mut().unwrap();
        worker.kill().unwrap(); // SIGKILL: no destructor or cleanup path runs.
        worker.wait().unwrap();
    }

    pub fn drain_and_wait(&mut self) {
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

    pub fn lines(&self, name: &str) -> Vec<String> {
        std::fs::read_to_string(self.log.join(name))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// (leader, descendant) of every attempt-mode OMP launch, in order.
    pub fn trees(&self) -> Vec<(i32, i32)> {
        self.lines("trees")
            .iter()
            .map(|line| {
                let mut parts = line.split_whitespace().map(|p| p.parse().unwrap());
                (parts.next().unwrap(), parts.next().unwrap())
            })
            .collect()
    }

    pub fn frames(&self, leader: i32) -> String {
        std::fs::read_to_string(self.log.join(format!("frames-{leader}"))).unwrap_or_default()
    }

    pub fn log(&self) -> Vec<Logged> {
        self.shared.lock().unwrap().log.clone()
    }

    pub fn accepted(&self, phase: u32, event: &str) -> bool {
        self.log().iter().any(|l| {
            l.phase == phase && l.kind() == "report_event" && l.event() == event && l.status == 200
        })
    }

    pub fn wait_for(&self, what: &str, ready: impl FnMut() -> bool) {
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

    pub fn receipts(&self) -> usize {
        std::fs::read_dir(self.data_dir.join("processes"))
            .map(|entries| entries.count())
            .unwrap_or(0)
    }

    pub fn update(&self, change: impl FnOnce(&mut Fake)) {
        change(&mut self.shared.lock().unwrap());
    }

    /// Whether any event of `attempt` reached the coordinator.
    pub fn reported(&self, attempt: &str) -> bool {
        self.log()
            .iter()
            .any(|l| l.kind() == "report_event" && l.command["attempt_id"] == attempt)
    }

    pub fn accepted_for(&self, attempt: &str, event: &str) -> bool {
        self.log().iter().any(|l| {
            l.kind() == "report_event"
                && l.command["attempt_id"] == attempt
                && l.event() == event
                && l.status == 200
        })
    }

    /// Admit another task on this worker, as the coordinator would.
    pub fn add_assignment(&self, task_id: &str, attempt_id: &str, resources: Resources) {
        self.update(|fake| {
            let mut task = fake.snapshot.tasks[0].clone();
            task.id = task_id.into();
            task.attempt_id = Some(attempt_id.into());
            task.spec.resources = resources.clone();
            let mut attempt = fake.snapshot.attempts[0].clone();
            attempt.id = attempt_id.into();
            attempt.task_id = task_id.into();
            attempt.state = "admitted".into();
            attempt.last_sequence = 0;
            attempt.resources = resources;
            let sequence = fake.messages.iter().map(|m| m.sequence).max().unwrap_or(0) + 1;
            fake.messages.push(ControlMessage {
                sequence,
                id: format!("message-assignment-{attempt_id}"),
                worker_id: WORKER.into(),
                attempt_id: Some(attempt_id.into()),
                generation: Some(attempt.generation),
                kind: "assignment".into(),
                payload: json!({"task":task,"attempt":attempt}),
            });
            fake.snapshot.tasks.push(task);
            fake.snapshot.attempts.push(attempt);
        });
    }

    /// Fence an attempt and queue its stop, as lease expiry or cancellation does.
    pub fn fence(&self, attempt_id: &str) {
        self.update(|fake| {
            let attempt = fake.attempt(attempt_id).unwrap();
            attempt.state = "fenced".into();
            let generation = attempt.generation;
            let task_id = attempt.task_id.clone();
            let task = fake
                .snapshot
                .tasks
                .iter_mut()
                .find(|t| t.id == task_id)
                .unwrap();
            task.cancel_requested = true;
            let sequence = fake.messages.iter().map(|m| m.sequence).max().unwrap_or(0) + 1;
            fake.messages.push(ControlMessage {
                sequence,
                id: format!("message-stop-{attempt_id}"),
                worker_id: WORKER.into(),
                attempt_id: Some(attempt_id.into()),
                generation: Some(generation),
                kind: "stop".into(),
                payload: json!({"reason":"administrator cancelled task"}),
            });
        });
    }
}
