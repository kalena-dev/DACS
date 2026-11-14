//! Worker crash with live supervised process trees. A helper process (this test
//! binary re-executed) launches real process groups through the persistent
//! supervisor and is then SIGKILLed, so no destructor or cleanup path runs.
#![cfg(unix)]

use dacs_protocol::WorkerEvent;
use dacs_worker::{Journal, ManagedChild, Supervisor};
use nix::{errno::Errno, sys::signal::kill, unistd::Pid};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const HELPER_DIR: &str = "DACS_CRASH_HELPER_DIR";

fn shell(directory: &Path, script: &str, attempt: &str) -> ManagedChild {
    ManagedChild {
        name: "crash fixture".into(),
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

fn alive(pid: i32) -> bool {
    match kill(Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        Err(error) => panic!("cannot inspect {pid}: {error}"),
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

fn read_pid(directory: &Path, name: &str) -> Option<i32> {
    std::fs::read_to_string(directory.join(name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn wait_for(what: &str, timeout: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Runs only when re-executed by the crash test; a plain `--ignored` run returns.
#[test]
#[ignore = "helper process for worker_crash_with_live_process_trees"]
fn crashing_worker_helper() {
    let Some(directory) = std::env::var_os(HELPER_DIR).map(PathBuf::from) else {
        return;
    };
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let supervisor = Supervisor::persistent(&directory.join("processes")).unwrap();
        // Live leader that waits on a live descendant in its process group.
        supervisor
            .launch(
                "omp-live",
                &shell(
                    &directory,
                    "sleep 600 & echo $! > live-child.pid; echo $$ > live-leader.pid; wait",
                    "attempt-live",
                ),
            )
            .await
            .unwrap();
        // Leader exits at once; its descendant keeps the process group alive.
        supervisor
            .launch(
                "omp-orphaned",
                &shell(
                    &directory,
                    "sleep 600 & echo $! > orphan-child.pid; echo $$ > orphan-leader.pid",
                    "attempt-orphaned",
                ),
            )
            .await
            .unwrap();
        let journal = Journal::open(directory.join("worker.sqlite")).unwrap();
        let id = journal
            .event(
                "task",
                "attempt-live",
                1,
                3,
                WorkerEvent::Progress {
                    message: "unreported before crash".into(),
                },
            )
            .unwrap();
        std::fs::write(directory.join("result.id"), id).unwrap();
        std::fs::write(directory.join("ready"), b"").unwrap();
        // Keep the supervisor (and its KillOnDrop handles) alive until SIGKILL.
        tokio::time::sleep(Duration::from_secs(600)).await;
        drop(supervisor);
    });
}

#[test]
fn worker_crash_with_live_process_trees_is_reconciled_on_restart() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().canonicalize().unwrap();
    let mut helper = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "crashing_worker_helper",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HELPER_DIR, &directory)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let names = [
        "live-leader.pid",
        "live-child.pid",
        "orphan-leader.pid",
        "orphan-child.pid",
    ];
    wait_for("helper launches", Duration::from_secs(60), || {
        directory.join("ready").exists() && names.iter().all(|n| read_pid(&directory, n).is_some())
    });
    // Crash: SIGKILL, so neither KillOnDrop nor any runtime cleanup executes.
    helper.kill().unwrap();
    helper.wait().unwrap();
    let pid = |name| read_pid(&directory, name).unwrap();
    let (live_leader, live_child, orphan_leader, orphan_child) = (
        pid("live-leader.pid"),
        pid("live-child.pid"),
        pid("orphan-leader.pid"),
        pid("orphan-child.pid"),
    );
    // Each shell is its own group leader (pgid == pid).
    let _guard = GroupGuard(vec![live_leader, orphan_leader]);
    // The crash really leaves supervised work running.
    assert!(alive(live_leader) && alive(live_child) && alive(orphan_child));
    wait_for("orphaned leader reaped", Duration::from_secs(10), || {
        !alive(orphan_leader)
    });
    let receipts = directory.join("processes");
    assert_eq!(std::fs::read_dir(&receipts).unwrap().count(), 2);

    // Restart from the same data directory.
    let stopped = tokio::runtime::Runtime::new().unwrap().block_on(async {
        Supervisor::persistent(&receipts)
            .unwrap()
            .reconcile_previous()
            .await
            .unwrap()
    });
    assert_eq!(stopped, vec!["attempt-live", "attempt-orphaned"]);
    // Independent kernel check, not the supervisor's own claim: every member of
    // both trees, including the orphaned descendant, is gone.
    for pid in [live_leader, live_child, orphan_leader, orphan_child] {
        assert!(!alive(pid), "process {pid} survived restart reconciliation");
    }
    assert_eq!(std::fs::read_dir(&receipts).unwrap().count(), 0);

    // The unreported result survived the crash with its original identity.
    let journal = Journal::open(directory.join("worker.sqlite")).unwrap();
    let pending = journal.pending_results(16).unwrap();
    let id = std::fs::read_to_string(directory.join("result.id")).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].result_id, id);
    assert_eq!(pending[0].payload["command_id"], id.as_str());
    assert_eq!(pending[0].sequence, 4);
}

#[test]
fn restart_without_live_processes_clears_receipts_only_after_inspection() {
    let temporary = tempfile::tempdir().unwrap();
    let receipts = temporary.path().join("processes");
    std::fs::create_dir_all(&receipts).unwrap();
    // A process that exited long ago: its group no longer exists.
    let mut child = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    std::fs::write(
        receipts.join("gone.json"),
        format!(r#"{{"id":"gone","reservation_id":"attempt","pid":{pid},"start_time":0}}"#),
    )
    .unwrap();
    let stopped = tokio::runtime::Runtime::new().unwrap().block_on(async {
        Supervisor::persistent(&receipts)
            .unwrap()
            .reconcile_previous()
            .await
            .unwrap()
    });
    assert_eq!(stopped, vec!["attempt"]);
    assert!(!receipts.join("gone.json").exists());
}
