//! Crash/reconnect of the real worker loop (`runtime::run`) with live processes.
//! See `support` for the fake coordinator and fake OMP.
#![cfg(unix)]

mod support;

use dacs_protocol::*;
use serde_json::json;
use support::*;

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
