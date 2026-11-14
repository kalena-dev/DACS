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
