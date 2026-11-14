## Race conditions

Everything below was established by reading; nothing was built or run.

### 1. A refused stop reconciliation is retried under a fixed command id and wedges the outbox permanently
- **Severity:** high (permanent stuck state, narrow trigger). **Confidence:** confirmed by reading, worker and coordinator sides.
- **Location:** `crates/dacs-worker/src/runtime.rs:262-298` (id built at `:274`, `ensure!` at `:288-291`); coordinator dedup at `crates/dacs-core/src/lib.rs:311-338`.
- **Interleaving:**
  1. A `Stopped` event reaches the coordinator while its task is `Publishing`, for example when the worker reconnects after an outage during a publication. `ReportEvent` returns 409 (`engine.rs:421`).
  2. `flush` posts `ReconcileWorker` with `command_id = "{result_id}-reconcile"`. It also returns 409 (`engine.rs:765`), and `Store::execute` caches that error under the id.
  3. The `ensure!` fails and the item stays pending.
  4. On every later flush both commands are re-sent with identical ids and bodies, so the coordinator replays the cached refusals, even after publication has resolved.
- **Impact:** `coordinator.flush().await?` is the first step of every cycle (`:522`), so the worker never heartbeats, polls messages or dispatches again. This survives restarts because the row stays pending in SQLite.
- **Related, suspected:** an engine error mapped to 500 is cached the same way, and the worker retries the same id indefinitely (`:250-252`).
- **Fix:** use a fresh id per reconciliation try, e.g. `format!("{}-reconcile-{}", item.result_id, Uuid::new_v4())`; `ReconcileWorker` is idempotent.

### 2. `Failed` is emitted after a submission the coordinator already accepted
- **Severity:** high (wrong result). **Confidence:** confirmed by reading.
- **Location:** `runtime.rs:586-595` (restart branch), `:1173-1181` with `:955-964` (teardown), `:972-980` (non-atomic finalisation).
- **Interleaving A (restart):**
  1. An external-effect agent attempt (all desktop tasks are external, `scheduling.rs:208`) gets `receipt.ok` for `SubmitCandidate`; the task is `Validating`, the attempt `result_submitted`.
  2. The worker dies before `phase=finished` at `:979`. The window covers the 2 s tick, the OMP teardown, and the gap between the `Stopped` append and the phase write.
  3. On restart, `valid` at `:579` is true and the phase is `"running"`, so `:590` queues `Failed`.
  4. The coordinator accepts it (contiguous sequence, task not terminal) and sets the task to `Failed`. The candidate can no longer be reviewed.
- **Interleaving B (no crash):** after the `break` at `:1100`, a teardown error at `:1175-1180` replaces `Ok(work)` with `Err`, and `:955-964` queues `Failed`. Triggers are a kill timeout, a stderr-log I/O error, or a descendant that left the process group and holds stderr past 10 s.
- **Also affected:** an external attempt that reported `CostCapped` is overwritten to `failed` by the restart branch.
- **Fix:**
  - Before queuing `Failed` at `:590` and `:955`, skip it when `submission:{attempt}` has an ok receipt or the snapshot state is `result_submitted`, `accepted` or `cost_capped`.
  - Write `Stopped` and the phase atomically with `event_checkpoint(..., Stopped, Some((key, json!("finished"))))`.

### 3. The coordinator's desktop-pause message is silently dropped (kind mismatch)
- **Severity:** high. **Confidence:** confirmed by reading. This is deterministic, not timing-dependent.
- **Location:** `runtime.rs:550` matches `"desktop_pause"`; the coordinator emits `"pause_desktop"` (`crates/dacs-core/src/engine.rs:110`; the UI and core tests use the same string).
- **Outcome:** the message is journaled, the cursor advances (`:558`) and it is acknowledged (`:560`), but no branch handles it. `RequestDesktopPause` returns `queued:true` and desktop control continues. The row also stays `received` forever and is re-parsed on every `pending_commands()` scan.
- **Fix:** match `"pause_desktop"` at `:550`.

### 4. `owner_controls` is read-modify-written outside a transaction by several writers
- **Severity:** medium (rare, but loses a safety control). **Confidence:** confirmed by reading.
- **Location:** `crates/dacs-worker/src/local_control.rs:45-55`; `runtime.rs:457-464` (Ctrl-C task), `:470` with `:499-500` (panic path), `:551` (in-process call on a second connection).
- **Interleaving:**
  1. The Ctrl-C task reads `{draining:false, emergency_cancel:false}`.
  2. The CLI `EmergencyCancel` reads the same value and writes `{true, true}`.
  3. The Ctrl-C task writes `{draining:true, emergency_cancel:false}`.
  4. The emergency cancel is lost although the CLI printed `requested:true`.
- **Same shape:** a `PauseDesktop` landing between `:470` and `:500` is overwritten by the stale struct.
- **Fix:** add a `Journal` update helper that does the read and write in one `Immediate` transaction, and use it at all four sites. Storing each flag under its own key also works.

### 5. Runtime status reports "active and paused" during pause teardown, which the coordinator rejects
- **Severity:** medium (usually self-healing). **Confidence:** confirmed by reading.
- **Location:** `runtime.rs:527` sends `desktop_paused: controls.desktop_paused`; the coordinator rejects active+paused at `engine.rs:85-86`.
- **Interleaving:**
  1. The owner pauses while a desktop attempt is running.
  2. On that tick `status.desktop_active` is true (`:514`) and `controls.desktop_paused` is true.
  3. `ReportRuntimeStatus` gets 400, `command()` errors, and the cycle aborts at `:528`.
- **Impact:** no message fetch, acknowledgement, stale-attempt stop or dispatch until the attempt is reaped. If its process tree cannot be reconciled, `desktop_owner` stays live (`:515`) and every cycle aborts permanently.
- **Fix:** send `status.desktop_paused`, which is already computed at `:516` as `paused && !active`.

### 6. The external-restart branch queues `Stopped` without verifying termination when reached in-process
- **Severity:** medium (needs a kill timeout or a panic). **Confidence:** confirmed by reading.
- **Location:** `runtime.rs:588-594`, compared with the verified branch at `:581`.
- **Interleaving:**
  1. An external attempt is stopped by desktop pause, or ends cost-capped.
  2. `terminate_attempt` returns false and `execute_attempt` returns `Err` at `:966-971` without `Stopped`, leaving phase `"running"`.
  3. On the next tick the attempt is still `valid`, has no `stop:` flag and no emergency, so `:590-591` queue `Failed` ("Worker restarted…") and `Stopped`.
  4. The coordinator releases the reservation while the tree is still alive.
- **Also reachable:** after a panic in `execute_attempt`, where cleanup is only the unverified spawn in `AttemptCleanup::drop`.
- **Fix:** add the same `ensure!(supervisor.terminate_attempt(...))` used at `:581` before `:589`.

### 7. One refused event permanently gaps the attempt's sequence; later terminal events are lost
- **Severity:** medium. **Confidence:** mechanism confirmed; the telemetry trigger is confirmed on the coordinator side, but I did not check which workflows write `engine.jsonl`.
- **Location:** `crates/dacs-worker/src/journal.rs:266` allocates at enqueue; `runtime.rs:299-303` consumes a refused event as `ok:false`; the coordinator requires strict contiguity (`engine.rs:412-420`).
- **Sequence:**
  1. `CompilerTelemetry` n is refused, for example once the project holds over 100,000 telemetry records (`builds.rs:340`) or for a non-server workflow (`builds.rs:327`).
  2. The local counter stays at n while the coordinator's `last_sequence` is n-1.
  3. `Settled` and `WorkflowCompleted` are refused as non-contiguous.
  4. Only `Stopped` is rescued, by the reconcile path the comment at `:269-271` describes. The task ends as "worker stopped; explicit result or abandonment required".
- **Fix:** on a 4xx refusal of a `ReportEvent`, renumber the later pending events for that attempt and roll back `sequences.value` in one journal transaction.

### 8. Drain exit ignores cycle failure and journaled-but-undispatched assignments
- **Severity:** medium. **Confidence:** confirmed by reading.
- **Location:** `runtime.rs:616-620`.
- **Interleaving:**
  1. The worker is draining.
  2. The cycle receives assignment X durably (`:539`), advances the cursor (`:558`) and acknowledges it (`:560`).
  3. `GET /api/state` fails at `:561`, so the cycle errors before dispatch.
  4. `running` is empty and the loop breaks; the process exits cleanly with X admitted and never run.
- **Impact:** the reservation is held until the lease expires. An external task then waits for a human.
- **Fix:** break only when the cycle succeeded and `journal.pending_commands()` holds no assignment.

### 9. An upload whose file is missing or changed blocks the outbox forever
- **Severity:** medium (rare trigger, permanent). **Confidence:** mechanism confirmed; a trigger without a crash was not found.
- **Location:** `runtime.rs:225-229`; `write_durable` at `:985-995` does no temp-and-rename and no directory fsync.
- **Sequence:** an upload is queued, then the file disappears or changes (power loss, owner cleanup, quarantine). Every flush returns `Err` at that item, with the same heartbeat consequence as finding 1.
- **Fix:** on `NotFound` or digest mismatch, call `complete_result(id, {"ok":false,…})`. The dependants at `:188` and `:205-217` already handle failed upload receipts.

### 10. The dispatch loop aborts on the first failing command and starves later assignments
- **Severity:** low to medium. **Confidence:** confirmed by reading.
- **Location:** `?` and `ensure!` inside the `for` at `runtime.rs:571-607` (`:572`, `:574-575`, `:581`, `:582`, `:589`).
- **Sequence:** one attempt's tree cannot be killed (`:581`), or its reconcile is refused (`:582`), or its telemetry file is truncated (`:589`). The cycle errors at that command every tick, and assignments ordered after it never start while heartbeats keep their leases alive.
- **Fix:** record the per-command error and `continue`.

### 11. Heartbeat is queued behind the outbox backlog
- **Severity:** low (self-healing). **Confidence:** mechanism confirmed; lease impact depends on `lease_seconds`.
- **Location:** `runtime.rs:173` (`pending_results(256)`), `:307-314`.
- **Sequence:** after an outage leaves more than 511 pending results, `command(Heartbeat)` returns "command still pending". Cycles fail until the backlog drains at 512 items per tick, and each failed cycle adds a stale heartbeat.
- **Fix:** send Heartbeat directly over HTTP, not through the FIFO outbox, or loop `flush` until the outbox is empty.

### 12. `status()` reports `running:false` for a live worker
- **Severity:** low. **Confidence:** confirmed by reading.
- **Location:** `local_control.rs:67` (15 s threshold) against `runtime.rs:361-364` (poll interval up to 30 s) and one status write per tick (`:520`).
- **Outcome:** with a poll interval over 15 s, or a slow cycle of sequential 20 s-timeout requests, the UI shows a worker with active attempts as not running.
- **Fix:** derive the threshold from the poll interval (store it in `LocalStatus`), or refresh `observed_at` from a timer.

## Cleanup candidates

- **`journal.rs:155-162` `acknowledge_result`:** no callers; a repo-wide search finds only the definition. `complete_result` supersedes it.
- **`journal.rs:50` `in_memory`:** used only by the `#[cfg(test)]` module in the same file; can be `#[cfg(test)]`.
- **`journal.rs:168`:** state `'admitted'` in the `IN` list is never written. `mark_command` is only called with `"running"` and `"done"`.
- **`journal.rs:60-64, 110-111`:** `attempts.state` and `attempts.updated_at_ms` are written and never read; only `MAX(generation)` is queried.
- **`journal.rs:164-165`:** the doc comment says restart rehydration, but `pending_commands` is also the per-tick dispatch scan (`runtime.rs:571`) and the 2 s mailbox poll (`:1106`).
- **`runtime.rs:55-61, 167` `Upload.artifact`:** serialised but never read after deserialisation; `endpoint` already encodes it.
- **`runtime.rs:604, 862-866, 1018-1022`:** the desktop-capability predicate is written three times.
- **`runtime.rs:566-568` and `:579`:** the attempt-validity predicate is duplicated in inverse form.
- **`runtime.rs:587`:** `phase == "finished"` does `continue` without `mark_command(done)`, so after a crash between `:979` and `:980` the row is rescanned forever.
- **`runtime.rs:914` and `:931`:** `workflow-transport.json` holds the device credential and is removed only on the success path. Error and stop paths (`:920`, `:925-926`, `:930`) and crashes leave it on disk.
- **`lib.rs:16-18`:**
  - `OmpToolCall`: no references anywhere beyond its definition.
  - `OmpEvent`, `OmpRpc`, `OmpRpcError`, `ProcessIo`: no use outside `crates/dacs-worker/src` (searched `crates`, `examples`, `person-4`, `net`); can be `pub(crate)`.
  - `ProcessState`, `JournalError`, `PendingCommand`, `PendingResult`: likewise, per a search that excluded `supervisor.rs` and `omp.rs` and truncated at 20 hits per file.
- **Not dead code, found in passing:** rows in `results` and `outbound_receipts` are never pruned, at three or more per poll cycle, and `pending_results` filters on the unindexed `state` column.

## Checked and found sound

- **Persist before acknowledge:** `receive` commits (WAL, `synchronous=FULL`, `Immediate` transaction) before the cursor write at `:558` and the remote acknowledgement at `:560`. A crash replays from the old cursor; duplicates return `Ok(false)` and the stop handling is idempotent.
- **Sequence allocation:** counter, outbox row and optional checkpoint are one transaction (`journal.rs:247-297`), so outbox row order equals sequence order. Telemetry-offset and usage checkpoints cannot diverge from their events.
- **Flush concurrency and cancellation:** the transport mutex is taken before the pending list is read. The 500 ms timeout in `host_call` (`:1323`) can cancel mid-request safely because the item stays pending and the coordinator dedups by command id.
- **Stop against launch:** the watchdog sets the local stop before `terminate_attempt`, which fences the attempt in the supervisor, so later launches are refused. This covers the pause check-then-launch gap at `:1032-1050` and the probe.
- **Stop against failure reporting:** the stop flag is set before the kill, so the `!*stop.borrow()` guards at `:948` and `:955` hold whichever `select!` arm wins.
- **Stop for an undispatched attempt:** the `stop:{id}` flag is durable before the in-memory signal, and dispatch checks it at `:578`.
- **Double execution:** `running.contains_key` at `:573` holds until the join handle is reaped.
- **Stale attempts:** events are fenced by the coordinator's `current()`, and the worker stops stale attempts at `:564-569`.
- **`pending-tools` and `submission:` updates:** both writers run in the same task; `staged:` and `submission:` are written atomically (`:1304-1314`); host calls are idempotent by tool-call id.
- **Drain ordering on a successful cycle:** `SetDraining` (`:530`) precedes the message fetch (`:534`), so an assignment admitted before draining is fetched and dispatched before the exit check.
- **Final flush:** attempts are reaped at the top of the tick, then flushed in the cycle and again at `:618`.
- **Locks:** `worker.lock`, the host lock and the desktop lock use `create` plus `try_lock_exclusive` with no exists-check.
- **File checks:** the check-then-open sequences in `write_private`, `secure_directory` and the credential-mode check sit inside the owner-only data directory.
- **Tests in the slice:** they use `tempdir` or in-memory databases, with no fixed ports, paths, environment variables or sleeps.

## Not covered

- `omp.rs` internals: reader-task shutdown, `request_id` duplicate handling, `close`.
- `supervisor.rs` beyond the functions called. One thing seen in passing: `terminate` holds the `children` mutex across a wait of up to 10 s, which stalls `active_attempts()` in the main loop.
- `build_runner::execute_admitted`, including where artifacts and `engine.jsonl` are written and whether they are cleaned up.
- `workspace.rs` beyond `prepare`'s resume branch.
- Coordinator publication timing beyond `publication.rs:90-230`, and Windows `process_wrap` kill and wait behaviour.