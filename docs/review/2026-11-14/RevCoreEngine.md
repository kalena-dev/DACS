## Race conditions

### 1. A stop report that lands during a publication window is refused forever, wedging the worker
- **Severity:** high. **Confidence:** confirmed by reading (coordinator and worker outbox).
- **Location:** `crates/dacs-core/src/lib.rs:345-367`, `crates/dacs-core/src/engine.rs:421-425`, `crates/dacs-core/src/engine.rs:764-769`.
- **Interleaving:**
  1. `prepare_publication` commits and the task is `Publishing`. This lasts milliseconds normally, but until restart after a crash and indefinitely in `reconciling`.
  2. A worker delivers `ReportEvent{Stopped}` for any attempt of that task, including an older fenced or abandoned one. `engine.rs:421` returns Conflict, and `lib.rs:363` stores that Conflict as the permanent replay result for the `command_id`.
  3. The worker falls back to `ReconcileWorker` with the deterministic id `{result_id}-reconcile` (`dacs-worker/src/runtime.rs:262-285`). `engine.rs:764` returns Conflict, which is also stored.
  4. Publication resolves. The worker retries both commands with the same ids, and `lib.rs:311-341` replays the stored Conflicts every time.
- **Impact:**
  - The attempt's reservation is never released by the worker.
  - `flush()` aborts at that head-of-line item every cycle, so nothing after it is delivered, including heartbeats (`runtime.rs:522-523`). All of that worker's leases then expire.
  - An administrator `ReconcileWorker` frees the reservation but does not unwedge the outbox.
- **Fix:** let `Stopped` and `ReconcileWorker` record the stop while `Publishing`. `reconcile_stop` does not change a `result_submitted` attempt's state, and `finalize_cancellation` already skips `Publishing`. Alternatively, do not write a `requests` row for these two retryable refusals.
- **Tests:** `failed_commands_are_deduplicated_and_effects_rolled_back` covers only a capacity refusal; no test sends a stop during `Publishing`.

### 2. Leases keep expiring while the coordinator is down, and expiry cannot be undone
- **Severity:** high. **Confidence:** confirmed by reading.
- **Location:** `crates/dacs-core/src/scheduling.rs:678-716`, `crates/dacs-core/src/engine.rs:337`, `crates/dacs-core/src/lib.rs:44-49`.
- **Interleaving:**
  1. The coordinator stops at T0 with live attempts whose leases end at T0+L.
  2. It restarts after T0+L. The maintenance loop's first tick fires immediately (`dacs-app/src/server.rs:1187-1192`).
  3. `maintain` sees `lease_expires_at <= now()` and fences every Workspace attempt.
  4. Heartbeats arriving after restart cannot help, because renewal requires `a.lease_expires_at > now()`.
- **Impact:**
  - Healthy Workspace attempts are stopped. With the default `max_retries = 0` (`dacs-protocol/src/lib.rs:122-123`) the task goes `Failed`, and `refresh_dependencies` cascades `Failed` to its dependants.
  - External attempts become permanently "effectful lease expired" and need a stop or abandonment.
  - A forward wall-clock step has the same effect, since `now()` is `SystemTime`.
- **Fix:** persist a last-maintenance timestamp and, on the first `maintain` after a gap, extend unfenced leases by the gap. Alternatively, skip fencing until one lease interval after process start.
- **Tests:** none restart with a running attempt past its lease.

### 3. An External workflow whose lease expired can never record its completion
- **Severity:** high. **Confidence:** confirmed by reading.
- **Location:** `crates/dacs-core/src/engine.rs:426-435` versus `crates/dacs-core/src/engine.rs:503-513`; `crates/dacs-core/src/scheduling.rs:91-104`.
- **Interleaving:**
  1. A build workflow (always External) misses one lease window; `maintain` sets "reconcile original completion or explicitly abandon" (`scheduling.rs:687-692`).
  2. The build finishes and the worker sends `Settled`, `WorkflowCompleted`, then `Stopped` (`runtime.rs:945-978`).
  3. The blanket guard at `engine.rs:426-435` rejects `Settled` and `WorkflowCompleted` with "attempt lease expired". The `result_eligible` call at `:509`, which handles exactly this case, is never reached.
  4. Only `Stopped` is accepted.
- **Impact:** the task stays `Executing` with no way to record the result. The only exit is `AbandonAttempt`, which re-runs external effects or fails the task. The agent path does not have this problem because `SubmitCandidate` has no blanket guard.
- **Fix:** exempt `Settled` and `WorkflowCompleted` from the guard at `:426` and let `:509` decide. The expired path also needs the stop record, so the completion must be accepted after `Stopped` (worker order, or coordinator-side hold).
- **Tests:** `expired_original_external_completion_is_accepted_only_after_explicit_reconciliation` covers the candidate path only.

### 4. A submitted candidate is still lease-fenced while it waits for review or publication
- **Severity:** medium. **Confidence:** confirmed by reading.
- **Location:** `crates/dacs-core/src/engine.rs:331-341`, `crates/dacs-core/src/scheduling.rs:672-716`, `crates/dacs-core/src/publication.rs:197-203`.
- **Interleavings:**
  - **Publishing is not renewed.** Heartbeats renew only `Executing | Validating`. If publication outlasts the lease (crash or `reconciling`) and resolves `not_published`, the task returns to `Validating` with an expired lease. The next `maintain` fences it.
  - **Review time counts against runtime.** Renewal is capped at `started_at + max_runtime_seconds` (`engine.rs:339-340`). A Workspace candidate still under review at that instant is fenced.
- **Impact:** the delivered, reviewable result is discarded. The task goes to `Ready` or `Failed`, and with default retries its dependants cascade to `Failed`.
- **Fix:** in `maintain`, skip attempts in `result_submitted` (or re-arm the lease on `not_published`), and include `Publishing` in heartbeat renewal.
- **Related, intent unclear:** the same fencing happens if the submitting worker simply goes offline during review. I did not count that as a defect.

### 5. `Failed` and `CostCapped` are accepted after a candidate was submitted
- **Severity:** medium. **Confidence:** confirmed by reading.
- **Location:** `crates/dacs-core/src/engine.rs:544-557`.
- **Interleaving:**
  1. `SubmitCandidate` succeeds; the task is `Validating` and the attempt is `result_submitted`.
  2. The worker restarts before its local phase is `finished`.
  3. For External tasks the restart path emits `Failed` then `Stopped` (`runtime.rs:588-593`).
  4. `engine.rs:551` has no state precondition, so the task becomes terminal `Failed`.
- **Impact:** the candidate under review is lost and dependants fail. `CostCapped` behaves the same way: it overwrites `result_submitted`, so `prepare_publication` refuses permanently (`publication.rs:86`).
- **Fix:** require `t.state == Executing` in both branches.

### 6. One refused event permanently breaks the attempt's sequence
- **Severity:** medium. **Confidence:** confirmed by reading.
- **Location:** `crates/dacs-core/src/engine.rs:412-420` with the rollback at `crates/dacs-core/src/lib.rs:345-348`.
- **Interleaving:**
  1. The worker allocates sequence N durably at enqueue (`dacs-worker/src/journal.rs` `event_checkpoint`).
  2. The coordinator refuses event N for any reason: expired lease, the `Publishing` window, a telemetry bound, an invalid usage value.
  3. `last_sequence` stays at N-1 because the refusal is rolled back; the worker marks N as done and moves on.
  4. N+1 onwards are refused as non-contiguous.
- **Impact:** a later `Settled` or `WorkflowCompleted` is dropped and the workflow result is lost. Only `Stopped` survives, through the `ReconcileWorker` fallback.
- **Fix:** consume the sequence number for semantically refused events (advance `last_sequence` outside the rolled-back effects).

### 7. Pool acquisition accepts a service attempt that already failed or was abandoned
- **Severity:** low. **Confidence:** confirmed by reading.
- **Location:** `crates/dacs-core/src/builds.rs:59-68`.
- **Interleaving:**
  1. Service S reports `Failed`, or an administrator abandons it. Its reservation is held until stop, `settled` is still false, and the task generation is unchanged.
  2. Build B is admitted before S's `Stopped` arrives.
  3. The check passes because it tests only generation, lease, `settled` and `cancel_requested`.
- **Impact:** B owns a pool with a dead member.
- **Fix:** also require `service_task.attempt_id == Some(service.id)`, task state `Executing`, and attempt state `admitted` or `executing`.

### 8. `enroll` is not idempotent
- **Severity:** low. **Confidence:** confirmed by reading.
- **Location:** `crates/dacs-core/src/lib.rs:495-542`.
- **Crash point:** the transaction commits the worker and credential, then the response is lost. The token is unrecoverable, and a retry creates a second pending worker because the invitation is not consumed.
- **Fix:** key enrollment on a client-supplied id, or consume the invitation per physical host.

## Cleanup candidates

- **`engine.rs:48-52`, `publication.rs:111-113`, `:247`, `:252-254`:** four copies of the "pending or reconciling publication for this project" predicate. Pure reads; one helper would be safe.
- **`scheduling.rs:236`, `:341`, `publication.rs:101`:** the same `deferred` EXISTS query three times.
- **`scheduling.rs:136`, `:175`, `builds.rs:74`, `:268`:** the same reservation-held EXISTS query four times.
- **`scheduling.rs:99`, `builds.rs:189`:** the same `execution_stopped` EXISTS query twice.
- **`engine.rs:66-70`, `:514-517`, `builds.rs:370-371`:** 64-hex digest validation written three times. The `builds.rs` copy accepts uppercase and the others reject it.
- **`scheduling.rs:56-60`:** `reserved(db, column: &str, …)` treats any string other than `"worker"` as physical host. All four callers (`:86`, `:427`, `:452`, `:494`) pass literals, so a two-variant enum is safe.
- **`builds.rs:168`:** the comment says "Build completion requests service drain". The only caller is `reconcile_stop` (`scheduling.rs:112`), so drain happens on the holder's confirmed stop, not on `WorkflowCompleted`.
- **`engine.rs:509`:** the expired-lease branch of this `result_eligible` call is unreachable because of `:426-435` (see finding 3).
- **`dacs-protocol/src/lib.rs:173`, `:237`:** `AttemptRecord.state` and `Publication.state` are free strings matched by literal across four core files. The worker already matches `"stopping"` (`runtime.rs:567`, `:579`), which no coordinator code writes (grep of `crates/`).
- **`dacs-core/src/lib.rs:1`:** `pub mod builds` is the only public sibling module. Its free functions are `pub(crate)` and the rest are `Store` methods. Grep found no `dacs_core::builds` path under `crates/`, so it can be private.

## Checked and found sound

- **Transaction scope.** One `Connection` behind a `Mutex`. `execute`, `enroll`, `stage_candidate`, `prepare_publication`, `finish_publication` and `maintain` each run in a single transaction. The only non-test `Store::open` is `server.rs:1081`, so deferred transactions never meet a second writer.
- **Replay.** The digest covers the whole envelope; a mismatch is a Conflict. Replay returns before any effect. Failed commands roll back through the savepoint, and the dedup row commits with the effects.
- **Cancel and invalidation versus publication.** `prepare_publication` rejects a cancelled or fenced attempt through `sched::current` in its transaction. `cancel`, `SetRequirements` and `InvalidateContract` defer when a publication is unresolved. `finish_publication` applies the deferred cancel and invalidation in the same transaction as the outcome. Tests cover all three.
- **Expected head and crash reconciliation.** `dacs-source` re-checks head against `expected_head` under the project Git lock before creating the intent. There is one unresolved intent per project. Reconciliation only observes the ref and runs in `SourceService::new` before listeners start.
- **Unresolved intents.** While `Publishing`, admission, abandonment, events, delegation, lease fencing and stop reconciliation are all refused or skipped.
- **Dependants.** `Accepted` and `integrated_sha` are written in the same put (`publication.rs:184-195`) before `refresh_dependencies` (`:236`). `create_task` computes readiness the same way.
- **Generation fencing.** `current()` checks attempt generation, task generation, current attempt id, fenced or abandoned, terminal, and cancel-requested. `SubmitCandidate`, staging and publication all re-check it. Stale attempts can only deliver `Stopped` through `owned_attempt`.
- **Lease boundary.** Heartbeat, `maintain`, `result_eligible` and `ReportEvent` use the same `> now` / `<= now` split, so there is no instant where one renews and another fences.
- **Reservations.** One row per attempt, deleted only by `release` from `reconcile_stop`. A failed admission after the insert is rolled back. Physical-host totals span workers and projects. Parent-then-child and child-then-parent stop orders both finalize cancellation.
- **Stale in-memory records.** `ReportEvent{Stopped}` and `cancel` re-read the task after nested calls. Loops over stale snapshots (`invalidate`, `RevokeWorker`, `cancel`'s children) can rewrite a just-finalized task, but `finalize_cancellation` runs immediately after and converges; the only side effect is a duplicate "stop" message.
- **`refresh_dependencies` ordering.** It calls `notify_parent` (`scheduling.rs:593`) before the put (`:603`). This is unreachable today because `Waiting` tasks never have parents.

## Not covered

- **`crates/dacs-core/src/network.rs`:** outside the slice, not read.
- **`crates/dacs-core/src/tests.rs`:** all test names were scanned, but only six test bodies were read to confirm coverage.
- **`dacs-worker`, `dacs-source`, `dacs-app/src/server.rs`:** read only where needed to follow a call for findings 1, 2, 3, 5 and 6.
- **Dynamic confirmation:** nothing was executed (read-only assignment); every finding is from reading.
- **Non-state-machine commands in `engine.rs`** (invitations, desktop pause, `SendMessage`, `AcknowledgeMessages`, runtime status): read once, with no interleaving analysis beyond the single-transaction check.