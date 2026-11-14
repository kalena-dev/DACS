//! Generations, leases and coordinator loss through the real worker loop.
//!
//! `dacs_worker::runtime::run` runs in a re-executed helper process (isolated
//! HOME, killable to model a crash) and talks to an in-process mock
//! coordinator over TLS through its real HTTPS client. OMP is a scripted
//! stand-in (this test binary re-executed behind a wrapper script) that speaks
//! the JSONL RPC protocol and never contacts a model provider. Process
//! termination is checked against real PIDs at the moment the coordinator
//! receives a reservation release.
#![cfg(unix)]

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use dacs_protocol::*;
use dacs_worker::{Journal, runtime::WorkerConfig};
use nix::{
    errno::Errno,
    sys::signal::{Signal, kill, killpg},
    unistd::Pid,
};
use serde_json::{Value, json};
use std::{
    cell::Cell,
    io::{BufRead, Write},
    net::SocketAddr,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command as Process, Stdio},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

const WORKER_CONFIG: &str = "DACS_LEASES_WORKER_CONFIG";
const FAKE_OMP_DIR: &str = "DACS_LEASES_FAKE_OMP_DIR";
const FAKE_OMP_ARGS: &str = "DACS_LEASES_FAKE_OMP_ARGS";
const TOKEN: &str = "leases-device-token";
const WORKER_ID: &str = "worker-leases";
const PROJECT: &str = "project-leases";
const OMP_VERSION: &str = "9.9.9-leases";
const CONTRACT: u64 = 7;
const POLICY: u64 = 3;

// ---------------------------------------------------------------------------
// Helper processes (re-executed test binary). Plain `--ignored` runs return.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "helper process: the real worker loop for tests/leases.rs"]
fn leases_worker_process() {
    let Some(path) = std::env::var_os(WORKER_CONFIG) else {
        return;
    };
    let config: WorkerConfig = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let outcome = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(dacs_worker::runtime::run(config));
    eprintln!("worker loop ended: {outcome:?}");
}

#[test]
#[ignore = "helper process: scripted OMP RPC stand-in for tests/leases.rs"]
fn leases_fake_omp_process() {
    let Some(control) = std::env::var_os(FAKE_OMP_DIR).map(PathBuf::from) else {
        return;
    };
    fake_omp::run(&control, &std::env::var(FAKE_OMP_ARGS).unwrap_or_default());
}

/// JSONL RPC stand-in. RPC frames go to fd 3 (the worker's stdout pipe; the
/// wrapper moves libtest's own output to stderr). Behaviour after the managed
/// assignment prompt is driven by trigger files written by the test.
mod fake_omp {
    use super::*;
    use std::sync::mpsc;

    struct Session {
        out: std::fs::File,
        control: PathBuf,
        frames: mpsc::Receiver<Value>,
        usage: Value,
        tools: Vec<Child>,
        calls: u64,
    }

    pub fn run(control: &Path, args: &str) {
        let probe = args.contains("--no-tools");
        let out = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/fd/3")
            .expect("RPC output descriptor");
        let (sender, frames) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if let Ok(frame) = serde_json::from_str::<Value>(&line) {
                    if sender.send(frame).is_err() {
                        break;
                    }
                }
            }
        });
        let mut session = Session {
            out,
            control: control.to_owned(),
            frames,
            usage: json!({}),
            tools: Vec::new(),
            calls: 0,
        };
        session.log(json!({"kind":"start","probe":probe,"args":args,
            "cwd":std::env::current_dir().unwrap()}));
        session.send(json!({"type":"ready","protocolVersion":1}));
        let mut closed = false;
        loop {
            match session.frames.recv_timeout(Duration::from_millis(50)) {
                Ok(frame) => session.handle(frame, probe),
                Err(mpsc::RecvTimeoutError::Timeout) if !probe => session.triggers(),
                Err(mpsc::RecvTimeoutError::Timeout) => (),
                Err(mpsc::RecvTimeoutError::Disconnected) if probe => std::process::exit(0),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // A managed session never exits by itself: only the
                    // worker's process-tree termination stops it.
                    if !closed {
                        closed = true;
                        session.log(json!({"kind":"stdin_closed"}));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }

    impl Session {
        fn send(&mut self, frame: Value) {
            let mut bytes = serde_json::to_vec(&frame).unwrap();
            bytes.push(b'\n');
            let _ = self.out.write_all(&bytes);
            let _ = self.out.flush();
        }

        fn log(&self, mut entry: Value) {
            entry["pid"] = json!(std::process::id());
            entry["at_ms"] = json!(now_ms());
            let mut line = serde_json::to_vec(&entry).unwrap();
            line.push(b'\n');
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.control.join("omp.log"))
                .unwrap()
                .write_all(&line)
                .unwrap();
        }

        fn handle(&mut self, frame: Value, probe: bool) {
            let kind = frame["type"].as_str().unwrap_or("").to_owned();
            let Some(id) = frame["id"].as_str().map(str::to_owned) else {
                return;
            };
            if kind == "host_tool_result" {
                self.log(json!({"kind":"unexpected_tool_result","frame":frame}));
                return;
            }
            self.log(json!({"kind":"request","type":kind,"id":id,"message":frame.get("message")}));
            let data = if kind == "get_session_stats" {
                self.usage.clone()
            } else {
                json!({})
            };
            self.send(json!({"type":"response","id":id,"command":kind,"success":true,"data":data}));
            if kind != "prompt" {
                return;
            }
            let message = frame["message"].as_str().unwrap_or("").to_owned();
            if probe {
                self.send(json!({"type":"prompt_result","id":id,"status":"completed","sessionSettled":true}));
            } else if message.starts_with("DACS managed assignment") {
                // A long-running tool process inside OMP's process group.
                let tool = Process::new("sleep")
                    .arg("600")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap();
                self.log(
                    json!({"kind":"assignment","message":message,"tool_pid":tool.id(),
                    "cwd":std::env::current_dir().unwrap()}),
                );
                self.tools.push(tool);
            }
        }

        fn triggers(&mut self) {
            let directory = self.control.join("triggers");
            let Ok(entries) = std::fs::read_dir(&directory) else {
                return;
            };
            let mut names: Vec<_> = entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name())
                .filter(|name| name.to_string_lossy().ends_with(".json"))
                .collect();
            names.sort();
            for name in names {
                let claimed = self.control.join("claimed").join(&name);
                // Rename is the claim: exactly one OMP process executes it.
                if std::fs::rename(directory.join(&name), &claimed).is_err() {
                    continue;
                }
                let action: Value =
                    serde_json::from_slice(&std::fs::read(&claimed).unwrap()).unwrap();
                self.act(action);
            }
        }

        fn act(&mut self, action: Value) {
            self.log(json!({"kind":"trigger","action":action}));
            match action["action"].as_str().unwrap_or("") {
                "settle" => self.send(
                    json!({"type":"prompt_result","id":"fake-turn","status":"completed","sessionSettled":true}),
                ),
                "usage" => {
                    self.usage = action["stats"].clone();
                    self.send(json!({"type":"agent_end"}));
                }
                "commit" => {
                    let cwd = std::env::current_dir().unwrap();
                    std::fs::write(cwd.join("leases-change.txt"), "admitted work\n").unwrap();
                    let mut ok = true;
                    for args in [
                        vec!["add", "-A"],
                        vec![
                            "-c",
                            "user.name=DACS fake OMP",
                            "-c",
                            "user.email=fake-omp@example.invalid",
                            "-c",
                            "commit.gpgsign=false",
                            "commit",
                            "-q",
                            "-m",
                            "Admitted work finished during coordinator loss",
                        ],
                    ] {
                        ok &= Process::new("git")
                            .args(&args)
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .status()
                            .is_ok_and(|status| status.success());
                    }
                    let head = Process::new("git").args(["rev-parse", "HEAD"]).output().unwrap();
                    self.log(json!({"kind":"committed","ok":ok,
                        "sha":String::from_utf8_lossy(&head.stdout).trim()}));
                }
                "tool" => {
                    let name = action["name"].as_str().unwrap().to_owned();
                    let result = self.call_tool(&name, action["arguments"].clone());
                    self.log(json!({"kind":"tool_result","name":name,"result":result}));
                }
                other => self.log(json!({"kind":"unknown_action","action":other})),
            }
        }

        fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
            self.calls += 1;
            let id = format!("host-{}-{}", std::process::id(), self.calls);
            let tool_call_id = format!("toolcall-{}-{name}-{}", std::process::id(), self.calls);
            self.send(
                json!({"type":"host_tool_call","id":id,"toolCallId":tool_call_id,
                "toolName":name,"arguments":arguments}),
            );
            let deadline = Instant::now() + Duration::from_secs(60);
            while Instant::now() < deadline {
                match self.frames.recv_timeout(Duration::from_millis(50)) {
                    Ok(frame)
                        if frame["type"] == "host_tool_result" && frame["id"] == id.as_str() =>
                    {
                        return frame;
                    }
                    Ok(frame) => self.handle(frame, false),
                    Err(mpsc::RecvTimeoutError::Timeout) => (),
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return json!({"disconnected":true});
                    }
                }
            }
            json!({"timeout":true})
        }
    }
}

// ---------------------------------------------------------------------------
// Mock coordinator: TLS, bearer token, durable message queue, snapshot and the
// coordinator's event/generation/sequence checks that matter to the worker.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Received {
    at: Instant,
    command_id: String,
    command: Value,
    status: u16,
}

#[derive(Clone, Debug)]
struct Release {
    attempt_id: String,
    at: Instant,
    alive: Vec<i32>,
}

struct Coord {
    worker: WorkerRecord,
    tasks: Vec<TaskRecord>,
    attempts: Vec<AttemptRecord>,
    messages: Vec<ControlMessage>,
    /// At-least-once delivery: return every message on every poll.
    redeliver_all: bool,
    /// Answer ReportEvent with 503 (temporarily refused, retried).
    refuse_events: bool,
    commands: Vec<Received>,
    polls: Vec<(Instant, &'static str)>,
    uploads: Vec<(Instant, String)>,
    releases: Vec<Release>,
    /// PIDs that must be dead whenever a reservation release arrives.
    watch_pids: Vec<i32>,
    bundle: Vec<u8>,
}

type Shared = Arc<Mutex<Coord>>;

impl Coord {
    fn queue(&mut self, attempt: Option<&AttemptRecord>, kind: &str, payload: Value) -> String {
        let sequence = self.messages.len() as i64 + 1;
        let id = format!("message-{sequence}");
        self.messages.push(ControlMessage {
            sequence,
            id: id.clone(),
            worker_id: WORKER_ID.into(),
            attempt_id: attempt.map(|a| a.id.clone()),
            generation: attempt.map(|a| a.generation),
            kind: kind.into(),
            payload,
        });
        id
    }

    fn attempt(&mut self, id: &str) -> &mut AttemptRecord {
        self.attempts
            .iter_mut()
            .find(|a| a.id == id)
            .expect("known attempt")
    }

    fn task(&mut self, id: &str) -> &mut TaskRecord {
        self.tasks
            .iter_mut()
            .find(|t| t.id == id)
            .expect("known task")
    }

    fn expire_lease(&mut self, attempt: &str) {
        self.attempt(attempt).lease_expires_at = now_s() - 1;
    }

    /// Coordinator-side fencing (lease replacement) or explicit abandonment:
    /// the attempt loses its task and the worker gets a durable stop.
    fn fence(&mut self, attempt_id: &str, state: &str, payload: Value) {
        let attempt = self.attempt(attempt_id);
        attempt.state = state.into();
        attempt.stop_reason = payload["reason"].as_str().map(str::to_owned);
        let attempt = attempt.clone();
        let task = self.task(&attempt.task_id);
        task.attempt_id = None;
        task.state = TaskState::Ready;
        self.queue(Some(&attempt), "stop", payload);
    }

    fn release(&mut self, attempt_id: &str) {
        let alive = self
            .watch_pids
            .iter()
            .copied()
            .filter(|pid| alive(*pid))
            .collect();
        self.releases.push(Release {
            attempt_id: attempt_id.into(),
            at: Instant::now(),
            alive,
        });
    }

    fn apply(&mut self, command: &Command) -> Result<Value, (u16, String)> {
        match command {
            Command::Heartbeat { .. }
            | Command::ReportRuntimeStatus { .. }
            | Command::SetDraining { .. } => Ok(json!(self.worker)),
            Command::AcknowledgeMessages { through_sequence } => {
                Ok(json!({"acknowledged":through_sequence}))
            }
            Command::ReportEvent {
                attempt_id,
                generation,
                sequence,
                event,
            } => self.report(attempt_id, *generation, *sequence, event),
            Command::ReconcileWorker {
                stopped_attempt_ids,
            } => {
                for id in stopped_attempt_ids {
                    self.release(id);
                }
                Ok(json!({"reconciled":stopped_attempt_ids}))
            }
            Command::Delegate { .. } => Err((
                409,
                "no approved ready worker has physical-host capacity and capabilities".into(),
            )),
            Command::SubmitCandidate { submission } => {
                let current = self.attempts.iter().any(|a| {
                    a.id == submission.attempt_id
                        && a.generation == submission.generation
                        && !matches!(a.state.as_str(), "fenced" | "abandoned")
                });
                if !current {
                    return Err((409, "stale or terminal attempt generation".into()));
                }
                Ok(json!({"candidate_id":format!("candidate-{}", submission.attempt_id)}))
            }
            _ => Err((400, "unsupported by the leases mock coordinator".into())),
        }
    }

    fn report(
        &mut self,
        attempt_id: &str,
        generation: u64,
        sequence: u64,
        event: &WorkerEvent,
    ) -> Result<Value, (u16, String)> {
        if self.refuse_events {
            return Err((503, "events temporarily refused".into()));
        }
        let stopped = matches!(event, WorkerEvent::Stopped);
        let Some(attempt) = self.attempts.iter().find(|a| a.id == attempt_id).cloned() else {
            return Err((404, "unknown attempt".into()));
        };
        let task = self
            .tasks
            .iter()
            .find(|t| t.id == attempt.task_id)
            .cloned()
            .unwrap();
        if attempt.worker_id != WORKER_ID || attempt.generation != generation {
            return Err((409, "attempt generation does not match".into()));
        }
        if !stopped
            && (task.generation != generation
                || task.attempt_id.as_deref() != Some(attempt_id)
                || matches!(attempt.state.as_str(), "fenced" | "abandoned" | "stopped"))
        {
            return Err((409, "stale or terminal attempt generation".into()));
        }
        if sequence != attempt.last_sequence + 1 {
            return Err((
                409,
                "worker event sequence must be strictly contiguous".into(),
            ));
        }
        if !stopped && attempt.lease_expires_at <= now_s() {
            return Err((409, "attempt lease expired".into()));
        }
        match event {
            WorkerEvent::Started if attempt.state != "admitted" => {
                return Err((409, "attempt already started".into()));
            }
            WorkerEvent::Started => self.attempt(attempt_id).state = "executing".into(),
            WorkerEvent::Settled => self.attempt(attempt_id).settled = true,
            WorkerEvent::Failed { message } => {
                self.attempt(attempt_id).state = "failed".into();
                self.attempt(attempt_id).stop_reason = Some(message.clone());
                self.task(&task.id).state = TaskState::Failed;
            }
            WorkerEvent::Stopped => {
                self.release(attempt_id);
                let attempt = self.attempt(attempt_id);
                if !matches!(attempt.state.as_str(), "fenced" | "abandoned" | "failed") {
                    attempt.state = "stopped".into();
                }
            }
            _ => (),
        }
        let attempt = self.attempt(attempt_id);
        attempt.last_sequence = sequence;
        Ok(json!(attempt))
    }
}

fn reply(status: u16, body: Value) -> Response {
    (StatusCode::from_u16(status).unwrap(), Json(body)).into_response()
}

fn authorized(headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some(format!("Bearer {TOKEN}").as_str())
}

async fn command(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !authorized(&headers) {
        return reply(
            401,
            json!({"code":"unauthorized","message":"invalid device token"}),
        );
    }
    let envelope: CommandEnvelope = match serde_json::from_value(body.clone()) {
        Ok(envelope) => envelope,
        Err(error) => return reply(400, json!({"code":"invalid","message":error.to_string()})),
    };
    let mut coord = state.lock().unwrap();
    let (status, response) = match coord.apply(&envelope.command) {
        Ok(result) => (
            200,
            json!({"command_id":envelope.command_id,"result":result}),
        ),
        Err((status, message)) => (status, json!({"code":"conflict","message":message})),
    };
    coord.commands.push(Received {
        at: Instant::now(),
        command_id: envelope.command_id,
        command: body["command"].clone(),
        status,
    });
    reply(status, response)
}

#[derive(serde::Deserialize)]
struct After {
    #[serde(default)]
    after: i64,
}

async fn messages(
    State(state): State<Shared>,
    headers: HeaderMap,
    Query(after): Query<After>,
) -> Response {
    if !authorized(&headers) {
        return reply(
            401,
            json!({"code":"unauthorized","message":"invalid device token"}),
        );
    }
    let mut coord = state.lock().unwrap();
    coord.polls.push((Instant::now(), "commands"));
    let list: Vec<_> = coord
        .messages
        .iter()
        .filter(|m| coord.redeliver_all || m.sequence > after.after)
        .cloned()
        .collect();
    reply(200, json!(list))
}

async fn snapshot(State(state): State<Shared>, headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return reply(
            401,
            json!({"code":"unauthorized","message":"invalid device token"}),
        );
    }
    let mut coord = state.lock().unwrap();
    coord.polls.push((Instant::now(), "state"));
    reply(
        200,
        json!(Snapshot {
            workers: vec![coord.worker.clone()],
            tasks: coord.tasks.clone(),
            attempts: coord.attempts.clone(),
            ..Snapshot::default()
        }),
    )
}

async fn source(State(state): State<Shared>, headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return reply(
            401,
            json!({"code":"unauthorized","message":"invalid device token"}),
        );
    }
    let mut coord = state.lock().unwrap();
    coord.polls.push((Instant::now(), "source"));
    coord.bundle.clone().into_response()
}

async fn upload(
    State(state): State<Shared>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if !authorized(&headers) {
        return reply(
            401,
            json!({"code":"unauthorized","message":"invalid device token"}),
        );
    }
    let mut coord = state.lock().unwrap();
    coord.uploads.push((Instant::now(), uri.path().to_owned()));
    reply(200, json!({"stored":uri.path(),"bytes":body.len()}))
}

struct MockServer {
    runtime: tokio::runtime::Runtime,
    state: Shared,
    address: SocketAddr,
    tls: axum_server::tls_rustls::RustlsConfig,
    running: Option<(
        axum_server::Handle,
        tokio::task::JoinHandle<std::io::Result<()>>,
    )>,
}

impl MockServer {
    fn new(state: Shared, certificate: &str, key: &str) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let tls = runtime
            .block_on(axum_server::tls_rustls::RustlsConfig::from_pem(
                certificate.as_bytes().to_vec(),
                key.as_bytes().to_vec(),
            ))
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let mut server = Self {
            runtime,
            state,
            address,
            tls,
            running: None,
        };
        server.serve(listener);
        server
    }

    fn serve(&mut self, listener: std::net::TcpListener) {
        listener.set_nonblocking(true).unwrap();
        let handle = axum_server::Handle::new();
        let app = Router::new()
            .route("/api/command", post(command))
            .route("/api/commands", get(messages))
            .route("/api/state", get(snapshot))
            .route("/api/projects/{project}/source", get(source).post(upload))
            .route("/api/projects/{project}/artifacts/{digest}", post(upload))
            .with_state(self.state.clone());
        let server =
            axum_server::from_tcp_rustls(listener, self.tls.clone()).handle(handle.clone());
        let join = self
            .runtime
            .spawn(async move { server.serve(app.into_make_service()).await });
        self.running = Some((handle, join));
    }

    /// Coordinator loss: the listener closes and live connections drop, so
    /// every request (heartbeats included) fails at the transport.
    fn stop(&mut self) {
        if let Some((handle, join)) = self.running.take() {
            handle.shutdown();
            let _ = self.runtime.block_on(join);
        }
    }

    fn start(&mut self) {
        let listener = std::net::TcpListener::bind(self.address).unwrap();
        self.serve(listener);
    }
}

// ---------------------------------------------------------------------------
// Fixture: source repository, OMP stand-in, TLS material, worker process.
// ---------------------------------------------------------------------------

struct Launch {
    pid: i32,
    tool_pid: i32,
    cwd: PathBuf,
    message: String,
}

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    server: MockServer,
    state: Shared,
    config: WorkerConfig,
    config_path: PathBuf,
    source_commit: String,
    worker: Option<Child>,
    triggers: Cell<u32>,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        for sub in [
            "home",
            "tmp",
            "bin",
            "source",
            "control/triggers",
            "control/claimed",
        ] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let source = root.join("source");
        git(&source, &["init", "-q", "--initial-branch=main"]);
        std::fs::write(source.join("README.md"), "leases fixture\n").unwrap();
        git(&source, &["add", "README.md"]);
        git(
            &source,
            &[
                "-c",
                "user.name=DACS fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "Pinned fixture source",
            ],
        );
        let source_commit = git(&source, &["rev-parse", "HEAD"]);
        let bundle_path = root.join("source.bundle");
        git(
            &source,
            &[
                "bundle",
                "create",
                "-q",
                bundle_path.to_str().unwrap(),
                "--all",
            ],
        );
        let bundle = std::fs::read(&bundle_path).unwrap();

        let omp = root.join("bin/omp");
        std::fs::write(
            &omp,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"omp/{OMP_VERSION}\"; exit 0; fi\n\
                 {FAKE_OMP_DIR}='{}'; export {FAKE_OMP_DIR}\n\
                 {FAKE_OMP_ARGS}=\"$*\"; export {FAKE_OMP_ARGS}\n\
                 exec '{}' --exact leases_fake_omp_process --ignored --nocapture --test-threads=1 3>&1 1>&2\n",
                root.join("control").display(),
                std::env::current_exe().unwrap().display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o755)).unwrap();

        let certified = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
        let certificate = certified.cert.pem();
        std::fs::write(root.join("ca.pem"), &certificate).unwrap();
        let credential = root.join("device.token");
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&credential)
            .unwrap()
            .write_all(TOKEN.as_bytes())
            .unwrap();

        let registration = WorkerRegistration {
            name: "leases worker".into(),
            physical_host_id: format!("leases-host-{}", uuid::Uuid::new_v4()),
            os: std::env::consts::OS.into(),
            architecture: std::env::consts::ARCH.into(),
            capabilities: Vec::new(),
            capacity: Resources {
                agent_slots: 2,
                cpu_millis: 2000,
                memory_mb: 2048,
                disk_mb: 2048,
            },
            omp_version: OMP_VERSION.into(),
            policy_revision: POLICY,
            owner_consented: true,
            consent_host: "leases-test-host".into(),
        };
        let state = Arc::new(Mutex::new(Coord {
            worker: WorkerRecord {
                id: WORKER_ID.into(),
                project_id: PROJECT.into(),
                registration: registration.clone(),
                approved: true,
                revoked: false,
                draining: false,
                last_seen: now_s(),
                reserved: Resources::default(),
            },
            tasks: Vec::new(),
            attempts: Vec::new(),
            messages: Vec::new(),
            redeliver_all: false,
            refuse_events: false,
            commands: Vec::new(),
            polls: Vec::new(),
            uploads: Vec::new(),
            releases: Vec::new(),
            watch_pids: Vec::new(),
            bundle,
        }));
        let server = MockServer::new(
            state.clone(),
            &certificate,
            &certified.key_pair.serialize_pem(),
        );
        let config = WorkerConfig {
            coordinator_url: format!("https://{}", server.address),
            credential_file: credential,
            worker_id: WORKER_ID.into(),
            registration,
            omp_executable: omp,
            omp_version: OMP_VERSION.into(),
            omp_profile: None,
            data_dir: root.join("data"),
            workspaces_dir: root.join("workspaces"),
            ca_certificate: Some(root.join("ca.pem")),
            poll_interval_ms: 150,
        };
        let config_path = root.join("worker.json");
        std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        Self {
            _directory: directory,
            root,
            server,
            state,
            config,
            config_path,
            source_commit,
            worker: None,
            triggers: Cell::new(0),
        }
    }

    fn coord(&self) -> MutexGuard<'_, Coord> {
        self.state.lock().unwrap()
    }

    /// Admit an agent attempt for this worker and queue its assignment.
    fn assign(
        &self,
        task_id: &str,
        attempt_id: &str,
        generation: u64,
        effect: EffectClass,
    ) -> (TaskRecord, AttemptRecord) {
        let resources = Resources {
            agent_slots: 1,
            cpu_millis: 500,
            memory_mb: 256,
            disk_mb: 256,
        };
        let task = TaskRecord {
            id: task_id.into(),
            spec: TaskSpec {
                project_id: PROJECT.into(),
                title: format!("leases {task_id}"),
                instructions: "Lease fixture. The scripted OMP stand-in never contacts a provider."
                    .into(),
                source_commit: self.source_commit.clone(),
                execution: TaskExecution::Agent,
                dependencies: Vec::new(),
                required_capabilities: Vec::new(),
                resources: resources.clone(),
                effect_class: effect,
                lease_seconds: 1,
                max_runtime_seconds: 3600,
                contract_revision: CONTRACT,
                policy_revision: POLICY,
                max_retries: 3,
            },
            state: TaskState::Executing,
            generation,
            parent_task_id: None,
            attempt_id: Some(attempt_id.into()),
            integrated_sha: None,
            workflow_result: None,
            blocked_reason: None,
            cancel_requested: false,
        };
        // The lease is coordinator state; tests expire it explicitly.
        let attempt = AttemptRecord {
            id: attempt_id.into(),
            task_id: task_id.into(),
            worker_id: WORKER_ID.into(),
            generation,
            state: "admitted".into(),
            lease_expires_at: now_s() + 3600,
            started_at: now_s(),
            settled: false,
            pending_children: Vec::new(),
            last_sequence: 0,
            stop_reason: None,
            resources,
        };
        let mut coord = self.coord();
        coord.tasks.retain(|t| t.id != task.id);
        coord.tasks.push(task.clone());
        coord.attempts.push(attempt.clone());
        coord.queue(
            Some(&attempt),
            "assignment",
            json!({"task":task,"attempt":attempt}),
        );
        (task, attempt)
    }

    fn start_worker(&mut self) {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("worker.log"))
            .unwrap();
        let child = Process::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "leases_worker_process",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.root.join("home"))
            .env("TMPDIR", self.root.join("tmp"))
            .env("RUST_BACKTRACE", "1")
            .env(WORKER_CONFIG, &self.config_path)
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        self.worker = Some(child);
    }

    /// SIGKILL: no destructor, drain or final flush runs.
    fn kill_worker(&mut self) {
        if let Some(mut child) = self.worker.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn worker_log(&self) -> String {
        std::fs::read_to_string(self.root.join("worker.log")).unwrap_or_default()
    }

    fn omp_log(&self) -> Vec<Value> {
        std::fs::read_to_string(self.root.join("control/omp.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn assignment_prompts(&self, attempt: &AttemptRecord) -> Vec<Launch> {
        let needle = format!(
            "DACS managed assignment {} generation {}.",
            attempt.task_id, attempt.generation
        );
        self.omp_log()
            .into_iter()
            .filter(|e| {
                e["kind"] == "assignment"
                    && e["message"]
                        .as_str()
                        .is_some_and(|m| m.starts_with(&needle))
            })
            .map(|e| Launch {
                pid: e["pid"].as_i64().unwrap() as i32,
                tool_pid: e["tool_pid"].as_i64().unwrap() as i32,
                cwd: PathBuf::from(e["cwd"].as_str().unwrap()),
                message: e["message"].as_str().unwrap().to_owned(),
            })
            .collect()
    }

    fn wait_launch(&self, attempt: &AttemptRecord) -> Launch {
        self.wait_for(
            "managed OMP assignment prompt",
            Duration::from_secs(60),
            || !self.assignment_prompts(attempt).is_empty(),
        );
        self.assignment_prompts(attempt).remove(0)
    }

    fn trigger(&self, action: Value) {
        let index = self.triggers.get() + 1;
        self.triggers.set(index);
        let directory = self.root.join("control/triggers");
        let staging = directory.join(format!(".{index:04}.tmp"));
        std::fs::write(&staging, serde_json::to_vec(&action).unwrap()).unwrap();
        std::fs::rename(staging, directory.join(format!("{index:04}.json"))).unwrap();
    }

    fn tool_result(&self, name: &str) -> Option<Value> {
        self.omp_log()
            .into_iter()
            .find(|e| e["kind"] == "tool_result" && e["name"] == name)
            .map(|e| e["result"].clone())
    }

    fn wait_tool_result(&self, name: &str) -> Value {
        self.wait_for(
            &format!("{name} host tool result"),
            Duration::from_secs(30),
            || self.tool_result(name).is_some(),
        );
        self.tool_result(name).unwrap()
    }

    /// ReportEvent commands for one attempt, in arrival order.
    fn events(&self, attempt: &str) -> Vec<Received> {
        self.coord()
            .commands
            .iter()
            .filter(|r| r.command["type"] == "report_event" && r.command["attempt_id"] == attempt)
            .cloned()
            .collect()
    }

    fn accepted_event(&self, attempt: &str, kind: &str) -> bool {
        self.events(attempt)
            .iter()
            .any(|r| r.status == 200 && r.command["event"]["type"] == kind)
    }

    fn count(&self, kind: &str) -> usize {
        self.coord()
            .commands
            .iter()
            .filter(|r| r.command["type"] == kind && r.status == 200)
            .count()
    }

    fn state_polls(&self) -> usize {
        self.coord()
            .polls
            .iter()
            .filter(|(_, kind)| *kind == "state")
            .count()
    }

    fn release(&self, attempt: &str) -> Option<Release> {
        self.coord()
            .releases
            .iter()
            .find(|r| r.attempt_id == attempt)
            .cloned()
    }

    fn wait_release(&self, attempt: &str) -> Release {
        self.wait_for(
            &format!("reservation release for {attempt}"),
            Duration::from_secs(60),
            || self.release(attempt).is_some(),
        );
        self.release(attempt).unwrap()
    }

    /// Several complete worker cycles that reached the snapshot step.
    fn wait_cycles(&self, cycles: usize) {
        let start = self.state_polls();
        self.wait_for(
            "worker reconciliation cycles",
            Duration::from_secs(60),
            || self.state_polls() >= start + cycles,
        );
    }

    fn pending_results(&self) -> Vec<dacs_worker::PendingResult> {
        Journal::open(self.config.data_dir.join("worker.sqlite"))
            .unwrap()
            .pending_results(1024)
            .unwrap()
    }

    fn wait_for(&self, what: &str, timeout: Duration, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + timeout;
        while !ready() {
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {what}\n--- worker log ---\n{}\n--- OMP log ---\n{:#?}\n--- commands ---\n{:#?}",
                    self.worker_log(),
                    self.omp_log(),
                    self.coord()
                        .commands
                        .iter()
                        .map(|r| (
                            r.status,
                            r.command["type"].clone(),
                            r.command.get("event").cloned()
                        ))
                        .collect::<Vec<_>>()
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.kill_worker();
        // Never leak OMP stand-ins or their tool processes.
        for entry in self.omp_log() {
            if let Some(pid) = entry["pid"].as_i64() {
                let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
                let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
            }
            if let Some(pid) = entry["tool_pid"].as_i64() {
                let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
            }
        }
        self.server.stop();
    }
}

fn git(directory: &Path, args: &[&str]) -> String {
    let output = Process::new("git")
        .args(args)
        .current_dir(directory)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Running (not merely an unreaped zombie).
fn alive(pid: i32) -> bool {
    match kill(Pid::from_raw(pid), None) {
        Err(Errno::ESRCH) => false,
        _ => Process::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .map(|output| {
                let stat = String::from_utf8_lossy(&output.stdout);
                let stat = stat.trim();
                !stat.is_empty() && !stat.starts_with('Z')
            })
            .unwrap_or(true),
    }
}

fn now_s() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

fn hold_alive(f: &Fixture, pids: &[i32], duration: Duration) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        for pid in pids {
            assert!(
                alive(*pid),
                "admitted work {pid} was abandoned\n{}",
                f.worker_log()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------------------
// 1. Duplicate commands, conflicting redelivery, stale generations, pins.
// ---------------------------------------------------------------------------

#[test]
fn duplicate_and_stale_commands_are_refused_and_pins_stay_bound() {
    let mut f = Fixture::new();
    let (task, attempt) = f.assign("task-dup", "attempt-dup-g2", 2, EffectClass::Workspace);
    f.start_worker();
    let launch = f.wait_launch(&attempt);
    // Execution is bound to the assignment's source/contract/policy pins.
    assert!(
        launch.message.contains(&format!(
            "Source {}. Contract {CONTRACT} policy {POLICY}.",
            f.source_commit
        )),
        "{}",
        launch.message
    );
    assert_eq!(git(&launch.cwd, &["rev-parse", "HEAD"]), f.source_commit);
    f.wait_for("Started accepted", Duration::from_secs(30), || {
        f.accepted_event(&attempt.id, "started")
    });

    // A delayed delivery of the superseded generation-1 assignment, then
    // at-least-once redelivery of every message on every poll.
    {
        let mut coord = f.coord();
        let mut old_task = task.clone();
        old_task.generation = 1;
        old_task.attempt_id = Some("attempt-dup-g1".into());
        let old_attempt = AttemptRecord {
            id: "attempt-dup-g1".into(),
            generation: 1,
            ..attempt.clone()
        };
        coord.attempts.push(old_attempt.clone());
        coord.queue(
            Some(&old_attempt),
            "assignment",
            json!({"task":old_task,"attempt":old_attempt}),
        );
        coord.redeliver_all = true;
    }
    f.wait_cycles(5);
    assert_eq!(
        f.assignment_prompts(&attempt).len(),
        1,
        "duplicate delivery relaunched work"
    );
    let g1 = AttemptRecord {
        id: "attempt-dup-g1".into(),
        generation: 1,
        ..attempt.clone()
    };
    assert!(
        f.assignment_prompts(&g1).is_empty(),
        "stale generation was executed"
    );
    assert!(
        f.coord()
            .commands
            .iter()
            .all(|r| !r.command.to_string().contains("attempt-dup-g1")),
        "worker acted on the stale generation"
    );
    assert_eq!(
        f.events(&attempt.id)
            .iter()
            .filter(|r| r.command["event"]["type"] == "started")
            .count(),
        1
    );

    // Same id, different body: the redelivered assignment now carries other pins.
    {
        let mut coord = f.coord();
        let original = &mut coord.messages[0];
        original.payload["task"]["spec"]["contract_revision"] = json!(CONTRACT + 1);
        original.payload["task"]["spec"]["source_commit"] = json!("0".repeat(40));
    }
    // Refusal must not wedge the control loop: snapshots keep being reconciled.
    f.wait_cycles(5);
    let journal = Journal::open(f.config.data_dir.join("worker.sqlite")).unwrap();
    let refusal: Option<Value> = journal.get_value("refused-message:message-1").unwrap();
    drop(journal);
    assert!(
        refusal.is_some(),
        "conflicting redelivery was not recorded as refused"
    );
    assert_eq!(f.assignment_prompts(&attempt).len(), 1);
    assert!(alive(launch.pid) && alive(launch.tool_pid));

    // The coordinator's record of this generation stops carrying the
    // assignment pins: the attempt is not continued under other pins.
    {
        let mut coord = f.coord();
        coord.redeliver_all = false;
        coord.task(&task.id).spec.contract_revision = CONTRACT + 1;
        coord.watch_pids = vec![launch.pid, launch.tool_pid];
    }
    let release = f.wait_release(&attempt.id);
    assert!(
        release.alive.is_empty(),
        "released before stop: {release:?}"
    );
    assert_eq!(f.assignment_prompts(&attempt).len(), 1);
}

// ---------------------------------------------------------------------------
// 2. Unreported results survive restart and replay with original identity.
// ---------------------------------------------------------------------------

#[test]
fn unreported_results_survive_restart_and_replay_with_original_ids() {
    let mut f = Fixture::new();
    let (_task, attempt) = f.assign("task-replay", "attempt-replay", 1, EffectClass::Workspace);
    f.start_worker();
    f.wait_launch(&attempt);
    f.wait_for("Started accepted", Duration::from_secs(30), || {
        f.accepted_event(&attempt.id, "started")
    });
    f.coord().refuse_events = true;
    f.trigger(json!({"action":"settle"}));
    f.trigger(json!({"action":"usage","stats":{"tokens":{"input":11,"output":7},"cost":0.0}}));
    // The head of the ordered outbox is retried under the same id.
    f.wait_for("refused result retried", Duration::from_secs(30), || {
        f.events(&attempt.id)
            .iter()
            .filter(|r| r.status == 503)
            .count()
            >= 3
    });
    f.wait_for("usage result journaled", Duration::from_secs(30), || {
        f.pending_results()
            .iter()
            .any(|r| r.payload["command"]["event"]["type"] == "usage")
    });
    f.kill_worker();

    let pending: Vec<_> = f
        .pending_results()
        .into_iter()
        .filter(|r| r.payload["command"]["type"] == "report_event" && r.attempt_id == attempt.id)
        .collect();
    let kinds: Vec<_> = pending
        .iter()
        .map(|r| {
            r.payload["command"]["event"]["type"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(kinds, ["settled", "usage"], "{pending:#?}");
    assert_eq!(
        pending.iter().map(|r| r.sequence).collect::<Vec<_>>(),
        [2, 3]
    );
    let refused: Vec<_> = f
        .events(&attempt.id)
        .into_iter()
        .filter(|r| r.status == 503)
        .collect();
    assert!(refused.iter().all(|r| r.command_id == pending[0].result_id));

    f.coord().refuse_events = false;
    f.start_worker();
    f.wait_for(
        "replayed results acknowledged",
        Duration::from_secs(60),
        || {
            let coord = f.coord();
            pending.iter().all(|p| {
                coord
                    .commands
                    .iter()
                    .any(|r| r.command_id == p.result_id && r.status == 200)
            })
        },
    );
    f.wait_cycles(5);
    let coord = f.coord();
    for result in &pending {
        let accepted: Vec<_> = coord
            .commands
            .iter()
            .filter(|r| r.command_id == result.result_id && r.status == 200)
            .collect();
        assert_eq!(accepted.len(), 1, "acknowledged result was resent");
        assert_eq!(
            accepted[0].command, result.payload["command"],
            "replayed body changed"
        );
        assert!(
            coord
                .commands
                .iter()
                .filter(|r| r.command_id == result.result_id)
                .all(|r| r.at <= accepted[0].at),
            "result delivered again after acknowledgement"
        );
    }
    // Sequences stay unique and contiguous across the restart.
    let sequences: Vec<u64> = coord
        .commands
        .iter()
        .filter(|r| {
            r.status == 200
                && r.command["type"] == "report_event"
                && r.command["attempt_id"] == attempt.id.as_str()
        })
        .map(|r| r.command["sequence"].as_u64().unwrap())
        .collect();
    assert_eq!(sequences, (1..=sequences.len() as u64).collect::<Vec<_>>());
}

// ---------------------------------------------------------------------------
// 3. Heartbeat loss alone keeps work; lease replacement stops, reconciles,
//    then releases.
// ---------------------------------------------------------------------------

#[test]
fn heartbeat_loss_keeps_work_and_fenced_replacement_stops_before_release() {
    let mut f = Fixture::new();
    let (task, attempt) = f.assign("task-lease", "attempt-lease-g1", 1, EffectClass::Workspace);
    f.start_worker();
    let launch = f.wait_launch(&attempt);
    let pids = [launch.pid, launch.tool_pid];
    f.wait_for("Started accepted", Duration::from_secs(30), || {
        f.accepted_event(&attempt.id, "started")
    });

    // Coordinator loss for three lease periods; the lease lapses but the
    // coordinator has not replaced the attempt.
    let heartbeats = f.count("heartbeat");
    f.server.stop();
    f.coord().expire_lease(&attempt.id);
    hold_alive(&f, &pids, Duration::from_secs(3));
    f.server.start();
    f.wait_for("reconnected heartbeats", Duration::from_secs(30), || {
        f.count("heartbeat") >= heartbeats + 3
    });
    f.wait_cycles(3);
    hold_alive(&f, &pids, Duration::from_millis(500));
    assert_eq!(f.assignment_prompts(&attempt).len(), 1);
    assert!(
        f.release(&attempt.id).is_none(),
        "heartbeat loss released the reservation"
    );
    assert!(
        !f.omp_log().iter().any(|e| e["type"] == "abort"),
        "heartbeat loss aborted the session"
    );
    assert!(
        f.events(&attempt.id).iter().all(|r| !matches!(
            r.command["event"]["type"].as_str(),
            Some("failed" | "stopped")
        )),
        "heartbeat loss ended the attempt"
    );

    // Lease replacement while unreachable: the coordinator fences the
    // workspace-only attempt and admits generation 2 elsewhere. The fenced
    // attempt produced one more unreported result meanwhile.
    f.server.stop();
    f.trigger(json!({"action":"settle"}));
    f.wait_for(
        "result produced while unreachable",
        Duration::from_secs(30),
        || {
            f.pending_results().iter().any(|r| {
                r.attempt_id == attempt.id && r.payload["command"]["event"]["type"] == "settled"
            })
        },
    );
    {
        let mut coord = f.coord();
        coord.fence(
            &attempt.id,
            "fenced",
            json!({"reason":"lease expired","reservation_held_until_reconciliation":true}),
        );
        let replacement = AttemptRecord {
            id: "attempt-lease-g2".into(),
            worker_id: "worker-other".into(),
            generation: 2,
            state: "admitted".into(),
            last_sequence: 0,
            ..attempt.clone()
        };
        coord.attempts.push(replacement);
        let task = coord.task(&task.id);
        task.generation = 2;
        task.attempt_id = Some("attempt-lease-g2".into());
        task.state = TaskState::Executing;
        coord.watch_pids = pids.to_vec();
    }
    f.server.start();
    let release = f.wait_release(&attempt.id);
    assert!(
        release.alive.is_empty(),
        "released before the fenced tree stopped: {release:?}"
    );
    let events = f.events(&attempt.id);
    let settled = events
        .iter()
        .find(|r| r.command["event"]["type"] == "settled")
        .expect("unreported result reconciled after reconnect");
    assert!(settled.at <= release.at, "result reconciled after release");
    assert_eq!(settled.status, 409, "fenced attempt result must be refused");
    assert!(
        events
            .iter()
            .filter(|r| r.command["event"]["type"] != "stopped")
            .all(|r| r.at <= release.at),
        "fenced attempt reported after release"
    );
    // No local replay of the fenced generation, no claim on the replacement.
    f.wait_cycles(3);
    assert_eq!(f.assignment_prompts(&attempt).len(), 1);
    assert!(
        f.coord()
            .commands
            .iter()
            .all(|r| !r.command.to_string().contains("attempt-lease-g2"))
    );
    assert_eq!(
        f.coord()
            .releases
            .iter()
            .filter(|r| r.attempt_id == attempt.id)
            .count(),
        1,
        "released twice"
    );
}

// ---------------------------------------------------------------------------
// 4. Effectful attempts are never replaced automatically.
// ---------------------------------------------------------------------------

#[test]
fn effectful_attempt_survives_lease_expiry_until_explicit_abandonment() {
    let mut f = Fixture::new();
    let (_task, attempt) = f.assign("task-effect", "attempt-effect", 1, EffectClass::External);
    f.start_worker();
    let launch = f.wait_launch(&attempt);
    let pids = [launch.pid, launch.tool_pid];
    f.wait_for("Started accepted", Duration::from_secs(30), || {
        f.accepted_event(&attempt.id, "started")
    });
    let heartbeats = f.count("heartbeat");
    f.server.stop();
    f.coord().expire_lease(&attempt.id);
    hold_alive(&f, &pids, Duration::from_secs(3));
    f.server.start();
    f.wait_for("reconnected heartbeats", Duration::from_secs(30), || {
        f.count("heartbeat") >= heartbeats + 3
    });
    // Expired, unreachable-then-back, never fenced: the effectful attempt
    // keeps running and keeps its reservation.
    f.wait_cycles(5);
    hold_alive(&f, &pids, Duration::from_millis(500));
    assert!(f.release(&attempt.id).is_none());
    assert!(f.events(&attempt.id).iter().all(|r| !matches!(
        r.command["event"]["type"].as_str(),
        Some("failed" | "stopped")
    )));

    // Explicit abandonment by the operator.
    {
        let mut coord = f.coord();
        coord.fence(
            &attempt.id,
            "abandoned",
            json!({"reason":"operator inspected external effects","explicit_abandonment":true}),
        );
        coord.watch_pids = pids.to_vec();
    }
    let release = f.wait_release(&attempt.id);
    assert!(
        release.alive.is_empty(),
        "released before stop: {release:?}"
    );
    assert_eq!(f.assignment_prompts(&attempt).len(), 1);
}

#[test]
fn effectful_attempt_is_not_replayed_after_worker_restart() {
    let mut f = Fixture::new();
    let (_task, attempt) = f.assign(
        "task-effect-restart",
        "attempt-effect-restart",
        1,
        EffectClass::External,
    );
    f.start_worker();
    let launch = f.wait_launch(&attempt);
    f.wait_for("Started accepted", Duration::from_secs(30), || {
        f.accepted_event(&attempt.id, "started")
    });
    f.kill_worker();
    f.coord().watch_pids = vec![launch.pid, launch.tool_pid];
    f.start_worker();
    let release = f.wait_release(&attempt.id);
    assert!(
        release.alive.is_empty(),
        "released before the crashed tree stopped: {release:?}"
    );
    f.wait_cycles(3);
    assert_eq!(
        f.assignment_prompts(&attempt).len(),
        1,
        "external-effect attempt was replayed automatically"
    );
    let events = f.events(&attempt.id);
    // Uncertain external effects are never settled as a terminal failure.
    assert!(
        events
            .iter()
            .all(|r| r.command["event"]["type"] != "failed"),
        "crash report made the effectful attempt terminal"
    );
    let interrupted = events
        .iter()
        .find(|r| {
            r.command["event"]["type"] == "progress"
                && r.command["event"]["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("automatic replay is forbidden"))
        })
        .expect("explicit interruption report");
    assert!(interrupted.at <= release.at);

    // The worker's actual post-crash reports, replayed into the real
    // coordinator state machine, leave the attempt awaiting explicit
    // abandonment (not terminal), with its stopped reservation released.
    let reported: Vec<(u64, WorkerEvent)> = events
        .iter()
        .filter(|r| r.status == 200 && r.command["event"]["type"] != "started")
        .map(|r| {
            (
                r.command["sequence"].as_u64().unwrap(),
                serde_json::from_value(r.command["event"].clone()).unwrap(),
            )
        })
        .collect();
    assert!(
        reported
            .iter()
            .any(|(_, event)| matches!(event, WorkerEvent::Stopped))
    );
    real_coordinator_awaits_abandonment(&f.config.registration, &reported);
}

/// Real `dacs_core::Store`: admit an external-effect attempt, report Started
/// as the worker did before its crash, then the given post-crash events. The
/// task must stay unfinished with its reservation released, and the operator's
/// explicit AbandonAttempt must be accepted.
fn real_coordinator_awaits_abandonment(
    registration: &WorkerRegistration,
    reported: &[(u64, WorkerEvent)],
) {
    let directory = tempfile::tempdir().unwrap();
    let store = dacs_core::Store::open(
        directory.path().join("coordinator.sqlite"),
        "leases-administrator-token-with-32-bytes-or-more",
    )
    .unwrap();
    let admin = Principal {
        is_admin: true,
        device_id: None,
    };
    let run = |principal: &Principal, command: Command| {
        store
            .execute(
                principal,
                CommandEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    command_id: uuid::Uuid::new_v4().to_string(),
                    command,
                },
            )
            .map(|response| response.result)
    };
    let project: Project = serde_json::from_value(
        run(
            &admin,
            Command::CreateProject {
                name: "leases".into(),
                target_branch: "main".into(),
                max_tasks: 10,
                max_depth: 2,
            },
        )
        .unwrap(),
    )
    .unwrap();
    let invitation: Invitation = serde_json::from_value(
        run(
            &admin,
            Command::CreateInvitation {
                project_id: project.id.clone(),
                host_name: registration.consent_host.clone(),
                expires_in_seconds: 600,
            },
        )
        .unwrap(),
    )
    .unwrap();
    let mut registration = registration.clone();
    registration.policy_revision = project.policy_revision;
    let enrolled = store.enroll(&invitation.token, registration).unwrap();
    run(
        &admin,
        Command::ApproveWorker {
            worker_id: enrolled.worker.id.clone(),
        },
    )
    .unwrap();
    let device = store.authenticate(&enrolled.device_token).unwrap();
    run(
        &device,
        Command::ReportRuntimeStatus {
            status: WorkerRuntimeStatus {
                worker_id: enrolled.worker.id.clone(),
                provider_authenticated: Some(true),
                network_ready: Some(true),
                desktop_session: None,
                desktop_active: false,
                desktop_paused: false,
                observed_at: now_s(),
            },
        },
    )
    .unwrap();
    let task: TaskRecord = serde_json::from_value(
        run(
            &admin,
            Command::CreateTask {
                spec: TaskSpec {
                    project_id: project.id.clone(),
                    title: "external effect".into(),
                    instructions: "host-level change".into(),
                    source_commit: "1".repeat(40),
                    execution: TaskExecution::Agent,
                    dependencies: Vec::new(),
                    required_capabilities: Vec::new(),
                    resources: Resources {
                        agent_slots: 1,
                        cpu_millis: 500,
                        memory_mb: 256,
                        disk_mb: 256,
                    },
                    effect_class: EffectClass::External,
                    lease_seconds: 300,
                    max_runtime_seconds: 3600,
                    contract_revision: project.contract_revision,
                    policy_revision: project.policy_revision,
                    max_retries: 2,
                },
            },
        )
        .unwrap(),
    )
    .unwrap();
    let attempt: AttemptRecord = serde_json::from_value(
        run(
            &admin,
            Command::AdmitTask {
                task_id: task.id.clone(),
                worker_id: Some(enrolled.worker.id.clone()),
            },
        )
        .unwrap(),
    )
    .unwrap();
    let report = |sequence: u64, event: WorkerEvent| {
        run(
            &device,
            Command::ReportEvent {
                attempt_id: attempt.id.clone(),
                generation: attempt.generation,
                sequence,
                event,
            },
        )
    };
    report(1, WorkerEvent::Started).unwrap();
    for (sequence, event) in reported {
        report(*sequence, event.clone()).unwrap_or_else(|error| {
            panic!("real coordinator refused {event:?} at {sequence}: {error}")
        });
    }
    let snapshot = store.snapshot(&admin).unwrap();
    let current = snapshot.tasks.iter().find(|t| t.id == task.id).unwrap();
    assert_eq!(
        current.state,
        TaskState::Executing,
        "crash report settled the effectful task: {current:?}"
    );
    assert_eq!(current.attempt_id.as_deref(), Some(attempt.id.as_str()));
    let worker = snapshot
        .workers
        .iter()
        .find(|w| w.id == enrolled.worker.id)
        .unwrap();
    assert_eq!(
        worker.reserved,
        Resources::default(),
        "stopped attempt kept its reservation"
    );
    let abandoned: TaskRecord = serde_json::from_value(
        run(
            &admin,
            Command::AbandonAttempt {
                attempt_id: attempt.id.clone(),
                reason: "operator inspected the interrupted external effects".into(),
            },
        )
        .expect("explicit abandonment of the interrupted effectful attempt"),
    )
    .unwrap();
    assert_eq!(abandoned.attempt_id, None);
}

// ---------------------------------------------------------------------------
// 5. Coordinator loss: finish admitted work, no new admissions, retain results.
// ---------------------------------------------------------------------------

#[test]
fn coordinator_loss_finishes_admitted_work_without_new_admissions() {
    let mut f = Fixture::new();
    let (task, attempt) = f.assign("task-loss", "attempt-loss", 1, EffectClass::Workspace);
    f.start_worker();
    let launch = f.wait_launch(&attempt);
    let pids = [launch.pid, launch.tool_pid];
    f.wait_for("Started accepted", Duration::from_secs(30), || {
        f.accepted_event(&attempt.id, "started")
    });
    f.server.stop();
    let outage = Instant::now();

    // A child admission requested during the loss is not admitted locally and
    // says why.
    let child = TaskSpec {
        title: "child".into(),
        instructions: "child work".into(),
        ..task.spec.clone()
    };
    f.trigger(json!({"action":"tool","name":"dacs_delegate","arguments":{"spec":child}}));
    let delegated = f.wait_tool_result("dacs_delegate");
    let text: Value =
        serde_json::from_str(delegated["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text["pending"], true, "{delegated}");
    assert!(text["coordinator_unavailable"].is_string(), "{delegated}");
    let delegate_operation = text["operation_id"].as_str().unwrap().to_owned();
    f.trigger(json!({"action":"tool","name":"dacs_pending","arguments":{}}));
    let pending = f.wait_tool_result("dacs_pending");
    assert_eq!(pending["isError"], true, "{pending}");
    assert!(
        pending["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("coordinator unavailable"),
        "{pending}"
    );

    // The admitted work finishes inside its reservation and its result waits.
    f.trigger(json!({"action":"commit"}));
    let committed = {
        f.wait_for("commit", Duration::from_secs(30), || {
            f.omp_log().iter().any(|e| e["kind"] == "committed")
        });
        f.omp_log()
            .into_iter()
            .find(|e| e["kind"] == "committed")
            .unwrap()
    };
    assert_eq!(committed["ok"], true);
    let evidence = json!([{"name":"fixture check","command":"true",
        "source_sha":committed["sha"],"passed":true,"artifact_digest":null}]);
    f.trigger(json!({"action":"tool","name":"dacs_submit","arguments":{"evidence":evidence}}));
    let submitted = f.wait_tool_result("dacs_submit");
    assert_ne!(submitted["isError"], true, "{submitted}");
    f.trigger(json!({"action":"settle"}));
    f.wait_for(
        "submission retained durably",
        Duration::from_secs(30),
        || {
            f.pending_results()
                .iter()
                .any(|r| r.payload["command"]["type"] == "submit_candidate")
        },
    );
    hold_alive(&f, &pids, Duration::from_secs(2));
    let status = dacs_worker::local_control::status(&f.config.data_dir).unwrap();
    assert!(
        status
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("Coordinator unavailable")),
        "{status:?}"
    );
    assert!(status.active_attempt_ids.contains(&attempt.id));
    let reconnect = Instant::now();
    f.coord().watch_pids = pids.to_vec();
    f.server.start();

    let release = f.wait_release(&attempt.id);
    assert!(
        release.alive.is_empty(),
        "released before stop: {release:?}"
    );
    let coord = f.coord();
    assert!(
        coord
            .commands
            .iter()
            .all(|r| r.at < outage || r.at > reconnect),
        "worker reached the coordinator during the outage"
    );
    // No local admission or reservation growth: only the explicitly requested
    // child was proposed, once, after reconnection; the coordinator refused it.
    let delegates: Vec<_> = coord
        .commands
        .iter()
        .filter(|r| r.command["type"] == "delegate")
        .collect();
    assert_eq!(delegates.len(), 1);
    assert_eq!(delegates[0].command_id, delegate_operation);
    assert!(delegates[0].at > reconnect);
    assert!(coord.commands.iter().all(|r| !matches!(
        r.command["type"].as_str(),
        Some("admit_task" | "create_task")
    )));
    for heartbeat in coord
        .commands
        .iter()
        .filter(|r| r.command["type"] == "heartbeat")
    {
        assert_eq!(
            heartbeat.command["registration"]["capacity"],
            json!(f.config.registration.capacity)
        );
    }
    // The retained submission carries the assignment's generation and pins.
    let submission = coord
        .commands
        .iter()
        .find(|r| r.command["type"] == "submit_candidate" && r.status == 200)
        .expect("retained submission delivered");
    let body = &submission.command["submission"];
    assert_eq!(body["task_id"], task.id.as_str());
    assert_eq!(body["attempt_id"], attempt.id.as_str());
    assert_eq!(body["generation"], 1);
    assert_eq!(body["contract_revision"], CONTRACT);
    assert_eq!(body["policy_revision"], POLICY);
    assert_eq!(body["commit_sha"], committed["sha"]);
    let source_upload = coord
        .uploads
        .iter()
        .find(|(_, path)| path.ends_with("/source"))
        .expect("candidate source uploaded");
    assert!(source_upload.0 > reconnect && source_upload.0 < submission.at);
    assert!(submission.at <= release.at);
}
