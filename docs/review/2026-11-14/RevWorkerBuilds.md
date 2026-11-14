## Race conditions

**1. Exited workflow children stay unreconciled until the attempt ends; the late group kill can hit a recycled PGID**
- Severity: medium (needs PID reuse within one attempt; not self-healing when it happens).
- Confidence: confirmed by reading; the collision itself depends on host PID churn. Unix only for the signal, all platforms for the stale receipts.
- Locations: `crates/dacs-worker/src/build_runner.rs:318-330` and `:476-478`; mechanism in `crates/dacs-worker/src/supervisor.rs:312-348` and process-wrap 10.0.1 `src/tokio/process_group.rs`.
- Interleaving:
  - `command()` sees `Exited` at `:319-320`. That poll goes through `ProcessGroupChild::try_wait`, whose `waitpid(-pgid, WNOHANG)` reaps the leader, so the PID/PGID is free again.
  - `command()` never calls `supervisor.terminate(id)` on a natural exit, so the entry stays `reconciled == false` and its receipt stays on disk.
  - The cargo build then runs for the rest of `max_runtime_seconds`, spawning thousands of processes. PID p is reissued to an unrelated group leader; every other supervised child is one (`supervisor.rs:245`).
  - At `:476-478`, `terminate_attempt` walks every entry of the attempt. `terminate` (`supervisor.rs:322`) calls `start_kill`, which is an unconditional `killpg(pgid, SIGKILL)` with no exit check.
- Impact:
  - Same uid: an unrelated process group, such as another attempt's build or agent, is SIGKILLed and this attempt reports `processes_reconciled = true`.
  - Different uid: `EPERM` propagates through `:478`, no `result.json` is written, and the runtime's own `terminate_attempt` (`runtime.rs:967`) fails the same way, so the attempt never reaches `Stopped`.
  - Worker crash mid-build: the stale receipts hit the "PID reused" bail (`supervisor.rs:168-171`) whenever one of those PIDs is live again, which blocks worker start. Windows recycles PIDs quickly.
- Related, possibly deliberate: with no kill after leader exit, a descendant holding stdout/stderr makes `:332-340` wait for the whole remaining deadline and then fail a run whose leader exited 0.
- Fix: in `command()`, call `supervisor.terminate(id, Duration::from_secs(5))` right after the exit loop, before joining the drains, and require `true`. `workspace::capture` already does this at `workspace.rs:122`. The root cause (`terminate` signalling a group it already reaped) is in the supervisor slice.

**2. Private sccache port: probe, launch and readiness can attach to a listener this attempt does not own**
- Severity: medium (narrow window, integrity impact).
- Confidence: confirmed by reading; sccache's exit-on-bind-failure is assumed, not verified against the pinned source.
- Locations: `build_runner.rs:788-791`, `:810-811`, `:823-841`, `:1029-1033`, `:1596-1597`.
- Interleaving:
  - The probe listener is dropped at `:810` and the daemon is launched at `:811`.
  - Another local process binds `127.0.0.1:port` before our daemon does. The `port-N` lock only covers attempts sharing one `data_dir`, so a second worker instance or any other sccache qualifies.
  - The first readiness pass sees our daemon still `Running` (`:825`, it has not tried to bind yet) and `TcpStream::connect` succeeds against the foreign listener (`:828`), so the loop breaks.
  - Our daemon then exits on bind failure. Nothing checks it again: `terminate` at `:1031` returns `true` for an already-exited child.
- Impact: cargo runs with `SCCACHE_SERVER_PORT` pointing at a daemon the attempt does not own. This contradicts the comment at `:787` and the "unchanged owner sccache daemons" gate. A non-`required` run can report success with objects from a foreign cache, while `cold_cache: true` is hard-coded at `:1597`. A `required` run most likely fails closed because `client-engine.jsonl` is empty; I did not trace the audit to the end.
- The same gap means a daemon crash mid-build goes unnoticed.
- Fix: before `:1031`, require `supervisor.state(&id)` to still be `Running` and fail with "private daemon exited before the build finished". A daemon that lost the bind has exited by then.

**3. Compiler-pool, port and service locks are released even when the processes they guard were not reaped**
- Severity: low.
- Confidence: confirmed by reading.
- Location: `build_runner.rs:476-479`.
- Interleaving: `terminate_attempt` returns `false` (daemon or service outlives the 10 s grace) or errors through `?`. `drop(locks)` at `:479`, or scope exit, unlocks `pool`, `port-N` or `service-…` while the process still runs and still holds the port.
- Impact: a second attempt can pass `acquire_lock` (`:720`, `:773-777`). For builds it then fails at the probe with a misleading "port occupied"; for services there is no probe, so a second scheduler or server for the same pool starts beside the live one. The plan says reservations stay held until stop reconciliation.
- Guard checked: the runtime keeps the coordinator reservation (`runtime.rs:932`, `:967`), so this is only the local second line of defence.
- Fix: release only when `processes_reconciled` is true; otherwise keep the handles alive for the worker's lifetime.

**4. Daemon drain join has no timeout and ignores the deadline**
- Severity: low.
- Confidence: suspected; I could not show that the pinned daemon can leave a pipe holder outside its process group.
- Location: `build_runner.rs:1035-1036`.
- Interleaving: `terminate` at `:1031` certifies the group gone, but a holder of the daemon's stdout/stderr outside that group keeps the pipes open, so `out.await` / `err.await` never complete.
- Impact: `run` hangs past `deadline`, which only `command()` enforces. Only the runtime's stop path (30 s timeout, `runtime.rs:926`) recovers it.
- Fix: wrap the two joins in the same `tokio::time::timeout` that `command()` uses at `:332-340`.

**5. Task-board handlers run blocking redb transactions on async worker threads**
- Severity: low (latency only, self-healing).
- Confidence: confirmed by reading.
- Location: `crates/dacs-worker/src/task_board.rs:225-257` calling `:98`, `:169`, `:183`, `:205`.
- Interleaving: N concurrent mutations each block in `begin_write` behind the single writer, whose commit fsyncs. With as many in-flight writes as runtime workers (`#[tokio::main]` multi-thread in `dacs-app/src/main.rs:428`), every worker thread is parked.
- Impact: `list`, accept and graceful shutdown stall until the queue drains. There is no deadlock, because no handler awaits while holding a transaction.
- Fix: run the board calls in `tokio::task::spawn_blocking`, as `dacs-app/src/main.rs:408`, `:441`, `:522` already do for other blocking work.

**6. Test uses a 5 s wall-clock deadline that includes process launch**
- Severity: low.
- Confidence: suspected flake, not reproduced.
- Location: `build_runner.rs:1199`.
- Interleaving: the deadline is taken before `command()` runs `launch`, which calls `sysinfo::System::new_all()` (`supervisor.rs:257`) and spawns `cmd.exe`. Under a loaded parallel test run this can exceed 5 s; the child is then killed and `code` is `None`, so `assert_eq!(observed.code, Some(7))` fails.
- Fix: use a generous deadline such as 60 s; the test asserts the exit code, not timing.

## Cleanup candidates

Verified with a repo-wide grep that included ignored files and excluded `target/`; each name matches only its own definition.

- `crates/dacs-worker/src/build_execution.rs:97` — `validate_profile_ports` has no callers.
- `build_execution.rs:343` — `verify_toolchain` has no callers. It is the only user of `std::process::Command` (`:15`) and spawns unsupervised children, contradicting the module doc at `:3-4`. The supervised pin checks in `build_runner.rs` replace it.
- `build_execution.rs:368` — `cargo_build_descriptor` has no callers; the shard invocation in `build_runner.rs` replaced it. Its doc claims to prove a cold output directory, but `:380` is an `exists()` check followed by a later create, so two callers could both pass.
- `build_execution.rs:529-625` — this module's `BuildEvidence`, `require_two_remote_hosts` (`:551`) and `append_jsonl` (`:615`) are never constructed or called. The runner uses `dacs_release_audit::BuildEvidence` (`build_runner.rs:1588`). `append_jsonl` writes the record and the newline separately (`:621-622`), so do not revive it as is.
- `build_execution.rs:26` — `BuildPlanError::Json` is produced only by `append_jsonl` (`:619`, the sole `serde_json` call in the file), so it goes with it.
- `build_runner.rs:321` — the `ProcessState::Unreconciled` arm is unreachable. `Supervisor::state` (`supervisor.rs:294-308`) returns only `Exited` or `Running`, and the variant is never constructed.
- `build_runner.rs:1481` — `path.ends_with(".rlib") &&` is redundant; non-rlib outputs already `continue` at `:1468`.
- `build_runner.rs:1128-1338` — `mod tests` sits in the middle of the file with production items after it (`:1340-1618`). Moving it to the end is mechanical; this is the layout `clippy::items_after_test_module` flags.

## Checked and found sound

- **Output capture vs. exit:** drains start before the first poll and are joined to EOF before any log is read or hashed (`build_runner.rs:312-340`). Reading continues past `LOG_LIMIT`, so a chatty child never blocks on a full pipe. `artifact()` re-checks length and mtime after hashing (`:164-187`).
- **`engine.jsonl` written while relayed:** the writer appends record, newline, then `sync_data` (`:273-276`). The reader (`runtime.rs:1458` onward) stops at a line without a trailing newline, does not advance its offset, and tolerates a missing file.
- **`client-engine.jsonl` and `consumption.jsonl`:** read only after the daemon is terminated and its drains joined, and after cargo's `command()` returned. The wrapper writes each record as one `write_all` under an exclusive file lock (`person-4/builds-release/src/rustc_provenance.rs`).
- **Replay guard:** `fs::create_dir(&root)` at `:443` is atomic and every evidence file uses `create_new`, so two executions of one attempt cannot both proceed.
- **Lock ordering:** `try_lock_exclusive` is non-blocking, so pool-then-port cannot deadlock. Locks are taken before the probe and launch and dropped after `terminate_attempt` (apart from finding 3).
- **Cancellation vs. completion:** a killed child cannot read as exit 0 (signal gives `None` on Unix; `terminate_job(job, 1)` on Windows). `terminate_attempt` fences the attempt, so the next `launch` in `run` fails. The deadline-vs-exit race in `command()` errs toward recording `None`.
- **Temp and target sharing:** each shard gets its own clone, `CARGO_TARGET_DIR`, `TMP`/`TEMP`/`TMPDIR` and test DB under a freshly created directory. Shards run sequentially and `DACS_TEST_LISTEN` is `127.0.0.1:0`.
- **Env and global state:** no `set_var`, `remove_var` or `set_current_dir` anywhere in the repo. Children get `env_clear` plus an explicit map.
- **`build_execution.rs`:** `write_private_config` (`:648`) uses `create_new`, mode 0600 and `sync_all`, and finishes before its consumer is launched. The used remainder is pure validation and descriptor building.
- **`task_board.rs` persistence:** `update` reads, modifies and inserts inside one write transaction (`:183-203`). Error paths drop the transaction, so the empty-title edit rolls back and delete-then-update returns `NotFound`. `list` reads an MVCC snapshot.
- **`task_board.rs` ids and timestamps:** ids are `Uuid::now_v7`, so there is no counter to race. `updated_at_ms` is kept monotonic inside the transaction. `created_at_ms` is taken outside it, which only affects display order under a clock step.
- **`task_board.rs` tests:** they use tempdirs, with no fixed ports or paths.

## Not covered

- Nothing was executed; this is a read-only review on Windows. The service and distributed paths are Linux-only and were judged by reading.
- Pinned sccache behaviour (exit on bind failure, client auto-start) and redb 2.6.4 internals were taken from documented semantics, not source.
- `dacs_release_audit::verify_distributed_build` was traced only to about `lib.rs:145`.
- `crates/dacs-worker/tests/windows_workflow.rs` was not reviewed.
- `supervisor.rs` changed on disk during the review (554 to 636 lines; commit `3028ec9` added `verify_leaderless_group`). I re-read `reconcile_previous` and `terminate`; finding 1 reflects the newer version.
- Adjacent observations outside this slice, not reported as findings:
  - `runtime.rs:930` does one final telemetry relay, and each call reads at most 128 records (`runtime.rs:1482`), so a larger backlog at service stop looks unrelayed.
  - `Supervisor::terminate` holds the global children mutex across `wait()` for up to the grace period (`supervisor.rs:313-340`), stalling every `state()` poll, including the deadline check in `command()`.