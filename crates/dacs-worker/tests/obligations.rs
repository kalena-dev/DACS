//! Asynchronous DACS obligations through the real worker loop (`runtime::run`).
//!
//! * Coordinator: the real `dacs-core` store behind a thin HTTP shim with the
//!   worker routes and error mapping of `dacs-app`. Admission, reservations,
//!   pending children and `child_result` messages are therefore the
//!   coordinator's own. The shim serves one fixed source bundle and accepts
//!   uploads without storing them. A second, test-driven worker ("peer") on
//!   another physical host runs the delegated children.
//! * OMP: a fake RPC server speaking the worker's JSONL framing (`omp.rs`): this
//!   test binary re-executed through a wrapper script. Frames go to fd 3 so
//!   libtest's own stdout cannot corrupt them. It calls the DACS host tools,
//!   commits in the real workspace and never contacts a model provider.
//! * Worker: a re-executed helper process, so it can be SIGKILLed and restarted
//!   against the same durable state. `HOME` is a temporary directory.
#![cfg(unix)]

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use dacs_core::Store;
use dacs_protocol::*;
use dacs_worker::{
    local_control::{self, ControlAction},
    runtime::WorkerConfig,
};
use nix::{errno::Errno, sys::signal, unistd::Pid};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    process::{Child, Command as Process, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const ADMIN: &str = "obligations-test-administrator-token-0123456789";
const FAKE_OMP_VERSION: &str = "0.0.0-dacs-fake";
const CONSENT_HOST: &str = "obligations coordinator";
const WORKER_CONFIG: &str = "DACS_OBLIGATIONS_WORKER_CONFIG";
const FAKE_CONTROL: &str = "DACS_FAKE_OMP_CONTROL";
const FAKE_ARGS: &str = "DACS_FAKE_OMP_ARGS";
/// Upper bound for a host tool to answer; the coordinator delay below is longer.
const PROMPT_TOOL_MS: u64 = 3000;
const SLOW_ADMISSION: Duration = Duration::from_secs(6);
/// Upper bound for asynchronous progress; generous for loaded shared machines.
const WAIT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Helper processes. Each returns immediately unless re-executed by a test.

#[test]
#[ignore = "helper process: real worker runtime for the obligations tests"]
fn obligations_worker_process() {
    let Some(path) = std::env::var_os(WORKER_CONFIG) else {
        return;
    };
    let config: WorkerConfig = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let outcome = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(dacs_worker::runtime::run(config));
    if let Err(error) = outcome {
        eprintln!("worker runtime failed: {error:#}");
        std::process::exit(1);
    }
}

#[test]
#[ignore = "helper process: fake OMP RPC server for the obligations tests"]
fn fake_omp_process() {
    let Some(control) = std::env::var_os(FAKE_CONTROL) else {
        return;
    };
    FakeOmp::start(PathBuf::from(control)).serve();
}

// ---------------------------------------------------------------------------
// Fake OMP: answers commands at once (OMP acknowledges and queues a prompt),
// runs one prompt at a time as a turn, and logs everything to `omp.jsonl`.

struct FakeOmp {
    control: PathBuf,
    out: std::fs::File,
    input: std::io::StdinLock<'static>,
    pid: u32,
    queue: VecDeque<(String, String)>,
    calls: u64,
    turns: u64,
}

fn classify(message: &str) -> &'static str {
    if message.to_ascii_lowercase().contains("readiness check") {
        "probe"
    } else if message.starts_with("DACS managed assignment") {
        if message.contains("This is a restart") {
            "restart"
        } else {
            "assignment"
        }
    } else if message.starts_with("Durable DACS mailbox message") {
        "mailbox"
    } else if message.starts_with("Durable DACS operation") {
        "operation"
    } else if message.starts_with("DACS submission refused") {
        "refused"
    } else {
        "other"
    }
}

impl FakeOmp {
    fn start(control: PathBuf) -> Self {
        // The wrapper script moved the worker's stdout pipe to fd 3.
        let out = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/fd/3")
            .expect("fake OMP frame descriptor");
        let mut fake = Self {
            control,
            out,
            input: std::io::stdin().lock(),
            pid: std::process::id(),
            queue: VecDeque::new(),
            calls: 0,
            turns: 0,
        };
        let args: Vec<String> = std::env::var(FAKE_ARGS)
            .unwrap_or_default()
            .split('\u{1f}')
            .map(str::to_owned)
            .collect();
        fake.log(json!({"event":"start","args":args,"cwd":std::env::current_dir().unwrap()}));
        fake.send(json!({"type":"ready","protocolVersion":1,"supportedProtocolVersions":[1]}));
        fake
    }

    fn log(&self, mut entry: Value) {
        entry["pid"] = json!(self.pid);
        entry["at_ms"] = json!(epoch_ms());
        let mut line = serde_json::to_vec(&entry).unwrap();
        line.push(b'\n');
        // One append per entry keeps lines whole across fake OMP processes.
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.control.join("omp.jsonl"))
            .and_then(|mut file| file.write_all(&line))
            .unwrap();
    }

    fn send(&mut self, frame: Value) {
        let mut line = serde_json::to_vec(&frame).unwrap();
        line.push(b'\n');
        if self
            .out
            .write_all(&line)
            .and_then(|()| self.out.flush())
            .is_err()
        {
            self.log(json!({"event":"transport_closed"}));
            std::process::exit(0);
        }
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        match self.input.read_line(&mut line) {
            Ok(0) | Err(_) => {
                // Like OMP in RPC mode: a closed transport ends the server.
                self.log(json!({"event":"stdin_closed"}));
                std::process::exit(0);
            }
            Ok(_) => serde_json::from_str(&line).expect("worker sent invalid JSONL"),
        }
    }

    /// Answers a command immediately; returns host tool results to the caller.
    fn handle(&mut self, frame: Value) -> Option<Value> {
        let kind = frame["type"].as_str().unwrap_or_default().to_owned();
        if kind == "host_tool_result" {
            return Some(frame);
        }
        let id = frame["id"].as_str().unwrap_or_default().to_owned();
        let mut entry = json!({"event":"command","type":kind});
        let data = match kind.as_str() {
            "prompt" => {
                let message = frame["message"].as_str().unwrap_or_default().to_owned();
                self.queue.push_back((id.clone(), message));
                json!({"agentInvoked":true})
            }
            "get_session_stats" => {
                json!({"tokens":{"input":100 * self.turns,"output":10 * self.turns}})
            }
            "open_session" => {
                entry["sessionDir"] = frame["sessionDir"].clone();
                Value::Null
            }
            "set_host_tools" => {
                entry["tools"] = frame["tools"]
                    .as_array()
                    .map(|tools| tools.iter().map(|tool| tool["name"].clone()).collect())
                    .unwrap_or_default();
                Value::Null
            }
            _ => Value::Null,
        };
        self.log(entry);
        self.send(json!({"type":"response","id":id,"command":kind,"success":true,"data":data}));
        None
    }

    fn serve(mut self) -> ! {
        loop {
            if let Some((id, message)) = self.queue.pop_front() {
                self.turn(&id, &message);
                continue;
            }
            let frame = self.read();
            if let Some(stray) = self.handle(frame) {
                self.log(json!({"event":"stray_tool_result","frame":stray}));
            }
        }
    }

    fn scenario(&self) -> Value {
        std::fs::read(self.control.join("scenario.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(Value::Null)
    }

    fn turn(&mut self, id: &str, message: &str) {
        self.turns += 1;
        let kind = classify(message);
        self.log(json!({"event":"prompt","kind":kind,"text":message}));
        let scenario = self.scenario();
        let tool = scenario["tool"]
            .as_str()
            .unwrap_or("dacs_delegate")
            .to_owned();
        match kind {
            "assignment" => {
                if scenario["refused_spec"].is_object() {
                    self.call_tool(&tool, json!({"spec":scenario["refused_spec"]}), "refused");
                }
                self.call_tool(&tool, json!({"spec":scenario["child_spec"]}), "child");
                if scenario["early_submit"] == true {
                    self.submit("early");
                }
            }
            // A child_result payload carries the child's task ID and state.
            "mailbox" if message.contains("\"task_id\"") => self.submit("after-child"),
            _ => {}
        }
        self.send(json!({"type":"agent_end"}));
        // The assignment turn yields first and reports quiet separately.
        let separately = kind == "assignment";
        self.send(
            json!({"type":"prompt_result","id":id,"status":"completed","sessionSettled":!separately}),
        );
        if separately {
            self.send(json!({"type":"session_settled"}));
        }
        self.log(json!({"event":"settled","kind":kind}));
    }

    fn call_tool(&mut self, name: &str, arguments: Value, label: &str) -> Value {
        self.calls += 1;
        let id = format!("host-{}-{}", self.pid, self.calls);
        let started = Instant::now();
        self.send(json!({"type":"host_tool_call","id":id,"toolCallId":format!("call-{}-{}", self.pid, self.calls),"toolName":name,"arguments":arguments}));
        let result = loop {
            let frame = self.read();
            if let Some(result) = self.handle(frame) {
                if result["id"] == id.as_str() {
                    break result;
                }
                self.log(json!({"event":"stray_tool_result","frame":result}));
            }
        };
        let text = result["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        self.log(json!({"event":"tool_result","tool":name,"label":label,
            "latency_ms":started.elapsed().as_millis() as u64,"is_error":result["isError"] == true,"text":text}));
        result
    }

    /// Commits a real change in the attempt workspace, then submits it.
    fn submit(&mut self, label: &str) {
        let cwd = std::env::current_dir().unwrap();
        std::fs::write(cwd.join(format!("obligation-{label}.txt")), label).unwrap();
        git(&cwd, &["add", "-A"]);
        git(&cwd, &["commit", "-q", "-m", &format!("fake OMP {label}")]);
        let head = git(&cwd, &["rev-parse", "HEAD"]);
        self.call_tool(
            "dacs_submit",
            json!({"evidence":[{"name":"fixture evidence","command":"fake OMP commit","source_sha":head,"passed":true,"artifact_digest":null}],"unresolved":[]}),
            label,
        );
    }
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Process::new("git")
        .args([
            "-c",
            "user.name=DACS fixture",
            "-c",
            "user.email=fixture@dacs.invalid",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn write_fake_omp(root: &Path, control: &Path) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    for path in [control, exe.as_path()] {
        assert!(
            !path.to_string_lossy().contains('\''),
            "unquotable fixture path"
        );
    }
    let script = format!(
        "#!/bin/sh\n\
         if [ \"$1\" = \"--version\" ]; then echo \"omp/{FAKE_OMP_VERSION}\"; exit 0; fi\n\
         {FAKE_CONTROL}='{control}'\nexport {FAKE_CONTROL}\n\
         IFS=$(printf '\\037')\n{FAKE_ARGS}=\"$*\"\nexport {FAKE_ARGS}\n\
         # RPC frames on fd 3 (the worker's pipe); libtest chatter goes to stderr.\n\
         exec 3>&1 1>&2\n\
         exec '{exe}' --exact fake_omp_process --ignored --nocapture --test-threads=1\n",
        control = control.display(),
        exe = exe.display(),
    );
    let path = root.join("fake-omp");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

// ---------------------------------------------------------------------------
// Coordinator shim over the real store.

#[derive(Clone, Debug)]
struct Observed {
    device: Option<String>,
    command: Command,
    ok: bool,
}

#[derive(Clone)]
struct Shim {
    store: Arc<Store>,
    bundle: Arc<Vec<u8>>,
    observed: Arc<Mutex<Vec<Observed>>>,
    /// One-shot: hold the response of the next admitted delegation this long.
    slow_admission: Arc<Mutex<Option<Duration>>>,
}

fn api_error(error: ApiError) -> Response {
    let status = match error.code {
        ErrorCode::Invalid => StatusCode::BAD_REQUEST,
        ErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
        ErrorCode::Forbidden => StatusCode::FORBIDDEN,
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::Conflict | ErrorCode::CapacityUnavailable => StatusCode::CONFLICT,
        ErrorCode::Io | ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(error)).into_response()
}

impl Shim {
    fn principal(&self, headers: &HeaderMap) -> Result<Principal, ApiError> {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or_else(|| ApiError::new(ErrorCode::Unauthorized, "Authentication required"))?;
        self.store.authenticate(token)
    }

    fn router(self) -> Router {
        async fn state(State(shim): State<Shim>, headers: HeaderMap) -> Response {
            match shim.principal(&headers) {
                Ok(p) => match shim.store.snapshot(&p) {
                    Ok(snapshot) => Json(snapshot).into_response(),
                    Err(error) => api_error(error),
                },
                Err(error) => api_error(error),
            }
        }
        async fn execute(
            State(shim): State<Shim>,
            headers: HeaderMap,
            Json(envelope): Json<CommandEnvelope>,
        ) -> Response {
            let p = match shim.principal(&headers) {
                Ok(p) => p,
                Err(error) => return api_error(error),
            };
            let command = envelope.command.clone();
            let outcome = shim.store.execute(&p, envelope);
            let admitted = matches!(command, Command::Delegate { .. }) && outcome.is_ok();
            shim.observed.lock().unwrap().push(Observed {
                device: p.device_id.clone(),
                command,
                ok: outcome.is_ok(),
            });
            let delay = admitted
                .then(|| shim.slow_admission.lock().unwrap().take())
                .flatten();
            if let Some(delay) = delay {
                // The admission is committed; only its response is slow.
                tokio::time::sleep(delay).await;
            }
            match outcome {
                Ok(response) => Json(response).into_response(),
                Err(error) => api_error(error),
            }
        }
        #[derive(serde::Deserialize)]
        struct Cursor {
            #[serde(default)]
            after: i64,
        }
        async fn commands(
            State(shim): State<Shim>,
            headers: HeaderMap,
            Query(cursor): Query<Cursor>,
        ) -> Response {
            match shim.principal(&headers) {
                Ok(p) => match shim.store.commands(&p, cursor.after) {
                    Ok(messages) => Json(messages).into_response(),
                    Err(error) => api_error(error),
                },
                Err(error) => api_error(error),
            }
        }
        async fn source(State(shim): State<Shim>, headers: HeaderMap) -> Response {
            match shim.principal(&headers) {
                Ok(_) => shim.bundle.as_ref().clone().into_response(),
                Err(error) => api_error(error),
            }
        }
        async fn upload(State(shim): State<Shim>, headers: HeaderMap, body: Bytes) -> Response {
            match shim.principal(&headers) {
                Ok(_) => Json(json!({"stored":true,"bytes":body.len()})).into_response(),
                Err(error) => api_error(error),
            }
        }
        Router::new()
            .route("/api/state", get(state))
            .route("/api/command", post(execute))
            .route("/api/commands", get(commands))
            .route("/api/projects/{id}/source", get(source).post(upload))
            .route("/api/projects/{id}/artifacts/{digest}", post(upload))
            .with_state(self)
    }
}

fn execute(store: &Store, principal: &Principal, command: Command) -> Result<Value, ApiError> {
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
}

fn admin() -> Principal {
    Principal {
        is_admin: true,
        device_id: None,
    }
}

fn registration(
    project: &Project,
    host: &str,
    capabilities: &[&str],
    capacity: Resources,
) -> WorkerRegistration {
    WorkerRegistration {
        name: format!("{host} worker"),
        physical_host_id: host.into(),
        os: std::env::consts::OS.into(),
        architecture: std::env::consts::ARCH.into(),
        capabilities: capabilities.iter().map(|c| (*c).into()).collect(),
        capacity,
        omp_version: FAKE_OMP_VERSION.into(),
        policy_revision: project.policy_revision,
        owner_consented: true,
        consent_host: CONSENT_HOST.into(),
    }
}

fn enroll(store: &Store, project: &Project, registration: WorkerRegistration) -> Enrollment {
    let invitation: Invitation = serde_json::from_value(
        execute(
            store,
            &admin(),
            Command::CreateInvitation {
                project_id: project.id.clone(),
                host_name: CONSENT_HOST.into(),
                expires_in_seconds: 3600,
            },
        )
        .unwrap(),
    )
    .unwrap();
    let enrollment = store.enroll(&invitation.token, registration).unwrap();
    execute(
        store,
        &admin(),
        Command::ApproveWorker {
            worker_id: enrollment.worker.id.clone(),
        },
    )
    .unwrap();
    enrollment
}

// ---------------------------------------------------------------------------
// Fixture.

/// Samples coordinator reservations on the worker under test until stopped.
struct Sample {
    reserved_slots: u32,
    attempts_on_worker: Vec<String>,
    candidate_submitted: bool,
}

struct Sampler {
    stop: Arc<AtomicBool>,
    join: std::thread::JoinHandle<Vec<Sample>>,
}

impl Sampler {
    fn start(store: Arc<Store>, worker: String, attempt: String) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let join = std::thread::spawn(move || {
            let mut samples = Vec::new();
            while !flag.load(Ordering::SeqCst) {
                let snapshot = store.snapshot(&admin()).unwrap();
                samples.push(Sample {
                    reserved_slots: snapshot
                        .workers
                        .iter()
                        .find(|w| w.id == worker)
                        .unwrap()
                        .reserved
                        .agent_slots,
                    attempts_on_worker: snapshot
                        .attempts
                        .iter()
                        .filter(|a| a.worker_id == worker)
                        .map(|a| a.id.clone())
                        .collect(),
                    candidate_submitted: snapshot
                        .candidates
                        .iter()
                        .any(|c| c.submission.attempt_id == attempt),
                });
                std::thread::sleep(Duration::from_millis(25));
            }
            samples
        });
        Self { stop, join }
    }

    /// Every sample before the explicit submission shows the parent's own
    /// reservation, unreleased and not lent to any other attempt.
    fn assert_parent_reservation_held(self, attempt: &str) {
        self.stop.store(true, Ordering::SeqCst);
        let samples = self.join.join().unwrap();
        let waiting: Vec<_> = samples.iter().filter(|s| !s.candidate_submitted).collect();
        assert!(
            waiting.len() > 20,
            "too few reservation samples: {}",
            waiting.len()
        );
        for sample in waiting {
            assert_eq!(
                sample.reserved_slots, 1,
                "parent reservation released while waiting"
            );
            assert_eq!(
                sample.attempts_on_worker,
                vec![attempt.to_owned()],
                "worker slot lent to another attempt"
            );
        }
    }
}

struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    store: Arc<Store>,
    shim: Shim,
    project: Project,
    worker_id: String,
    peer: Principal,
    source_commit: String,
    control: PathBuf,
    data_dir: PathBuf,
    config_path: PathBuf,
    home: PathBuf,
    worker: Option<Child>,
}

impl Fixture {
    async fn new(scenario: impl FnOnce(&Fixture) -> Value) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let source = root.join("source");
        std::fs::create_dir_all(&source).unwrap();
        git(&source, &["init", "-q", "-b", "main"]);
        std::fs::write(source.join("README.md"), "obligations fixture\n").unwrap();
        git(&source, &["add", "README.md"]);
        git(&source, &["commit", "-q", "-m", "fixture source"]);
        let source_commit = git(&source, &["rev-parse", "HEAD"]);
        let bundle = root.join("source.bundle");
        git(
            &source,
            &["bundle", "create", bundle.to_str().unwrap(), "--all"],
        );

        let store = Arc::new(Store::open(root.join("coordinator.sqlite"), ADMIN).unwrap());
        let project: Project = serde_json::from_value(
            execute(
                &store,
                &admin(),
                Command::CreateProject {
                    name: "obligations".into(),
                    target_branch: "main".into(),
                    max_tasks: 100,
                    max_depth: 8,
                },
            )
            .unwrap(),
        )
        .unwrap();
        let suffix = &uuid::Uuid::new_v4().simple().to_string()[..12];
        // Small enough for any CI host: the worker refuses declarations above
        // the observed machine.
        let worker_registration = registration(
            &project,
            &format!("obligations-worker-{suffix}"),
            &["rust"],
            Resources {
                agent_slots: 1,
                cpu_millis: 1000,
                memory_mb: 512,
                disk_mb: 512,
            },
        );
        let worker = enroll(&store, &project, worker_registration.clone());
        let peer = enroll(
            &store,
            &project,
            registration(
                &project,
                &format!("obligations-peer-{suffix}"),
                &["rust", "peer-builder"],
                Resources {
                    agent_slots: 2,
                    cpu_millis: 4000,
                    memory_mb: 4096,
                    disk_mb: 4096,
                },
            ),
        );
        let peer = store.authenticate(&peer.device_token).unwrap();
        execute(
            &store,
            &peer,
            Command::ReportRuntimeStatus {
                status: WorkerRuntimeStatus {
                    worker_id: String::new(),
                    provider_authenticated: Some(true),
                    network_ready: Some(true),
                    desktop_session: None,
                    desktop_active: false,
                    desktop_paused: false,
                    observed_at: 0,
                },
            },
        )
        .unwrap();

        let shim = Shim {
            store: store.clone(),
            bundle: Arc::new(std::fs::read(&bundle).unwrap()),
            observed: Arc::default(),
            slow_admission: Arc::default(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router = shim.clone().router();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        let home = root.join("home");
        let control = root.join("omp-control");
        let data_dir = root.join("worker-data");
        for directory in [&home, &control, &data_dir] {
            std::fs::create_dir_all(directory).unwrap();
        }
        let credential = root.join("device.token");
        std::fs::write(&credential, &worker.device_token).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600)).unwrap();
        let config = WorkerConfig {
            coordinator_url: url,
            credential_file: credential,
            worker_id: worker.worker.id.clone(),
            registration: worker_registration,
            omp_executable: write_fake_omp(&root, &control),
            omp_version: FAKE_OMP_VERSION.into(),
            omp_profile: None,
            data_dir: data_dir.clone(),
            workspaces_dir: root.join("workspaces"),
            ca_certificate: None,
            poll_interval_ms: 100,
        };
        let config_path = root.join("worker.json");
        std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        let fixture = Self {
            _temporary: temporary,
            root,
            store,
            shim,
            project,
            worker_id: worker.worker.id,
            peer,
            source_commit,
            control,
            data_dir,
            config_path,
            home,
            worker: None,
        };
        let scenario = scenario(&fixture);
        std::fs::write(
            fixture.control.join("scenario.json"),
            serde_json::to_vec(&scenario).unwrap(),
        )
        .unwrap();
        fixture
    }

    fn spec(
        &self,
        title: &str,
        effect_class: EffectClass,
        execution: TaskExecution,
        capabilities: &[&str],
        resources: Resources,
    ) -> TaskSpec {
        TaskSpec {
            project_id: self.project.id.clone(),
            title: title.into(),
            instructions: format!("{title}: obligations fixture"),
            source_commit: self.source_commit.clone(),
            execution,
            dependencies: Vec::new(),
            required_capabilities: capabilities.iter().map(|c| (*c).into()).collect(),
            resources,
            effect_class,
            lease_seconds: 300,
            max_runtime_seconds: 3600,
            contract_revision: self.project.contract_revision,
            policy_revision: self.project.policy_revision,
            max_retries: 0,
        }
    }

    fn agent_slot() -> Resources {
        Resources {
            agent_slots: 1,
            cpu_millis: 500,
            memory_mb: 128,
            disk_mb: 128,
        }
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
                "obligations_worker_process",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(WORKER_CONFIG, &self.config_path)
            .env("HOME", &self.home)
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        self.worker = Some(child);
    }

    /// SIGKILL: no destructor, drain or Stopped report runs.
    fn kill_worker(&mut self) {
        let mut child = self.worker.take().expect("worker running");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    fn worker_running(&mut self) -> bool {
        self.worker
            .as_mut()
            .is_some_and(|child| child.try_wait().unwrap().is_none())
    }

    async fn drain_worker(&mut self) {
        local_control::request(&self.data_dir, ControlAction::Drain).unwrap();
        let mut child = self.worker.take().expect("worker running");
        let deadline = Instant::now() + WAIT;
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "worker did not drain");
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert!(status.success(), "worker exited with {status}");
        for pid in self.omp_pids() {
            wait_until("fake OMP exit", Duration::from_secs(10), || {
                (!alive(pid)).then_some(())
            })
            .await;
        }
    }

    fn snapshot(&self) -> Snapshot {
        self.store.snapshot(&admin()).unwrap()
    }

    fn omp_log(&self) -> Vec<Value> {
        std::fs::read_to_string(self.control.join("omp.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn omp_pids(&self) -> Vec<i32> {
        let mut pids: Vec<i32> = self
            .omp_log()
            .iter()
            .filter(|e| e["event"] == "start")
            .map(|e| e["pid"].as_i64().unwrap() as i32)
            .collect();
        pids.dedup();
        pids
    }

    async fn wait_omp(&self, what: &str, matches: impl Fn(&Value) -> bool) -> Value {
        wait_until(what, WAIT, || {
            self.omp_log().into_iter().find(|entry| matches(entry))
        })
        .await
    }

    async fn wait_provider_ready(&self) {
        wait_until("worker provider readiness", WAIT, || {
            self.snapshot()
                .runtime_status
                .iter()
                .any(|s| s.worker_id == self.worker_id && s.provider_authenticated == Some(true))
                .then_some(())
        })
        .await;
    }

    fn admit_parent(&self, effect_class: EffectClass) -> (TaskRecord, AttemptRecord) {
        let task: TaskRecord = serde_json::from_value(
            execute(
                &self.store,
                &admin(),
                Command::CreateTask {
                    spec: self.spec(
                        "parent awaiting background work",
                        effect_class,
                        TaskExecution::Agent,
                        &["rust"],
                        Self::agent_slot(),
                    ),
                },
            )
            .unwrap(),
        )
        .unwrap();
        let attempt: AttemptRecord = serde_json::from_value(
            execute(
                &self.store,
                &admin(),
                Command::AdmitTask {
                    task_id: task.id.clone(),
                    worker_id: Some(self.worker_id.clone()),
                },
            )
            .unwrap(),
        )
        .unwrap();
        (task, attempt)
    }

    fn child_of(&self, parent: &TaskRecord) -> Vec<(TaskRecord, AttemptRecord)> {
        let snapshot = self.snapshot();
        snapshot
            .tasks
            .iter()
            .filter(|t| t.parent_task_id.as_deref() == Some(&parent.id))
            .map(|t| {
                let attempt = snapshot
                    .attempts
                    .iter()
                    .find(|a| Some(&a.id) == t.attempt_id.as_ref())
                    .expect("delegated child is admitted, not queued")
                    .clone();
                (t.clone(), attempt)
            })
            .collect()
    }

    /// The peer worker reports its execution of a delegated child.
    fn peer_reports(&self, attempt: &AttemptRecord, events: Vec<WorkerEvent>) {
        for (index, event) in events.into_iter().enumerate() {
            execute(
                &self.store,
                &self.peer,
                Command::ReportEvent {
                    attempt_id: attempt.id.clone(),
                    generation: attempt.generation,
                    sequence: index as u64 + 1,
                    event,
                },
            )
            .unwrap();
        }
    }

    fn worker_events(&self, attempt: &str) -> Vec<WorkerEvent> {
        self.shim
            .observed
            .lock()
            .unwrap()
            .iter()
            .filter(|o| o.ok && o.device.as_deref() == Some(&self.worker_id))
            .filter_map(|o| match &o.command {
                Command::ReportEvent {
                    attempt_id, event, ..
                } if attempt_id == attempt => Some(event.clone()),
                _ => None,
            })
            .collect()
    }

    fn assert_not_completed(&self, task: &TaskRecord, attempt: &AttemptRecord) {
        let snapshot = self.snapshot();
        let current = snapshot.tasks.iter().find(|t| t.id == task.id).unwrap();
        assert_eq!(current.state, TaskState::Executing, "{current:?}");
        assert_eq!(current.attempt_id.as_deref(), Some(attempt.id.as_str()));
        assert_eq!(current.generation, attempt.generation);
        assert!(
            !snapshot
                .candidates
                .iter()
                .any(|c| c.submission.task_id == task.id),
            "attempt completed without an accepted explicit submission"
        );
        let events = self.worker_events(&attempt.id);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, WorkerEvent::Stopped | WorkerEvent::Failed { .. })),
            "parent attempt stopped or failed while waiting: {events:?}"
        );
    }

    fn assert_no_lending(&self) {
        let other: TaskRecord = serde_json::from_value(
            execute(
                &self.store,
                &admin(),
                Command::CreateTask {
                    spec: self.spec(
                        "would borrow the waiting slot",
                        EffectClass::Workspace,
                        TaskExecution::Agent,
                        &["rust"],
                        Self::agent_slot(),
                    ),
                },
            )
            .unwrap(),
        )
        .unwrap();
        let refused = execute(
            &self.store,
            &admin(),
            Command::AdmitTask {
                task_id: other.id.clone(),
                worker_id: Some(self.worker_id.clone()),
            },
        )
        .unwrap_err();
        assert_eq!(refused.code, ErrorCode::CapacityUnavailable, "{refused:?}");
        execute(
            &self.store,
            &admin(),
            Command::CancelTask { task_id: other.id },
        )
        .unwrap();
    }

    async fn wait_candidate(&self, attempt: &AttemptRecord) -> CandidateRecord {
        wait_until("accepted explicit submission", WAIT, || {
            self.snapshot()
                .candidates
                .into_iter()
                .find(|c| c.submission.attempt_id == attempt.id)
        })
        .await
    }

    async fn wait_released(&self, attempt: &AttemptRecord) {
        wait_until("stop reconciliation release", WAIT, || {
            let snapshot = self.snapshot();
            let reserved = snapshot
                .workers
                .iter()
                .find(|w| w.id == self.worker_id)
                .unwrap()
                .reserved
                .agent_slots;
            let stopped = self
                .worker_events(&attempt.id)
                .iter()
                .any(|e| matches!(e, WorkerEvent::Stopped));
            (stopped && reserved == 0).then_some(())
        })
        .await;
    }

    fn session_dir(&self, attempt: &AttemptRecord) -> String {
        self.data_dir
            .join("attempts")
            .join(&attempt.id)
            .join("session")
            .to_string_lossy()
            .into_owned()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut child) = self.worker.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        for pid in self.omp_pids() {
            let _ = signal::killpg(Pid::from_raw(pid), signal::Signal::SIGKILL);
        }
        if std::thread::panicking() || std::env::var_os("DACS_OBLIGATIONS_DUMP").is_some() {
            let log = std::fs::read_to_string(self.root.join("worker.log")).unwrap_or_default();
            eprintln!("--- worker log ---\n{log}");
            for entry in self.omp_log() {
                let mut entry = entry;
                if let Some(text) = entry["text"].as_str() {
                    entry["text"] = json!(text.chars().take(300).collect::<String>());
                }
                eprintln!("omp: {entry}");
            }
            for observed in self.shim.observed.lock().unwrap().iter() {
                eprintln!("coordinator: ok={} {:?}", observed.ok, observed.command);
            }
        }
    }
}

fn alive(pid: i32) -> bool {
    match signal::kill(Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        Err(error) => panic!("cannot inspect {pid}: {error}"),
    }
}

async fn wait_until<T>(what: &str, timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn tool_json(entry: &Value) -> Value {
    serde_json::from_str(entry["text"].as_str().unwrap()).unwrap_or(Value::Null)
}

fn agent_child(fixture: &Fixture) -> Value {
    serde_json::to_value(fixture.spec(
        "delegated child on the peer",
        EffectClass::Workspace,
        TaskExecution::Agent,
        &["rust", "peer-builder"],
        Fixture::agent_slot(),
    ))
    .unwrap()
}

/// Shared prefix of the delegation scenarios: the parent delegates to the peer,
/// gets the child ID within the tool call, then its OMP session settles.
async fn delegated_and_settled(
    f: &mut Fixture,
) -> (TaskRecord, AttemptRecord, TaskRecord, AttemptRecord, i64) {
    f.start_worker();
    f.wait_provider_ready().await;
    let (parent, attempt) = f.admit_parent(EffectClass::Workspace);
    let delegated = f
        .wait_omp("delegation tool result", |e| {
            e["event"] == "tool_result" && e["label"] == "child"
        })
        .await;
    assert_eq!(delegated["is_error"], false, "{delegated}");
    assert!(
        delegated["latency_ms"].as_u64().unwrap() < PROMPT_TOOL_MS,
        "{delegated}"
    );
    let receipt = tool_json(&delegated);
    assert_eq!(receipt["admitted"], true, "{delegated}");
    let children = f.child_of(&parent);
    assert_eq!(children.len(), 1);
    let (child, child_attempt) = children.into_iter().next().unwrap();
    assert_eq!(receipt["task"]["id"], child.id.as_str());
    assert_ne!(
        child_attempt.worker_id, f.worker_id,
        "child must run on the peer"
    );
    let session = delegated["pid"].as_i64().unwrap();
    f.wait_omp("assignment turn settled", |e| {
        e["event"] == "settled" && e["kind"] == "assignment" && e["pid"] == session
    })
    .await;
    wait_until("parent settled at coordinator", WAIT, || {
        f.snapshot()
            .attempts
            .into_iter()
            .find(|a| a.id == attempt.id && a.settled)
    })
    .await;
    (parent, attempt, child, child_attempt, session)
}

/// After the OMP session was replaced, the new session reopened the durable
/// session directory and was told about the still-pending child.
async fn assert_obligations_restored(
    f: &Fixture,
    attempt: &AttemptRecord,
    child: &TaskRecord,
    old_session: i64,
) -> i64 {
    let reopened = f
        .wait_omp("restarted OMP reopened its session", |e| {
            e["event"] == "command" && e["type"] == "open_session" && e["pid"] != old_session
        })
        .await;
    assert_eq!(reopened["sessionDir"], f.session_dir(attempt).as_str());
    let session = reopened["pid"].as_i64().unwrap();
    let restart = f
        .wait_omp("restart prompt", |e| {
            e["event"] == "prompt" && e["kind"] == "restart" && e["pid"] == session
        })
        .await;
    assert!(
        restart["text"].as_str().unwrap().contains(&child.id),
        "restart prompt must restore the pending child: {restart}"
    );
    // No tool call survived or was replayed: one delegation, one child.
    let log = f.omp_log();
    assert_eq!(
        log.iter()
            .filter(|e| e["event"] == "tool_result" && e["label"] == "child")
            .count(),
        1
    );
    assert_eq!(f.child_of_id(&child.parent_task_id).len(), 1);
    session
}

impl Fixture {
    fn child_of_id(&self, parent: &Option<String>) -> Vec<TaskRecord> {
        self.snapshot()
            .tasks
            .into_iter()
            .filter(|t| t.parent_task_id == *parent)
            .collect()
    }
}

/// The peer finishes the child; the durable child_result wakes the parent's
/// current OMP session, which then explicitly submits.
async fn child_result_wakes_parent(
    f: &Fixture,
    attempt: &AttemptRecord,
    child: &TaskRecord,
    child_attempt: &AttemptRecord,
    session: i64,
) -> CandidateRecord {
    f.peer_reports(
        child_attempt,
        vec![
            WorkerEvent::Started,
            WorkerEvent::Failed {
                message: "peer finished the delegated child without a candidate".into(),
            },
            WorkerEvent::Stopped,
        ],
    );
    let wake = f
        .wait_omp("child_result delivered to OMP", |e| {
            e["event"] == "prompt" && e["kind"] == "mailbox" && {
                let text = e["text"].as_str().unwrap();
                text.contains(&child.id) && text.contains("\"failed\"")
            }
        })
        .await;
    assert_eq!(wake["pid"], session, "result must wake the waiting session");
    let candidate = f.wait_candidate(attempt).await;
    assert_eq!(candidate.submission.generation, attempt.generation);
    candidate
}

// ---------------------------------------------------------------------------
// Tests.

/// Items 1, 3 and 4: a long-running job tool returns its ID without waiting for
/// a slow coordinator; the job outcome arrives through durable messages; OMP
/// yielding/settling and an early explicit submission do not complete the
/// attempt; the waiting parent's reservation is neither released nor lent, and
/// the background result wakes the same OMP session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn background_job_returns_promptly_and_waiting_parent_keeps_reservation() {
    let mut f = Fixture::new(|f| {
        let job = |title: &str, capability: &str| {
            f.spec(
                title,
                EffectClass::External,
                TaskExecution::Workflow {
                    request: json!({"kind":"fixture-build"}),
                },
                &[capability],
                Resources {
                    agent_slots: 0,
                    cpu_millis: 500,
                    memory_mb: 128,
                    disk_mb: 128,
                },
            )
        };
        json!({
            "tool": "dacs_workflow",
            "refused_spec": job("unplaceable job", "no-such-builder"),
            "child_spec": job("background build job", "peer-builder"),
            "early_submit": true,
        })
    })
    .await;
    *f.shim.slow_admission.lock().unwrap() = Some(SLOW_ADMISSION);
    f.start_worker();
    f.wait_provider_ready().await;
    let (parent, attempt) = f.admit_parent(EffectClass::External);
    let sampler = Sampler::start(f.store.clone(), f.worker_id.clone(), attempt.id.clone());

    // A refusal is returned promptly as a tool error.
    let refused = f
        .wait_omp("refused job tool result", |e| {
            e["event"] == "tool_result" && e["label"] == "refused"
        })
        .await;
    assert_eq!(refused["is_error"], true, "{refused}");
    assert!(
        refused["text"]
            .as_str()
            .unwrap()
            .contains("capacity_unavailable")
    );
    assert!(refused["latency_ms"].as_u64().unwrap() < PROMPT_TOOL_MS);

    // The admission is committed but its response takes 3 s: the tool returns a
    // durable operation ID instead of blocking the OMP turn.
    let submitted = f
        .wait_omp("job tool result", |e| {
            e["event"] == "tool_result" && e["label"] == "child"
        })
        .await;
    assert_eq!(submitted["is_error"], false, "{submitted}");
    assert!(
        submitted["latency_ms"].as_u64().unwrap() < PROMPT_TOOL_MS,
        "job tool waited for the coordinator: {submitted}"
    );
    let pending = tool_json(&submitted);
    assert_eq!(pending["pending"], true, "{submitted}");
    let operation = pending["operation_id"].as_str().unwrap().to_owned();
    let session = submitted["pid"].as_i64().unwrap();
    let (child, child_attempt) = f.child_of(&parent).into_iter().next().unwrap();
    assert_eq!(
        f.child_of(&parent).len(),
        1,
        "replayed admission duplicated the job"
    );
    assert_ne!(child_attempt.worker_id, f.worker_id);

    // The admission outcome (with the job's task ID) arrives later, durably.
    let admitted = f
        .wait_omp("durable admission outcome", |e| {
            e["event"] == "prompt"
                && e["kind"] == "operation"
                && e["text"].as_str().unwrap().contains(&operation)
        })
        .await;
    assert_eq!(admitted["pid"], session);
    assert!(admitted["text"].as_str().unwrap().contains(&child.id));

    // Explicit submission while the job is pending is refused; nothing completes.
    let early = f
        .wait_omp("early submission tool result", |e| {
            e["event"] == "tool_result" && e["label"] == "early"
        })
        .await;
    assert_eq!(tool_json(&early)["pending"], true, "{early}");
    let refusal = f
        .wait_omp("submission refusal prompt", |e| {
            e["event"] == "prompt" && e["kind"] == "refused"
        })
        .await;
    assert_eq!(refusal["pid"], session);
    assert!(
        refusal["text"].as_str().unwrap().contains("unfinished"),
        "{refusal}"
    );
    for kind in ["refused", "operation"] {
        f.wait_omp("quiet session", |e| {
            e["event"] == "settled" && e["kind"] == kind
        })
        .await;
    }
    let waiting = wait_until("parent settled at coordinator", WAIT, || {
        f.snapshot()
            .attempts
            .into_iter()
            .find(|a| a.id == attempt.id && a.settled)
    })
    .await;
    assert_eq!(waiting.pending_children, vec![child.id.clone()]);
    // Quietness is not completion: hold the settled state and look again.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    f.assert_not_completed(&parent, &attempt);
    assert!(alive(session as i32), "settled OMP session was torn down");
    assert!(
        local_control::status(&f.data_dir)
            .unwrap()
            .active_attempt_ids
            .contains(&attempt.id)
    );
    f.assert_no_lending();

    // The job finishes on the peer; its result wakes the same session.
    f.peer_reports(
        &child_attempt,
        vec![
            WorkerEvent::Started,
            WorkerEvent::Settled,
            WorkerEvent::WorkflowCompleted {
                success: true,
                artifact_digest: "a".repeat(64),
                evidence: vec![CheckEvidence {
                    name: "peer build".into(),
                    command: "cargo build".into(),
                    source_sha: f.source_commit.clone(),
                    passed: true,
                    artifact_digest: None,
                }],
            },
            WorkerEvent::Stopped,
        ],
    );
    let wake = f
        .wait_omp("job result delivered to OMP", |e| {
            e["event"] == "prompt" && e["kind"] == "mailbox" && {
                let text = e["text"].as_str().unwrap();
                text.contains(&child.id) && text.contains("\"accepted\"")
            }
        })
        .await;
    assert_eq!(wake["pid"], session, "result must wake the waiting session");
    let candidate = f.wait_candidate(&attempt).await;
    assert_eq!(candidate.submission.generation, attempt.generation);
    sampler.assert_parent_reservation_held(&attempt.id);
    f.wait_released(&attempt).await;
    assert!(
        !f.omp_log().iter().any(|e| e["type"] == "open_session"),
        "the waiting session was never restarted"
    );
    f.drain_worker().await;
}

/// Items 1 and 2: the worker is SIGKILLed while its parent waits on a delegated
/// child. The restarted worker reattaches the same attempt, reopens the OMP
/// session and restores the pending child; the child's later result wakes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_delegation_survives_worker_crash_and_restart() {
    let mut f = Fixture::new(|f| json!({"tool":"dacs_delegate","child_spec":agent_child(f)})).await;
    let (parent, attempt, child, child_attempt, first) = delegated_and_settled(&mut f).await;
    let sampler = Sampler::start(f.store.clone(), f.worker_id.clone(), attempt.id.clone());
    f.kill_worker();
    f.assert_not_completed(&parent, &attempt);
    f.start_worker();
    let session = assert_obligations_restored(&f, &attempt, &child, first).await;
    f.assert_not_completed(&parent, &attempt);
    child_result_wakes_parent(&f, &attempt, &child, &child_attempt, session).await;
    sampler.assert_parent_reservation_held(&attempt.id);
    f.wait_released(&attempt).await;
    f.drain_worker().await;
}

/// Item 2: the OMP process dies (SIGKILL) while its parent waits on a delegated
/// child. The worker restarts OMP for the same attempt instead of failing it;
/// the obligation survives and the child's result wakes the new session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_delegation_survives_omp_process_crash() {
    let mut f = Fixture::new(|f| json!({"tool":"dacs_delegate","child_spec":agent_child(f)})).await;
    let (parent, attempt, child, child_attempt, first) = delegated_and_settled(&mut f).await;
    let sampler = Sampler::start(f.store.clone(), f.worker_id.clone(), attempt.id.clone());
    signal::kill(Pid::from_raw(first as i32), signal::Signal::SIGKILL).unwrap();
    let session = assert_obligations_restored(&f, &attempt, &child, first).await;
    assert!(f.worker_running(), "worker must survive an OMP crash");
    f.assert_not_completed(&parent, &attempt);
    child_result_wakes_parent(&f, &attempt, &child, &child_attempt, session).await;
    sampler.assert_parent_reservation_held(&attempt.id);
    f.wait_released(&attempt).await;
    f.drain_worker().await;
}
