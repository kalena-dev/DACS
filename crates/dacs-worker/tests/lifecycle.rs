//! Lifecycle and local controls: drain versus cancellation versus emergency
//! stop, permission reduction, desktop slot/pause and the local-only control
//! surface. Every process check uses real child processes and an independent
//! kernel probe (`kill(pid, 0)` -> ESRCH), never the supervisor's own claim
//! or an RPC abort reply.
#![cfg(unix)]

use dacs_protocol::{
    AttemptRecord, EffectClass, Resources, TaskExecution, TaskRecord, TaskSpec, TaskState,
};
use dacs_worker::{
    Journal, ManagedChild, Supervisor,
    local_control::{self, ControlAction, OwnerControls, StopPhase},
};
use nix::{errno::Errno, sys::signal::kill, unistd::Pid};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn alive(pid: i32) -> bool {
    match kill(Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        Err(error) => panic!("cannot inspect {pid}: {error}"),
    }
}

fn read_pid(directory: &Path, name: &str) -> Option<i32> {
    std::fs::read_to_string(directory.join(name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

async fn wait_for(what: &str, timeout: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Kills fixture process groups if an assertion fails, so no test leaks trees.
struct GroupGuard(Vec<i32>);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        for group in &self.0 {
            let _ =
                nix::sys::signal::killpg(Pid::from_raw(*group), nix::sys::signal::Signal::SIGKILL);
        }
    }
}

fn shell(directory: &Path, script: &str, attempt: &str) -> ManagedChild {
    ManagedChild {
        name: "lifecycle fixture".into(),
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), script.into()],
        cwd: directory.to_owned(),
        env: std::env::vars()
            .filter(|(name, _)| matches!(name.as_str(), "PATH" | "HOME" | "TMPDIR"))
            .collect::<BTreeMap<_, _>>(),
        effect_class: "workspace".into(),
        reservation_id: Some(attempt.into()),
    }
}

/// Drain is observation only: admitted trees are reported active, are not
/// signalled, and finish on their own terms (exit 0, not a kill signal).
#[tokio::test]
async fn drain_reports_admitted_tree_without_signalling_it() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().canonicalize().unwrap();
    let supervisor = Supervisor::new();
    supervisor
        .launch(
            "admitted",
            &shell(
                &directory,
                "sleep 600 & child=$!; echo $child > child.pid; echo $$ > leader.pid; \
                 while [ ! -e release ]; do sleep 0.05; done; kill $child; wait $child; exit 0",
                "attempt-drain",
            ),
        )
        .await
        .unwrap();
    wait_for("fixture pids", Duration::from_secs(10), || {
        read_pid(&directory, "child.pid").is_some() && read_pid(&directory, "leader.pid").is_some()
    })
    .await;
    let (leader, child) = (
        read_pid(&directory, "leader.pid").unwrap(),
        read_pid(&directory, "child.pid").unwrap(),
    );
    let _guard = GroupGuard(vec![leader]);
    // While draining the runtime only polls; the work keeps running.
    for _ in 0..10 {
        assert_eq!(supervisor.active_ids().await, vec!["admitted".to_owned()]);
        assert_eq!(supervisor.active_attempts().await, vec!["attempt-drain"]);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        alive(leader) && alive(child),
        "drain signalled admitted work"
    );
    std::fs::write(directory.join("release"), b"").unwrap();
    wait_for("admitted work finishes", Duration::from_secs(10), || {
        !alive(child)
    })
    .await;
    loop {
        if let dacs_worker::ProcessState::Exited { code } =
            supervisor.state("admitted").await.unwrap()
        {
            assert_eq!(code, Some(0), "drained work must finish, not be killed");
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Stop accounting after natural completion still verifies the group.
    assert!(
        supervisor
            .terminate_attempt("attempt-drain", Duration::from_secs(5))
            .await
            .unwrap()
    );
    assert!(!alive(leader) && !alive(child));
    assert!(supervisor.active_ids().await.is_empty());
}

/// Emergency cancellation kills the whole owned tree, including descendants
/// that ignore polite signals, and fences any further launch for the attempt.
#[tokio::test]
async fn emergency_termination_kills_whole_tree_including_signal_ignoring_descendants() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().canonicalize().unwrap();
    let supervisor = Supervisor::new();
    supervisor
        .launch(
            "omp",
            &shell(
                &directory,
                "trap '' TERM INT HUP; \
                 sh -c 'trap \"\" TERM INT HUP; sleep 600 & echo $! > grandchild.pid; \
                        while :; do sleep 1; done' & \
                 echo $! > child.pid; echo $$ > leader.pid; while :; do sleep 1; done",
                "attempt-emergency",
            ),
        )
        .await
        .unwrap();
    let names = ["leader.pid", "child.pid", "grandchild.pid"];
    wait_for("fixture pids", Duration::from_secs(10), || {
        names.iter().all(|n| read_pid(&directory, n).is_some())
    })
    .await;
    let pids: Vec<i32> = names
        .iter()
        .map(|n| read_pid(&directory, n).unwrap())
        .collect();
    let _guard = GroupGuard(vec![pids[0]]);
    assert!(pids.iter().all(|pid| alive(*pid)));
    let started = Instant::now();
    assert!(
        supervisor
            .terminate_attempt("attempt-emergency", Duration::from_secs(10))
            .await
            .unwrap(),
        "supervisor could not verify termination"
    );
    // Independent kernel check right after the supervisor's verified return.
    for pid in &pids {
        assert!(!alive(*pid), "process {pid} survived emergency termination");
    }
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(supervisor.active_ids().await.is_empty());
    let late = supervisor
        .launch("late", &shell(&directory, "sleep 600", "attempt-emergency"))
        .await;
    assert!(late.is_err(), "a fenced attempt launched a new process");
}

/// An answered abort and a closed RPC pipe prove nothing about termination;
/// the process stays active and accounted until the tree is really stopped.
#[tokio::test]
async fn abort_reply_and_closed_rpc_are_not_termination() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().canonicalize().unwrap();
    let supervisor = Supervisor::new();
    let pid = supervisor
        .launch(
            "omp",
            &shell(
                &directory,
                "read request; echo '{\"type\":\"response\",\"command\":\"abort\",\"success\":true}'; \
                 trap '' TERM INT HUP; while :; do sleep 1; done",
                "attempt-abort",
            ),
        )
        .await
        .unwrap() as i32;
    let _guard = GroupGuard(vec![pid]);
    let io = supervisor.take_io("omp").await.unwrap();
    let mut stdin = io.stdin;
    stdin
        .write_all(b"{\"type\":\"abort\",\"id\":\"a\"}\n")
        .await
        .unwrap();
    let mut reply = String::new();
    BufReader::new(io.stdout)
        .read_line(&mut reply)
        .await
        .unwrap();
    assert!(reply.contains("\"success\":true"));
    stdin.shutdown().await.unwrap();
    drop(stdin);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(alive(pid), "fixture must outlive its abort reply");
    assert_eq!(supervisor.active_attempts().await, vec!["attempt-abort"]);
    assert!(
        supervisor
            .terminate_attempt("attempt-abort", Duration::from_secs(10))
            .await
            .unwrap()
    );
    assert!(!alive(pid));
}

fn record(task: &str, attempt: &str, state: TaskState, current: Option<&str>) -> TaskRecord {
    TaskRecord {
        id: task.into(),
        spec: TaskSpec {
            project_id: "project".into(),
            title: "lifecycle".into(),
            instructions: String::new(),
            source_commit: "0".repeat(40),
            execution: TaskExecution::Agent,
            dependencies: Vec::new(),
            required_capabilities: Vec::new(),
            resources: Resources {
                agent_slots: 1,
                cpu_millis: 0,
                memory_mb: 0,
                disk_mb: 0,
            },
            effect_class: EffectClass::Workspace,
            lease_seconds: 60,
            max_runtime_seconds: 600,
            contract_revision: 1,
            policy_revision: 1,
            max_retries: 0,
        },
        state,
        generation: 1,
        parent_task_id: None,
        attempt_id: current.map(str::to_owned).or(Some(attempt.into())),
        integrated_sha: None,
        workflow_result: None,
        blocked_reason: None,
        cancel_requested: true,
    }
}

fn attempt(id: &str, settled: bool) -> AttemptRecord {
    AttemptRecord {
        id: id.into(),
        task_id: "task".into(),
        worker_id: "worker".into(),
        generation: 1,
        state: "fenced".into(),
        lease_expires_at: 0,
        started_at: 0,
        settled,
        pending_children: Vec::new(),
        last_sequence: 0,
        stop_reason: None,
        resources: Resources {
            agent_slots: 1,
            cpu_millis: 0,
            memory_mb: 0,
            disk_mb: 0,
        },
    }
}

/// Terminal cancellation needs all three observations, in order: verified
/// process termination, acknowledged reports, and a resolved coordinator task.
#[test]
fn cancellation_is_terminal_only_after_termination_reports_and_publication_outcome() {
    let ours = attempt("a1", true);
    let cancelled = record("task", "a1", TaskState::Cancelled, None);
    // Even a coordinator-side Cancelled cannot outrun live owned processes or
    // unacknowledged reports.
    assert_eq!(
        local_control::stop_phase(true, false, Some(&ours), Some(&cancelled)),
        StopPhase::Terminating
    );
    assert_eq!(
        local_control::stop_phase(false, true, Some(&ours), Some(&cancelled)),
        StopPhase::ReportingStop
    );
    // An unresolved publication is not a cancellation outcome yet.
    for unresolved in [
        TaskState::Publishing,
        TaskState::Validating,
        TaskState::Executing,
    ] {
        let task = record("task", "a1", unresolved, None);
        let phase = local_control::stop_phase(false, false, Some(&ours), Some(&task));
        assert_eq!(phase, StopPhase::AwaitingCoordinator, "{unresolved:?}");
        assert!(!phase.terminal());
    }
    assert_eq!(
        local_control::stop_phase(false, false, None, None),
        StopPhase::AwaitingCoordinator
    );
    let phase = local_control::stop_phase(false, false, Some(&ours), Some(&cancelled));
    assert!(phase == StopPhase::Cancelled && phase.terminal());
    // Publication won the race: reported as accepted, never as cancelled.
    let accepted = record("task", "a1", TaskState::Accepted, None);
    assert_eq!(
        local_control::stop_phase(false, false, Some(&ours), Some(&accepted)),
        StopPhase::Accepted
    );
    // A fenced attempt reconciled while the task moved to a replacement.
    let moved = record("task", "a1", TaskState::Executing, Some("a2"));
    assert_eq!(
        local_control::stop_phase(false, false, Some(&ours), Some(&moved)),
        StopPhase::Superseded
    );
    assert_eq!(
        local_control::stop_phase(false, false, Some(&attempt("a1", false)), Some(&moved)),
        StopPhase::AwaitingCoordinator
    );
}

#[test]
fn permission_reduction_lists_pending_changes_until_broader_work_drains() {
    let withdrawn = vec!["desktop".to_owned(), "elevated".to_owned()];
    let active = vec![
        ("a-desktop".to_owned(), vec!["desktop".to_owned()]),
        ("a-plain".to_owned(), vec!["rust".to_owned()]),
    ];
    let (pending, broader) = local_control::permission_drain(&withdrawn, &active);
    assert_eq!(pending, vec!["withdraw desktop"]);
    assert_eq!(broader.len(), 1);
    assert_eq!(broader[0].capability, "desktop");
    assert_eq!(broader[0].attempt_id, "a-desktop");
    // Once the broader work drained, nothing is pending.
    let (pending, broader) = local_control::permission_drain(&withdrawn, &active[1..]);
    assert!(pending.is_empty() && broader.is_empty());
    // Refinements of a withdrawn capability are withdrawn too (restrictive).
    assert!(local_control::withdrawn(&withdrawn, "desktop:macos"));
    assert!(!local_control::withdrawn(&withdrawn, "rust"));
    assert!(!local_control::withdrawn(&[String::new()], "rust"));
}

/// The local control surface is the owner's private directory, not a socket:
/// requests create or tighten it to owner-only and refuse links. Withdrawal,
/// restoration and pause persist; a manual start never undoes them.
#[test]
fn local_controls_are_owner_private_and_persistent() {
    use std::os::unix::fs::PermissionsExt;
    let temporary = tempfile::tempdir().unwrap();
    let fresh = temporary.path().join("fresh/worker");
    local_control::request(&fresh, ControlAction::PauseDesktop).unwrap();
    assert_eq!(
        std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let loose = temporary.path().join("loose");
    std::fs::create_dir(&loose).unwrap();
    std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o777)).unwrap();
    local_control::withdraw_capability(&loose, "desktop").unwrap();
    assert_eq!(
        std::fs::metadata(&loose).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let link = temporary.path().join("link");
    std::os::unix::fs::symlink(&loose, &link).unwrap();
    assert!(local_control::request(&link, ControlAction::Drain).is_err());
    assert!(local_control::withdraw_capability(&loose, " ").is_err());
    local_control::withdraw_capability(&loose, "desktop").unwrap();
    local_control::withdraw_capability(&loose, "elevated").unwrap();
    let journal = Journal::open(loose.join("worker.sqlite")).unwrap();
    let controls: OwnerControls = journal.get_value("owner_controls").unwrap().unwrap();
    assert_eq!(controls.withdrawn_capabilities, vec!["desktop", "elevated"]);
    local_control::restore_capability(&loose, "desktop").unwrap();
    let controls: OwnerControls = journal.get_value("owner_controls").unwrap().unwrap();
    assert_eq!(controls.withdrawn_capabilities, vec!["elevated"]);
    // Older journals without the field still deserialize.
    journal
        .set_value(
            "owner_controls",
            &serde_json::json!({"draining":false,"emergency_cancel":false,"desktop_paused":true}),
        )
        .unwrap();
    let controls: OwnerControls = journal.get_value("owner_controls").unwrap().unwrap();
    assert!(controls.desktop_paused && controls.withdrawn_capabilities.is_empty());
}

/// The real worker loop (`runtime::run`) in a re-executed worker process,
/// against an in-process fake coordinator and a scripted fake OMP that never
/// contacts a model provider. Each attempt OMP leads a process group holding a
/// descendant that ignores SIGTERM/SIGINT/SIGHUP and outlives stdin EOF, so
/// only real tree termination stops it.
mod real_loop {
    use super::{GroupGuard, alive};
    use axum::{
        Json, Router,
        body::Bytes,
        extract::{Query, State},
        http::StatusCode,
        routing::{get, post},
    };
    use dacs_protocol::*;
    use dacs_worker::{
        Journal,
        local_control::{self, ControlAction, LocalStatus, OwnerControls, StopPhase},
        runtime::WorkerConfig,
    };
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };
    use serde_json::{Value, json};
    use std::{
        collections::BTreeMap,
        path::{Path, PathBuf},
        process::{Child, Command as Process, ExitStatus, Stdio},
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    const CONFIG_ENV: &str = "DACS_LIFECYCLE_WORKER_CONFIG";
    const WORKER: &str = "worker-1";
    const PROJECT: &str = "project-1";
    /// Must equal the runtime's exact native capability probe.
    const DESKTOP_PROBE: &str =
        "console.log('DACS_DESKTOP_CAPS='+JSON.stringify(await computer.capabilities()));";

    /// Runs only when re-executed by a test below; a plain `--ignored` run returns.
    #[test]
    #[ignore = "worker process for real_loop lifecycle tests"]
    fn lifecycle_worker_process() {
        let Some(path) = std::env::var_os(CONFIG_ENV) else {
            return;
        };
        let config: WorkerConfig = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(dacs_worker::runtime::run(config))
            .unwrap();
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    fn resources() -> Resources {
        Resources {
            agent_slots: 1,
            cpu_millis: 1000,
            memory_mb: 256,
            disk_mb: 256,
        }
    }

    #[derive(Clone, Debug)]
    struct Logged {
        command: Value,
        status: u16,
        /// Liveness of the attempt's recorded OMP tree when a release arrived.
        owned_alive: Vec<bool>,
    }

    impl Logged {
        fn kind(&self) -> &str {
            self.command["type"].as_str().unwrap_or("")
        }
        fn event(&self) -> &str {
            self.command["event"]["type"].as_str().unwrap_or("")
        }
        fn releases(&self) -> Vec<String> {
            if self.kind() == "report_event" && self.event() == "stopped" {
                return vec![self.command["attempt_id"].as_str().unwrap().to_owned()];
            }
            if self.kind() == "reconcile_worker" {
                return self.command["stopped_attempt_ids"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|id| id.as_str().unwrap().to_owned())
                    .collect();
            }
            Vec::new()
        }
    }

    #[derive(Default)]
    struct Fake {
        snapshot: Snapshot,
        messages: Vec<ControlMessage>,
        log: Vec<Logged>,
        bundle: Vec<u8>,
        omp_log: PathBuf,
        /// Models an unresolved publication: the coordinator refuses to release
        /// the reservation (Stopped / ReconcileWorker) until it is reconciled.
        refuse_release: bool,
    }

    type Shared = Arc<Mutex<Fake>>;

    fn trees(omp_log: &Path) -> Vec<(i32, i32, String)> {
        std::fs::read_to_string(omp_log.join("trees"))
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let mut parts = line.split_whitespace();
                (
                    parts.next().unwrap().parse().unwrap(),
                    parts.next().unwrap().parse().unwrap(),
                    parts.next().unwrap().to_owned(),
                )
            })
            .collect()
    }

    impl Fake {
        fn apply(&mut self, command: &Command) -> (StatusCode, Value) {
            let refused = (
                StatusCode::CONFLICT,
                json!({"error":"reconcile Git publication before releasing reservation"}),
            );
            match command {
                Command::ReportEvent {
                    attempt_id,
                    sequence,
                    event,
                    ..
                } => {
                    let stopped = matches!(event, WorkerEvent::Stopped);
                    if stopped && self.refuse_release {
                        return refused;
                    }
                    let snapshot = &mut self.snapshot;
                    let Some(a) = snapshot.attempts.iter_mut().find(|a| a.id == *attempt_id) else {
                        return (StatusCode::NOT_FOUND, json!({"error":"unknown attempt"}));
                    };
                    let t = snapshot
                        .tasks
                        .iter_mut()
                        .find(|t| t.id == a.task_id)
                        .unwrap();
                    if matches!(a.state.as_str(), "fenced" | "abandoned") && !stopped {
                        return (StatusCode::CONFLICT, json!({"error":"fenced attempt"}));
                    }
                    a.last_sequence = *sequence;
                    match event {
                        WorkerEvent::Started | WorkerEvent::Progress { .. } => {
                            a.state = "executing".into()
                        }
                        WorkerEvent::Settled => a.settled = true,
                        WorkerEvent::Failed { .. } => {
                            a.state = "failed".into();
                            t.state = TaskState::Failed;
                        }
                        WorkerEvent::Stopped => {
                            a.settled = true;
                            if !matches!(a.state.as_str(), "fenced" | "failed") {
                                a.state = "stopped".into();
                            }
                            if t.cancel_requested {
                                t.state = TaskState::Cancelled;
                            }
                        }
                        _ => (),
                    }
                }
                Command::ReconcileWorker {
                    stopped_attempt_ids,
                } => {
                    if self.refuse_release {
                        return refused;
                    }
                    for id in stopped_attempt_ids {
                        let snapshot = &mut self.snapshot;
                        if let Some(a) = snapshot.attempts.iter_mut().find(|a| a.id == *id) {
                            a.settled = true;
                            if let Some(t) = snapshot.tasks.iter_mut().find(|t| t.id == a.task_id) {
                                if t.cancel_requested {
                                    t.state = TaskState::Cancelled;
                                }
                            }
                        }
                    }
                }
                Command::SubmitCandidate { submission } => {
                    if let Some(t) = self
                        .snapshot
                        .tasks
                        .iter_mut()
                        .find(|t| t.id == submission.task_id)
                    {
                        t.state = TaskState::Validating;
                    }
                }
                _ => (),
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
            command: serde_json::to_value(&envelope.command).unwrap(),
            status: status.as_u16(),
            owned_alive: Vec::new(),
        };
        // Independent kernel check at the moment a release reaches us.
        for attempt in logged.releases() {
            for (leader, descendant, owner) in trees(&fake.omp_log) {
                if owner == attempt {
                    logged.owned_alive.push(alive(leader));
                    logged.owned_alive.push(alive(descendant));
                }
            }
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
                "user.name=DACS lifecycle fixture",
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

    /// Scripted stand-in for the pinned OMP. Probe launches answer readiness
    /// (and, for desktop, replay recorded native capability frames). Attempt
    /// launches start a signal-ignoring descendant and submit through the
    /// real `dacs_submit` host tool only after the test writes `release-<id>`.
    fn fake_omp(directory: &Path, log: &Path) -> PathBuf {
        let script = r#"#!/bin/sh
LOG='__LOG__'
if [ "$1" = "--version" ]; then echo "omp/18.20.0"; exit 0; fi
mode=attempt
for argument in "$@"; do
  [ "$argument" = "--no-tools" ] && mode=probe
  [ "$argument" = "eval" ] && mode=desktop-probe
done
attempt=$(basename "$PWD")
echo "$$ $mode $attempt" >> "$LOG/launches"
if [ "$mode" = attempt ]; then
  sh -c 'trap "" TERM INT HUP; while :; do sleep 1; done' &
  echo "$$ $! $attempt" >> "$LOG/trees"
  ( while [ ! -e "$LOG/release-$attempt" ]; do sleep 0.1; done
    printf '%s\n' "{\"type\":\"host_tool_call\",\"id\":\"submit-$attempt\",\"toolCallId\":\"submit-$attempt\",\"toolName\":\"dacs_submit\",\"arguments\":{\"evidence\":[]}}" ) &
fi
printf '%s\n' '{"type":"ready","protocolVersion":1}'
awk -v logf="$LOG/frames-$$" -v mode="$mode" -v frames="$LOG/desktop-frames" '
function out(s) { print s; fflush() }
{
  print $0 >> logf; fflush(logf)
  id = ""
  if (match($0, /"id":"[^"]*"/)) id = substr($0, RSTART + 6, RLENGTH - 7)
  if ($0 ~ /"type":"host_tool_result"/) {
    out("{\"type\":\"prompt_result\",\"status\":\"completed\",\"sessionSettled\":true}")
    next
  }
  if (id != "") out("{\"type\":\"response\",\"id\":\"" id "\",\"success\":true,\"data\":{}}")
  if ($0 ~ /"type":"prompt"/ && mode != "attempt") {
    if (mode == "desktop-probe") { while ((getline line < frames) > 0) out(line); close(frames) }
    out("{\"type\":\"prompt_result\",\"id\":\"" id "\",\"status\":\"completed\",\"sessionSettled\":true}")
  }
}'
# An attempt OMP outlives its closed RPC transport, like a busy tool.
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
        omp_log: PathBuf,
        config: PathBuf,
        data_dir: PathBuf,
        commit: String,
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
            let _guard = GroupGuard(trees(&self.omp_log).iter().map(|t| t.0).collect());
        }
    }

    impl Harness {
        fn new(capabilities: &[&str]) -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().canonicalize().unwrap();
            let omp_log = root.join("omp-log");
            std::fs::create_dir_all(&omp_log).unwrap();
            std::fs::create_dir_all(root.join("home")).unwrap();
            let source = root.join("source");
            std::fs::create_dir_all(&source).unwrap();
            git(&source, &["init", "--initial-branch=main"]);
            std::fs::write(source.join("README.md"), "lifecycle fixture\n").unwrap();
            git(&source, &["add", "README.md"]);
            git(&source, &["commit", "-m", "fixture"]);
            let commit = git(&source, &["rev-parse", "HEAD"]);
            let bundle_path = root.join("source.bundle");
            git(
                &source,
                &["bundle", "create", bundle_path.to_str().unwrap(), "--all"],
            );
            let registration = WorkerRegistration {
                name: "lifecycle fixture".into(),
                physical_host_id: format!("host-{}", root.file_name().unwrap().to_string_lossy()),
                os: std::env::consts::OS.into(),
                architecture: std::env::consts::ARCH.into(),
                capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
                capacity: Resources {
                    agent_slots: 4,
                    cpu_millis: 8000,
                    memory_mb: 4096,
                    disk_mb: 4096,
                },
                omp_version: "18.20.0".into(),
                policy_revision: 1,
                owner_consented: true,
                consent_host: "lifecycle-fixture".into(),
            };
            let fake = Fake {
                snapshot: Snapshot {
                    projects: vec![Project {
                        id: PROJECT.into(),
                        name: "lifecycle fixture".into(),
                        target_branch: "main".into(),
                        contract_revision: 1,
                        policy_revision: 1,
                        max_tasks: 8,
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
                        reserved: resources(),
                    }],
                    ..Snapshot::default()
                },
                bundle: std::fs::read(&bundle_path).unwrap(),
                omp_log: omp_log.clone(),
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
                omp_executable: fake_omp(&root, &omp_log),
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
                omp_log,
                config: config_path,
                data_dir,
                commit,
                shared,
                _runtime: runtime,
                workers: Vec::new(),
            }
        }

        /// Native capability frames the fake desktop probe replays.
        fn desktop_observation(&self, capabilities: Value) {
            let start = json!({"type":"tool_execution_start","toolCallId":"probe-1","toolName":"eval",
                "args":{"language":"js","code":DESKTOP_PROBE}});
            let end = json!({"type":"tool_execution_end","toolCallId":"probe-1","toolName":"eval","isError":false,
                "result":{"content":[{"type":"text","text":format!("DACS_DESKTOP_CAPS={capabilities}")}]}});
            std::fs::write(
                self.omp_log.join("desktop-frames"),
                format!("{start}\n{end}\n"),
            )
            .unwrap();
        }

        /// Coordinator admits `task`/`attempt` to this worker and queues the
        /// assignment message.
        fn assign(&self, task: &str, attempt: &str, required: &[&str]) {
            let spec = TaskSpec {
                project_id: PROJECT.into(),
                title: format!("lifecycle {task}"),
                instructions: "Scripted OMP; no provider is contacted.".into(),
                source_commit: self.commit.clone(),
                execution: TaskExecution::Agent,
                dependencies: vec![],
                required_capabilities: required.iter().map(|c| (*c).to_owned()).collect(),
                resources: resources(),
                effect_class: EffectClass::Workspace,
                lease_seconds: 3600,
                max_runtime_seconds: 3600,
                contract_revision: 1,
                policy_revision: 1,
                max_retries: 0,
            };
            let task = TaskRecord {
                id: task.into(),
                spec,
                state: TaskState::Executing,
                generation: 1,
                parent_task_id: None,
                attempt_id: Some(attempt.into()),
                integrated_sha: None,
                workflow_result: None,
                blocked_reason: None,
                cancel_requested: false,
            };
            let attempt = AttemptRecord {
                id: attempt.into(),
                task_id: task.id.clone(),
                worker_id: WORKER.into(),
                generation: 1,
                state: "admitted".into(),
                lease_expires_at: now() + 3600,
                started_at: now(),
                settled: false,
                pending_children: vec![],
                last_sequence: 0,
                stop_reason: None,
                resources: resources(),
            };
            let payload = json!({"task":task,"attempt":attempt});
            self.update(|fake| {
                fake.snapshot.tasks.push(task);
                fake.snapshot.attempts.push(attempt.clone());
            });
            self.message("assignment", Some(&attempt.id), payload);
        }

        fn message(&self, kind: &str, attempt: Option<&str>, payload: Value) -> i64 {
            self.update(|fake| {
                let sequence = fake.messages.len() as i64 + 1;
                fake.messages.push(ControlMessage {
                    sequence,
                    id: format!("message-{sequence}"),
                    worker_id: WORKER.into(),
                    attempt_id: attempt.map(str::to_owned),
                    generation: attempt.map(|_| 1),
                    kind: kind.into(),
                    payload,
                });
                sequence
            })
        }

        fn start_worker(&mut self) {
            let index = self.workers.len();
            let stderr =
                std::fs::File::create(self.root.join(format!("worker-{index}.stderr"))).unwrap();
            let child = Process::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "real_loop::lifecycle_worker_process",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CONFIG_ENV, &self.config)
                // The owner/desktop slot locks live under HOME/.omp.
                .env("HOME", self.root.join("home"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(stderr)
                .spawn()
                .unwrap();
            self.workers.push(child);
        }

        fn signal_worker(&self, signal: Signal) {
            let pid = self.workers.last().unwrap().id() as i32;
            kill(Pid::from_raw(pid), signal).unwrap();
        }

        fn worker_running(&mut self) -> bool {
            self.workers
                .last_mut()
                .unwrap()
                .try_wait()
                .unwrap()
                .is_none()
        }

        fn wait_exit(&mut self) -> ExitStatus {
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                if let Some(status) = self.workers.last_mut().unwrap().try_wait().unwrap() {
                    return status;
                }
                assert!(
                    Instant::now() < deadline,
                    "worker did not exit\n{}",
                    self.worker_log()
                );
                std::thread::sleep(Duration::from_millis(50));
            }
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

        fn wait_for(&self, what: &str, mut ready: impl FnMut() -> bool) {
            let started = Instant::now();
            while !ready() {
                assert!(
                    started.elapsed() < Duration::from_secs(60),
                    "timed out waiting for {what}\nworker:\n{}\ncoordinator log: {:#?}\nstatus: {:#?}",
                    self.worker_log(),
                    self.log()
                        .iter()
                        .map(|l| (l.command.clone(), l.status))
                        .collect::<Vec<_>>(),
                    self.status()
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        fn tree(&self, attempt: &str) -> Option<(i32, i32)> {
            trees(&self.omp_log)
                .into_iter()
                .find(|t| t.2 == attempt)
                .map(|t| (t.0, t.1))
        }

        fn attempt_launches(&self, attempt: &str) -> usize {
            std::fs::read_to_string(self.omp_log.join("launches"))
                .unwrap_or_default()
                .lines()
                .filter(|line| line.ends_with(&format!(" attempt {attempt}")))
                .count()
        }

        fn release(&self, attempt: &str) {
            std::fs::write(self.omp_log.join(format!("release-{attempt}")), b"").unwrap();
        }

        fn log(&self) -> Vec<Logged> {
            self.shared.lock().unwrap().log.clone()
        }

        fn reported(&self, attempt: &str, event: &str) -> bool {
            self.log().iter().any(|l| {
                l.kind() == "report_event"
                    && l.event() == event
                    && l.command["attempt_id"] == attempt
                    && l.status == 200
            })
        }

        /// Every release of `attempt` that reached the coordinator, with the
        /// tree's liveness observed at that moment.
        fn releases(&self, attempt: &str) -> Vec<Logged> {
            self.log()
                .into_iter()
                .filter(|l| l.releases().iter().any(|id| id == attempt))
                .collect()
        }

        fn status(&self) -> Option<LocalStatus> {
            local_control::status(&self.data_dir).ok()
        }

        fn controls(&self) -> OwnerControls {
            Journal::open(self.data_dir.join("worker.sqlite"))
                .unwrap()
                .get_value("owner_controls")
                .unwrap()
                .unwrap_or_default()
        }

        fn update<T>(&self, change: impl FnOnce(&mut Fake) -> T) -> T {
            change(&mut self.shared.lock().unwrap())
        }

        fn running_attempt(&mut self, attempt: &str) -> (i32, i32) {
            self.wait_for("attempt OMP tree and Started", || {
                self.tree(attempt).is_some() && self.reported(attempt, "started")
            });
            self.tree(attempt).unwrap()
        }
    }

    fn assert_released_only_after_termination(harness: &Harness, attempt: &str) {
        let releases = harness.releases(attempt);
        assert!(!releases.is_empty(), "no release reached the coordinator");
        for release in releases {
            assert!(
                !release.owned_alive.is_empty() && release.owned_alive.iter().all(|alive| !alive),
                "release sent while owned tree was alive: {:?}",
                release.command
            );
        }
    }

    /// Ctrl-C, SIGHUP (terminal close) and SIGTERM request a drain: the worker
    /// keeps running, reports draining, leaves the admitted tree alone, and
    /// exits only after that work finished and its stop was reported.
    #[test]
    fn ordinary_close_drains_admitted_work_instead_of_killing_it() {
        let mut harness = Harness::new(&[]);
        harness.assign("task-1", "attempt-1", &[]);
        harness.start_worker();
        let (leader, descendant) = harness.running_attempt("attempt-1");
        harness.signal_worker(Signal::SIGINT);
        harness.wait_for("drain reported locally and to the coordinator", || {
            harness.status().is_some_and(|s| s.draining)
                && harness
                    .log()
                    .iter()
                    .any(|l| l.kind() == "set_draining" && l.command["draining"] == true)
        });
        // Repeated close signals never escalate into a kill.
        for signal in [Signal::SIGINT, Signal::SIGHUP, Signal::SIGTERM] {
            harness.signal_worker(signal);
        }
        std::thread::sleep(Duration::from_millis(1500));
        assert!(harness.worker_running(), "close signal stopped the worker");
        assert!(
            alive(leader) && alive(descendant),
            "drain killed admitted work"
        );
        let status = harness.status().unwrap();
        assert!(status.running && status.draining && !status.emergency_cancel);
        assert_eq!(status.active_attempt_ids, vec!["attempt-1"]);
        assert!(harness.releases("attempt-1").is_empty());
        // The admitted work completes through its real submission path.
        harness.release("attempt-1");
        let exit = harness.wait_exit();
        assert!(
            exit.success(),
            "worker exited with {exit}\n{}",
            harness.worker_log()
        );
        assert!(
            harness
                .log()
                .iter()
                .any(|l| l.kind() == "submit_candidate" && l.status == 200),
            "drained work did not complete its submission"
        );
        assert!(harness.reported("attempt-1", "stopped"));
        assert_released_only_after_termination(&harness, "attempt-1");
        assert!(!alive(leader) && !alive(descendant));
    }

    /// Emergency cancel is distinct from drain: it terminates the owned tree
    /// (including signal-ignoring descendants) and reports the stop only after
    /// the kernel confirms the tree is gone.
    #[test]
    fn emergency_cancel_terminates_owned_trees_unlike_drain() {
        let mut harness = Harness::new(&[]);
        harness.assign("task-1", "attempt-1", &[]);
        harness.start_worker();
        let (leader, descendant) = harness.running_attempt("attempt-1");
        let started = Instant::now();
        local_control::request(&harness.data_dir, ControlAction::EmergencyCancel).unwrap();
        let exit = harness.wait_exit();
        assert!(
            exit.success(),
            "worker exited with {exit}\n{}",
            harness.worker_log()
        );
        assert!(started.elapsed() < Duration::from_secs(30));
        assert!(!alive(leader) && !alive(descendant));
        assert_released_only_after_termination(&harness, "attempt-1");
        assert!(harness.reported("attempt-1", "stopped"));
        assert!(
            !harness.log().iter().any(|l| l.kind() == "submit_candidate"),
            "emergency cancel let the work complete"
        );
        let status = harness.status().unwrap();
        assert!(!status.running && status.requested_controls.emergency_cancel);
        assert!(
            status
                .stops
                .iter()
                .any(|s| s.attempt_id == "attempt-1" && s.reason == "emergency_cancel")
        );
        assert!(
            std::fs::read_dir(harness.data_dir.join("processes"))
                .unwrap()
                .next()
                .is_none(),
            "a process receipt survived emergency cancel"
        );
    }

    /// Coordinator cancellation: the tree is terminated before any release is
    /// sent; while the coordinator holds an unresolved publication the stop is
    /// not terminal; only the coordinator's resolution makes it Cancelled.
    #[test]
    fn coordinator_cancel_is_terminal_only_after_verified_stop_and_publication_outcome() {
        let mut harness = Harness::new(&[]);
        harness.assign("task-1", "attempt-1", &[]);
        harness.start_worker();
        let (leader, descendant) = harness.running_attempt("attempt-1");
        harness.update(|fake| {
            fake.refuse_release = true;
            let task = fake
                .snapshot
                .tasks
                .iter_mut()
                .find(|t| t.id == "task-1")
                .unwrap();
            task.cancel_requested = true;
            let attempt = fake
                .snapshot
                .attempts
                .iter_mut()
                .find(|a| a.id == "attempt-1")
                .unwrap();
            attempt.state = "fenced".into();
            attempt.stop_reason = Some("task cancelled".into());
        });
        harness.message(
            "stop",
            Some("attempt-1"),
            json!({"reason":"task cancelled"}),
        );
        harness.wait_for("tree terminated and release refused", || {
            !alive(leader) && !harness.releases("attempt-1").is_empty()
        });
        assert_released_only_after_termination(&harness, "attempt-1");
        // Unresolved publication: reports stay pending, the stop is not terminal.
        harness.wait_for("stop phase reporting", || {
            harness.status().is_some_and(|s| {
                s.stops
                    .iter()
                    .any(|s| s.attempt_id == "attempt-1" && s.phase == StopPhase::ReportingStop)
            })
        });
        for _ in 0..10 {
            let status = harness.status().unwrap();
            let stop = status
                .stops
                .iter()
                .find(|s| s.attempt_id == "attempt-1")
                .unwrap();
            assert!(
                !stop.phase.terminal(),
                "terminal before coordinator resolution"
            );
            assert!(matches!(
                stop.reason.as_str(),
                "coordinator_stop" | "coordinator_cancel"
            ));
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(!alive(descendant));
        // Cancellation stopped admissions: nothing relaunched for the attempt.
        assert_eq!(harness.attempt_launches("attempt-1"), 1);
        harness.update(|fake| fake.refuse_release = false);
        harness.wait_for("coordinator-resolved cancellation", || {
            harness.status().is_some_and(|s| {
                s.stops
                    .iter()
                    .any(|s| s.attempt_id == "attempt-1" && s.phase == StopPhase::Cancelled)
            })
        });
        assert!(harness.reported("attempt-1", "stopped"));
        assert_eq!(harness.attempt_launches("attempt-1"), 1);
        local_control::request(&harness.data_dir, ControlAction::Drain).unwrap();
        assert!(harness.wait_exit().success());
    }

    /// Permission reduction: the capability leaves the heartbeat at once, the
    /// running attempt using it drains (not killed) while the status shows the
    /// pending change and the still-active broader grant, and an unstarted
    /// assignment needing it is refused without launching anything.
    #[test]
    fn permission_reduction_drains_and_shows_pending_broader_grants() {
        let mut harness = Harness::new(&["browser"]);
        harness.assign("task-1", "attempt-1", &["browser"]);
        harness.start_worker();
        let (leader, descendant) = harness.running_attempt("attempt-1");
        let before = harness.log().len();
        local_control::withdraw_capability(&harness.data_dir, "browser").unwrap();
        harness.wait_for("narrowed heartbeat and pending change", || {
            harness.log()[before..].iter().any(|l| {
                l.kind() == "heartbeat" && l.command["registration"]["capabilities"] == json!([])
            }) && harness
                .status()
                .is_some_and(|s| s.pending_permission_changes == vec!["withdraw browser"])
        });
        let status = harness.status().unwrap();
        assert!(status.effective_capabilities.is_empty());
        assert_eq!(status.active_broader_permissions.len(), 1);
        assert_eq!(status.active_broader_permissions[0].capability, "browser");
        assert_eq!(status.active_broader_permissions[0].attempt_id, "attempt-1");
        assert!(
            alive(leader) && alive(descendant),
            "reduction killed admitted work"
        );
        assert!(harness.releases("attempt-1").is_empty());
        // New use of the withdrawn grant is refused explicitly, never started.
        harness.assign("task-2", "attempt-2", &["browser"]);
        harness.wait_for("unstarted assignment refused", || {
            harness.reported("attempt-2", "stopped")
        });
        assert!(harness.log().iter().any(|l| {
            l.kind() == "report_event"
                && l.event() == "failed"
                && l.command["attempt_id"] == "attempt-2"
                && l.command["event"]["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("withdrew local capability browser"))
        }));
        assert_eq!(harness.attempt_launches("attempt-2"), 0);
        // Heartbeats after the withdrawal never re-widen.
        let narrowed = harness.log()[before..]
            .iter()
            .position(|l| {
                l.kind() == "heartbeat" && l.command["registration"]["capabilities"] == json!([])
            })
            .unwrap()
            + before;
        assert!(harness.log()[narrowed..].iter().all(|l| {
            l.kind() != "heartbeat" || l.command["registration"]["capabilities"] == json!([])
        }));
        // Once the broader work drains, nothing remains pending.
        harness.release("attempt-1");
        harness.wait_for("drained broader grant", || {
            harness.reported("attempt-1", "stopped")
                && harness.status().is_some_and(|s| {
                    s.pending_permission_changes.is_empty()
                        && s.active_broader_permissions.is_empty()
                })
        });
        assert_released_only_after_termination(&harness, "attempt-1");
        local_control::request(&harness.data_dir, ControlAction::Drain).unwrap();
        assert!(harness.wait_exit().success());
    }

    /// Coordinator messages can only narrow local controls (pause); messages
    /// that try to resume the desktop, restore a withdrawn capability or
    /// approve a local control change nothing, and snapshots claiming broader
    /// registration never widen the heartbeat.
    #[test]
    fn coordinator_messages_cannot_approve_or_widen_local_controls() {
        let mut harness = Harness::new(&["browser"]);
        local_control::request(&harness.data_dir, ControlAction::PauseDesktop).unwrap();
        local_control::withdraw_capability(&harness.data_dir, "browser").unwrap();
        harness.update(|fake| {
            let worker = &mut fake.snapshot.workers[0];
            worker.registration.capabilities =
                vec!["browser".into(), "desktop".into(), "elevated".into()];
            worker.draining = false;
        });
        harness.start_worker();
        harness.message("resume_desktop", None, json!({}));
        harness.message("local_control", None, json!({"action":"resume_desktop"}));
        harness.message(
            "local_control",
            None,
            json!({"action":"restore_capability","capability":"browser"}),
        );
        let last = harness.message(
            "approve_local_control",
            None,
            json!({"request":"emergency_cancel","approved":true}),
        );
        harness.wait_for("messages durably received and acknowledged", || {
            harness.log().iter().any(|l| {
                l.kind() == "acknowledge_messages" && l.command["through_sequence"] == last
            })
        });
        std::thread::sleep(Duration::from_millis(500));
        let controls = harness.controls();
        assert!(
            controls.desktop_paused,
            "remote message resumed the desktop"
        );
        assert_eq!(controls.withdrawn_capabilities, vec!["browser"]);
        assert!(!controls.emergency_cancel && !controls.draining);
        assert!(harness.worker_running());
        for heartbeat in harness.log().iter().filter(|l| l.kind() == "heartbeat") {
            assert_eq!(heartbeat.command["registration"]["capabilities"], json!([]));
        }
        // A remote pause is a narrowing and is honoured (coordinator kind).
        local_control::request(&harness.data_dir, ControlAction::ResumeDesktop).unwrap();
        assert!(!harness.controls().desktop_paused);
        harness.message("pause_desktop", None, json!({"requested_at":now()}));
        harness.wait_for("remote pause applied", || harness.controls().desktop_paused);
        harness.wait_for("pause reported to the coordinator", || {
            harness.log().iter().any(|l| {
                l.kind() == "report_runtime_status" && l.command["status"]["desktop_paused"] == true
            })
        });
        local_control::request(&harness.data_dir, ControlAction::Drain).unwrap();
        assert!(harness.wait_exit().success());
    }

    /// One desktop slot per owner session, a persisted activity indicator and
    /// a single local pause that terminates the desktop attempt's tree. The
    /// pause survives a manual restart. Native desktop input is not exercised.
    #[test]
    fn desktop_slot_activity_and_local_pause() {
        let mut harness = Harness::new(&["desktop"]);
        harness.desktop_observation(json!({"capture":true,"input":true,"displayCount":1,
            "screenLocked":false,"displayAsleep":false,"secureInput":false,
            "capturePermission":"granted","inputPermission":"granted"}));
        harness.assign("task-1", "attempt-1", &["desktop"]);
        harness.start_worker();
        let (leader, descendant) = harness.running_attempt("attempt-1");
        harness.wait_for("persisted desktop activity", || {
            harness
                .status()
                .is_some_and(|s| s.desktop_supported && s.desktop_active && !s.desktop_paused)
        });
        assert!(harness.log().iter().any(|l| {
            l.kind() == "report_runtime_status" && l.command["status"]["desktop_active"] == true
        }));
        // A second, unrelated desktop workflow cannot interleave: it never
        // gets an OMP session while the slot is held.
        harness.assign("task-2", "attempt-2", &["desktop"]);
        harness.wait_for("second desktop attempt refused", || {
            harness.reported("attempt-2", "stopped")
        });
        assert!(harness.log().iter().any(|l| {
            l.event() == "failed"
                && l.command["attempt_id"] == "attempt-2"
                && l.command["event"]["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("owns this owner session"))
        }));
        assert_eq!(harness.attempt_launches("attempt-2"), 0);
        assert!(alive(leader) && alive(descendant));
        // The one local pause action stops desktop access now (not a drain).
        local_control::request(&harness.data_dir, ControlAction::PauseDesktop).unwrap();
        harness.wait_for("desktop attempt terminated and paused", || {
            !alive(leader)
                && harness.reported("attempt-1", "stopped")
                && harness
                    .status()
                    .is_some_and(|s| s.desktop_paused && !s.desktop_active)
        });
        assert!(!alive(descendant));
        assert_released_only_after_termination(&harness, "attempt-1");
        local_control::request(&harness.data_dir, ControlAction::Drain).unwrap();
        assert!(harness.wait_exit().success());
        // A manual restart resets drain, never the owner's pause.
        harness.start_worker();
        harness.wait_for("restarted worker reports persistent pause", || {
            harness.status().is_some_and(|s| {
                s.running && !s.draining && s.desktop_paused && s.requested_controls.desktop_paused
            })
        });
        local_control::request(&harness.data_dir, ControlAction::Drain).unwrap();
        assert!(harness.wait_exit().success());
    }
}
