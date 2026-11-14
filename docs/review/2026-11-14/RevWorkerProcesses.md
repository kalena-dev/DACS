## Race conditions

Nothing was executed (read-only, Windows host); every Unix conclusion is from reading the code plus the vendored `process-wrap 10.0.1`, `tokio 1.54.1` and `sysinfo 0.37.2` sources. Line numbers are for the tree after commit 3028ec9.

### 1. Unix: signals are sent by number after the leader was reaped behind tokio's back
- **Severity:** high (rare). **Confidence:** confirmed by reading, except std's `Child::kill` (raw `kill(pid)` when it has not itself waited), which is from memory because the std source is not on this machine.
- **Location:** `crates/dacs-worker/src/supervisor.rs:62`, `:322`, `:406`
- **Interleaving:**
  - `state()` polls through `self.child.try_wait()` at `:62`. `ProcessGroupChild::try_wait` reaps the leader with a raw `waitpid(-pgid, WNOHANG)`; tokio and std never learn the status.
  - Once the group is empty, the number P is free. Entries stay in the map long after that: `build_runner.rs:309-333` never terminates a normally exited command; that only happens at `build_runner.rs:477`.
  - After PID wraparound, `terminate()` at `:322` calls `killpg(P, SIGKILL)` with no attribution check. Every supervised child is a group leader, so P may now be another attempt's OMP tree.
  - When the entry is dropped at `:406` (or at worker exit), tokio's still-armed kill-on-drop sends `kill(P, SIGKILL)` to whatever process now has that PID.
- **Impact:** an unrelated process or another attempt's tree is SIGKILLed. Entries with `reservation_id: None` (the startup `capture(None)` calls) are never removed, so their exposure lasts the whole worker uptime.
- **Fix:** on Unix, observe exit without reaping (`waitid` with `WNOWAIT`) so the zombie leader pins PID and PGID until `terminate` has signalled the group. `terminate` then reaps through `inner.wait()`, which records the status in tokio and disarms kill-on-drop.

### 2. Linux: the `launched_at` bound falsely rejects the worker's own leaderless tree
- **Severity:** high. **Confidence:** confirmed by reading and arithmetic; not run.
- **Location:** `supervisor.rs:223`, `:445`, `:468`
- **Interleaving:** sysinfo computes start time as `btime + start_ticks / HZ`, two separate floors (`sysinfo/src/unix/linux/process.rs:418-422, 554-556`). That can be one second below the true wall-clock second. Example: boot at 1000.7, launch at 5000.20 gives `launched_at = 5000`; a child forked at 5000.25 computes as 1000 + 3999 = 4999, which is less than 5000.
- **Impact:** any live member forked in the same wall second as the launch is rejected with "started before the recorded leader", with probability roughly the fractional part of the boot time. `reconcile_previous` fails, the worker refuses to start, and the orphaned tree keeps running.
- **Test flake:** `worker_crash_with_live_process_trees_is_reconciled_on_restart` goes through the real `launch`, and its orphan `sleep 600 &` forks milliseconds after launch. The `.unwrap()` at `crash_recovery.rs:173` will therefore fail intermittently on Linux.
- **Fix:** compare with one second of slack (`process.start_time() + 1 < started`), or record raw start ticks plus a boot id instead of derived seconds.

### 3. One refused receipt aborts reconciliation of all the others
- **Severity:** medium. **Confidence:** confirmed by reading.
- **Location:** `supervisor.rs:156-192`
- **Interleaving:** every check uses `?` inside the `read_dir` loop (`:161`, `:164`, `:168`, `:176`, `:188`, `:190`). Receipt A is unattributable (or hits finding 2) and returns; receipt B, later in directory order, has a live leader with a matching start time and is never visited.
- **Impact:** B's tree, for example an OMP agent with owner credentials, keeps running unsupervised while the worker is down.
- **Fix:** process every receipt, collect the errors, and return them together at the end.

### 4. A stale receipt whose PID now names another process blocks startup indefinitely
- **Severity:** medium (rare, not self-healing). **Confidence:** confirmed by reading.
- **Location:** `supervisor.rs:167-171`, `:172-177` (Windows), `:435-438` (EPERM)
- **Interleaving:** receipts survive a reboot or PID wraparound. If the recorded PID is now any process (on Linux also any thread, because `System::new_all()` lists tasks), the start-time comparison at `:169` fails with "PID reused". An EPERM from the probe at `:435` bails the same way.
- **Impact:** the worker cannot start until the unrelated process exits or the owner deletes the file by hand. Yet by the invariant stated at `:182-184`, a reallocated id proves the original group is gone.
- **Fix:** record a boot identity in the receipt and clear receipts from an earlier boot without signalling. On the same boot, treat proven reuse as "group gone".
- **Suspected, not determined:** the exact-equality check at `:169` could also mismatch for the original live leader if the wall clock is stepped across a second boundary of `btime`.

### 5. The global `children` mutex is held across awaits
- **Severity:** medium (rare trigger). **Confidence:** confirmed by reading.
- **Location:** `supervisor.rs:313-352`, `:206-275`
- **Interleaving:**
  - `terminate` holds the map lock through `entry.child.wait()` for up to `grace` (`:327`), then through the Unix probe loop for another `grace` (`:335-345`).
  - `launch` holds it through two fsyncs and a blocking `System::new_all()` scan (`:232`, `:257`, `:260`).
  - If attempt A's tree does not disappear within grace, each of A's children holds the lock for up to 20 s. Meanwhile attempt B's watchdog blocks at `:360`, and the main loop blocks in `active_attempts()` (`runtime.rs:507`).
- **Impact:** emergency cancel or desktop pause for B, and the whole poll cycle, stall behind A.
- **Fix:** take the entry out of the map (or use a per-entry lock) before awaiting.

### 6. `prepare` cannot resume after an interruption
- **Severity:** medium (rare). **Confidence:** confirmed by reading.
- **Location:** `crates/dacs-worker/src/workspace.rs:204-206`, `:225-259`
- **Interleaving:** a crash, or a fence that kills `git clone`, between `:227` and `:259` leaves the directory present without `.dacs-source-pin`.
- **Impact:** a resumed workspace-class attempt (`runtime.rs:901`) fails permanently with "partial workspace exists; refuse to overwrite owner work", although no owner work can exist before OMP launches. The partial clone and `<attempt>.source.bundle` are never removed.
- **Fix:** clone into `<attempt>.partial`, finish all steps, then rename into place; delete a stale `.partial` on entry.

### 7. Leaderless attribution can accept a foreign group in the same session
- **Severity:** low. **Confidence:** confirmed by reading; needs a PGID-reuse coincidence.
- **Location:** `supervisor.rs:445-473`
- **Interleaving:** the two tests (session equals the launching worker's, start time not before `launched_at`) hold for any group created later in that session. For a worker started from a terminal, that session is the owner's whole login session. If the original group empties, its id is reused by a shell job whose first process then exits, and that job passes both checks.
- **Impact:** `terminate_group` at `:190` kills the owner's job.
- **Fix:** add a per-launch marker to the child environment (for example `DACS_PROCESS_ID=<id>`) and require it in every member's environment before killing.

### 8. Launch failure paths leave a PID-less or truncated receipt
- **Severity:** low. **Confidence:** confirmed by reading.
- **Location:** `supervisor.rs:125-135`, `:255`, `:257-264`
- **Interleaving:**
  - `save_receipt` truncates and rewrites in place. A crash between truncate and write leaves invalid JSON, which fails at `:161`.
  - A failure at `:255` or `:260` returns with the `pid: null` receipt (or its truncated remains) still on disk, even though the child was killed and waited.
  - The full-system scan at `:257` sits between spawn and the durable PID, which widens the crash window that produces a `pid: null` receipt and, on Unix, an untracked tree.
- **Impact:** the next start is blocked for a process that is provably gone.
- **Fix:** write to a temp file and rename; clear the receipt on those error paths; persist the PID before looking up the start time.

### 9. The marker is not the last step of `prepare`
- **Severity:** low. **Confidence:** confirmed by reading.
- **Location:** `workspace.rs:259-268`
- **Interleaving:** a crash or I/O error after `:259` but before `:267`. The resumed `prepare` accepts the workspace at `:204-223`, but `.dacs-source-pin` is untracked and not excluded.
- **Impact:** `git status --porcelain` at `:297` is never empty, so `dacs_submit` is refused until the agent commits the pin file. The bundle at `:268` also leaks.
- **Fix:** write the exclude entry and remove the bundle first, then write the marker last.

### 10. Unix: an exit without `terminate` kills only group leaders
- **Severity:** low (self-healing at next start, subject to finding 2). **Confidence:** confirmed by reading.
- **Location:** `supervisor.rs:243-245`
- **Interleaving:** `KillOnDrop` only sets tokio's `kill_on_drop`, which signals the leader PID; `ProcessGroupChild` has no `Drop`. An error return from `run` or a panic drops the supervisor.
- **Impact:** descendants keep running until the next start reconciles them as leaderless groups.
- **Fix:** `killpg` every unreconciled entry in a `Drop` for the supervisor's shared state.

### 11. OMP request timeout does not cover the write
- **Severity:** low. **Confidence:** confirmed by reading.
- **Location:** `crates/dacs-worker/src/omp.rs:152`, `:156`, `:188-192`
- **Interleaving:** `send()` is awaited outside the timeout while holding the `writer` mutex. If the child stops reading stdin and the frame exceeds the pipe buffer, `write_all` never completes.
- **Impact:** the request outlives its timeout and every other sender queues behind it. The agent loop is then stuck inside a tick branch and cannot observe stop or the runtime deadline. Only the watchdog's kill breaks it.
- **Fix:** wrap send and receive in one `tokio::time::timeout`.

### 12. Requests issued after the read loop ended wait the full timeout
- **Severity:** low. **Confidence:** confirmed by reading.
- **Location:** `omp.rs:341`, `:150-162`
- **Interleaving:** the read loop exits on a protocol error or event overflow and drains `pending` once at `:341`. A later `request_id` inserts at `:150`; `send` still succeeds because stdin is open.
- **Impact:** the caller waits 30–60 s and gets `Timeout` instead of `Closed`.
- **Fix:** set a closed flag under the `pending` lock on exit and return `Closed` when it is set.

### 13. Windows: stop is certified without confirming the job is empty
- **Severity:** low. **Confidence:** suspected.
- **Location:** `supervisor.rs:327-349`
- **Interleaving:** `JobObjectChild::wait` returns after the leader exits plus any one completion packet; `wait_on_job` never inspects the message code. There is no Windows equivalent of the Unix `killpg` probe.
- **Impact:** `reconciled` and `Stopped` may precede descendants releasing their handles. I could not determine whether `TerminateJobObject` guarantees all processes have ended when it returns.
- **Fix:** wait for `JOB_OBJECT_MSG_ACTIVE_PROCESS_ZERO`, or query the job's active process count.

### 14. `candidate` bundles `HEAD`, not the verified sha
- **Severity:** low. **Confidence:** suspected.
- **Location:** `workspace.rs:279`, `:308-311`
- **Interleaving:** `HEAD` moves between `rev-parse` and `bundle create`.
- **Impact:** the uploaded bundle's tip differs from the returned sha. I could not determine whether OMP can run another tool while the host call is pending.
- **Fix:** pin a ref to `sha` and bundle that ref.

### 15. Test-environment flakes
- **Severity:** low. **Confidence:** suspected (not run).
- **`crash_recovery.rs:161-163`, `:320-321`:** both tests need PID 1 to reap reparented orphans. In a container without an init they time out deterministically.
- **`crash_recovery.rs:226`:** hard dependency on `perl` being on PATH.
- **`crash_recovery.rs:135`, `:144`:** if the 60 s wait panics, the helper is never killed and holds the inherited stderr for 600 s. `leaderless_group` also asserts at `:249` before the caller creates its `GroupGuard`.
- **`crates/dacs-worker/tests/windows_workflow.rs:163`:** the 120 s `max_runtime_seconds` budget also covers a second full sysroot hash inside the workflow (`build_runner.rs:596-597`) plus a cold `cargo build`. On a slow or loaded runner it can hit the deadline.

## Cleanup candidates

- **`supervisor.rs:18`, `:20`, `:22`:** `ProcessState::{Starting, Stopping, Unreconciled}` are never constructed. Grep over `crates`, `person-4` and `examples` finds only the match arm at `build_runner.rs:321`, which is therefore dead.
- **`supervisor.rs:33`:** `ManagedChild.effect_class` is write-only. Grep finds only struct-literal assignments (`workspace.rs:104`, `runtime.rs:693`, `build_runner.rs:153`/`1191`, `build_execution.rs:192`/`222`/`436`/`501`, tests) and no read.
- **`omp.rs:34`:** `OmpToolCall` is unused. Grep finds only the definition and the re-export at `lib.rs:17`.
- **`supervisor.rs:323`:** the `InvalidInput` tolerance is unreachable. `ProcessGroupChild::start_kill` returns `killpg` errnos and `JobObjectChild::start_kill` wraps with `Error::other`, so on Windows `raw_os_error() == Some(3)` cannot match either.
- **`supervisor.rs:406`, `:361`:** entries with `reservation_id: None` are never removed (`forget_attempt` is the only removal) and `fenced` only grows. This is a handle and memory leak for the worker's lifetime.
- **`supervisor.rs:423-426`, `:470`:** the doc and error text say "recorded leader", but the bound is `launched_at` first (`:445`).
- **`runtime.rs:389`:** the vector returned by `reconcile_previous` is discarded; only tests consume it.
- **Worker workspaces:** nothing in the worker removes workspaces or source bundles. Grep for `remove_dir_all` hits only `dacs-source/src/storage.rs`.

## Checked and found sound

- **Fence versus launch (`supervisor.rs:206-211`, `:359-362`):** the fence is set under the same lock and in the same order as launch. A launch either finishes its insert before enumeration or sees the fence.
- **Receipt ordering (`:232`, `:251`, `:347-348`):** the receipt is durable before spawn, cleared on spawn failure, and cleared only after verified termination.
- **Attribution-to-kill window (`:451` to `:491`):** nothing pins the group between the checks and `killpg`. Swapping it needs every verified member to exit and the id to be reallocated to a new group within that window, which means a full PID wrap in milliseconds. It holds in practice, but only a pidfd would close it.
- **Group whose leader exited (`:435-477`):** an empty group clears, a foreign-session group is refused, a receipt without session is refused. The weaknesses are findings 2 and 7.
- **Windows leader-only polling (`:57-59`):** correct, because `JobObjectChild::try_wait` would consume completion packets the final wait needs.
- **`capture` (`workspace.rs:111-125`):** readers start before the wait, the group is killed before joining readers, and both joins are bounded. No pipe deadlock, no read-after-wait loss.
- **OMP correlation (`omp.rs:146-162`, `:317-333`):** insert happens before send, and the duplicate check is under the same lock. A timeout removes the entry, and a late reply falls through as `Unknown`, which consumers ignore. The read loop uses `try_send`, so responses cannot block behind events.
- **OMP readiness (`omp.rs:95-101`):** the `ready` watch has no missed wakeup.
- **Workspace paths:** `safe_id` plus a per-attempt directory; ids contain no dots, so `<id>.source.bundle` cannot collide with a directory. The same attempt cannot run twice (`running.contains_key`, plus the data-dir lock at `runtime.rs:371-378`). With no deletion, there is no cleanup-versus-running-child race.
- **Test isolation:** unique tempdirs; the helper's env var is set only on the child; the ignored helper returns without it. Port 49271 is only bound when `distributed` is `Some` (`build_runner.rs:748-793`), which the Windows test does not set.
- **`restart_without_live_processes…`:** it reuses a reaped PID, so it fails only on PID wraparound within the test.

## Not covered

- No builds or tests were run; findings 2 and 15 are unreproduced.
- I did not read sysinfo's macOS/BSD start-time and process-listing behaviour. Finding 2 is Linux-specific as stated.
- OMP's wire behaviour is unknown to me (event volume against the 64-slot queue, concurrent tool calls). The overflow-fails-session choice at `omp.rs:330-334` is documented as deliberate and I left it at that.
- `build_runner.rs` and `runtime.rs` were read only at call sites. One observation for the `runtime.rs` owner, outside my slice: `runtime.rs:586-594` emits `Stopped` for an external-effect attempt on `phase.is_some()` alone, without consulting the supervisor. It looks reachable in-process after `execute_attempt` fails with "attempt is not stopped" (around `:966-972`), because `pending_commands` includes `running` (`journal.rs:168`).