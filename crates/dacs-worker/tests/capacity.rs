//! Capacity accounting against the physical host: every live process of an
//! attempt is attributed to its reservation, and a worker cannot declare more
//! than the machine it runs on.
#![cfg(unix)]

mod support;

use dacs_protocol::{EffectClass, Resources, WorkerRegistration};
use dacs_worker::{ManagedChild, Supervisor, capacity, runtime::WorkerConfig};
use nix::{errno::Errno, sys::signal::kill, unistd::Pid};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn shell(directory: &Path, script: &str, reservation: Option<&str>) -> ManagedChild {
    ManagedChild {
        name: "capacity fixture".into(),
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), script.into()],
        cwd: directory.to_owned(),
        env: std::env::vars()
            .filter(|(name, _)| matches!(name.as_str(), "PATH" | "HOME" | "TMPDIR"))
            .collect::<BTreeMap<_, _>>(),
        effect_class: "workspace".into(),
        reservation_id: reservation.map(str::to_owned),
    }
}

fn alive(pid: u32) -> bool {
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        Err(error) => panic!("cannot inspect {pid}: {error}"),
    }
}

fn pids(directory: &Path, names: &[&str]) -> Vec<u32> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let read: Option<Vec<u32>> = names
            .iter()
            .map(|name| {
                std::fs::read_to_string(directory.join(name))
                    .ok()?
                    .trim()
                    .parse()
                    .ok()
            })
            .collect();
        if let Some(read) = read {
            return read;
        }
        assert!(Instant::now() < deadline, "fixture pids not written");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[tokio::test]
async fn every_process_of_an_attempt_tree_counts_against_its_reservation() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path();
    let supervisor = Supervisor::new();
    // OMP whose tool runs a nested command, as a bash tool would.
    supervisor
        .launch(
            "omp",
            &shell(
                directory,
                "sh -c 'sleep 600 & echo $! > tool-child.pid; echo $$ > tool.pid; wait' & \
                 echo $$ > omp.pid; wait",
                Some("attempt-a"),
            ),
        )
        .await
        .unwrap();
    // Build/test command with a child (e.g. cargo spawning rustc).
    supervisor
        .launch(
            "build",
            &shell(
                directory,
                "sleep 600 & echo $! > rustc.pid; echo $$ > build.pid; wait",
                Some("attempt-a"),
            ),
        )
        .await
        .unwrap();
    // Compiler service allocated to the same attempt.
    supervisor
        .launch(
            "compiler",
            &shell(
                directory,
                "echo $$ > compiler.pid; exec sleep 600",
                Some("attempt-a"),
            ),
        )
        .await
        .unwrap();
    supervisor
        .launch(
            "other",
            &shell(
                directory,
                "echo $$ > other.pid; exec sleep 600",
                Some("attempt-b"),
            ),
        )
        .await
        .unwrap();
    supervisor
        .launch(
            "probe",
            &shell(directory, "echo $$ > probe.pid; exec sleep 600", None),
        )
        .await
        .unwrap();
    let attempt_a = pids(
        directory,
        &[
            "omp.pid",
            "tool.pid",
            "tool-child.pid",
            "build.pid",
            "rustc.pid",
            "compiler.pid",
        ],
    );
    let other = pids(directory, &["other.pid"]);
    let probe = pids(directory, &["probe.pid"]);

    let usage = supervisor.usage().await;
    let a = &usage[&Some("attempt-a".to_owned())];
    for pid in &attempt_a {
        assert!(
            a.processes.contains(pid),
            "{pid} not attributed to attempt-a: {usage:?}"
        );
    }
    assert!(!a.processes.contains(&other[0]) && !a.processes.contains(&probe[0]));
    assert!(a.memory_bytes > 0);
    assert_eq!(usage[&Some("attempt-b".to_owned())].processes, other);
    assert_eq!(usage[&None].processes, probe);

    // Releasing attempt-a's capacity means its whole tree is gone, verified.
    assert!(
        supervisor
            .terminate_attempt("attempt-a", Duration::from_secs(10))
            .await
            .unwrap()
    );
    for pid in &attempt_a {
        assert!(!alive(*pid), "{pid} outlived its reservation");
    }
    let usage = supervisor.usage().await;
    assert!(!usage.contains_key(&Some("attempt-a".to_owned())));
    assert!(alive(other[0]) && alive(probe[0]));
    for attempt in ["attempt-b"] {
        assert!(
            supervisor
                .terminate_attempt(attempt, Duration::from_secs(10))
                .await
                .unwrap()
        );
    }
    assert!(
        supervisor
            .terminate("probe", Duration::from_secs(10))
            .await
            .unwrap()
    );
    assert!(supervisor.usage().await.is_empty());
}

#[tokio::test]
async fn worker_refuses_to_declare_more_than_the_physical_host() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let host = capacity::observe(&root).unwrap();
    // The OMP stand-in records any execution; refusal must precede all launches.
    let omp = root.join("omp");
    std::fs::write(
        &omp,
        format!("#!/bin/sh\ntouch '{}'\n", root.join("launched").display()),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o755)).unwrap();
    let credential = root.join("token");
    std::fs::write(&credential, "token").unwrap();
    std::fs::set_permissions(&credential, std::fs::Permissions::from_mode(0o600)).unwrap();
    let config = WorkerConfig {
        coordinator_url: "http://127.0.0.1:9".into(),
        credential_file: credential,
        worker_id: "worker".into(),
        registration: WorkerRegistration {
            name: "capacity".into(),
            physical_host_id: "capacity-host".into(),
            os: std::env::consts::OS.into(),
            architecture: std::env::consts::ARCH.into(),
            capabilities: vec![],
            capacity: Resources {
                agent_slots: 1,
                cpu_millis: (host.logical_cpus + 1) * 1000,
                memory_mb: 1,
                disk_mb: 1,
            },
            omp_version: "18.20.0".into(),
            policy_revision: 1,
            owner_consented: true,
            consent_host: "capacity".into(),
        },
        omp_executable: omp,
        omp_version: "18.20.0".into(),
        omp_profile: None,
        data_dir: root.join("data"),
        workspaces_dir: root.join("workspaces"),
        ca_certificate: None,
        poll_interval_ms: 100,
    };
    let error = dacs_worker::runtime::run(config).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("exceeds the observed physical host"),
        "{error:#}"
    );
    assert!(!root.join("launched").exists());
}

/// Real worker loop: an assignment that would exceed the declaration beside a
/// running attempt is held unstarted, and starts only once the running
/// attempt's whole tree is verifiably gone.
#[test]
fn assignment_beyond_declared_capacity_waits_for_running_work_to_release() {
    let mut harness = support::Harness::new(EffectClass::Workspace);
    // Declared: 2 agent slots, 2000 cpu_millis, 1024 MB memory and disk.
    // attempt-1 holds 1000 cpu_millis; attempt-2 needs 1500.
    harness.add_assignment(
        "task-2",
        "attempt-2",
        Resources {
            agent_slots: 1,
            cpu_millis: 1500,
            memory_mb: 256,
            disk_mb: 256,
        },
    );
    harness.start_worker();
    harness.wait_for("first attempt running", || {
        !harness.trees().is_empty() && harness.accepted_for(support::ATTEMPT, "started")
    });
    // Twenty 100 ms worker cycles later the oversubscribing assignment has
    // neither launched a process nor reported anything.
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(harness.trees().len(), 1, "{}", harness.worker_log());
    assert!(!harness.reported("attempt-2"));

    let (leader, descendant) = harness.trees()[0];
    std::fs::write(
        harness.log.join("old-pids"),
        format!("{leader} {descendant}\n"),
    )
    .unwrap();
    harness.update(|fake| fake.watch = vec![leader, descendant]);
    harness.fence(support::ATTEMPT);
    harness.wait_for("second attempt started after the first released", || {
        harness.trees().len() == 2 && harness.accepted_for("attempt-2", "started")
    });
    let release = harness
        .log()
        .into_iter()
        .find(|l| l.releases(support::ATTEMPT))
        .expect("first attempt released");
    assert_eq!(release.watched_alive, vec![false, false]);
    // The second OMP checked the first tree's pids itself at launch.
    assert!(harness.lines("overlap").is_empty());

    harness.fence("attempt-2");
    harness.wait_for("second attempt released", || {
        harness.log().iter().any(|l| l.releases("attempt-2"))
    });
    harness.drain_and_wait();
    assert_eq!(harness.receipts(), 0);
}
