# Worker on macOS: supervision, crash/reconnect and capacity

Scope: the Person 2 items covering process supervision, crash/reconnect with live processes, restart reconciliation of PIDs and receipts, and capacity accounting against the physical host. Other worker items (obligations, leases, coordinator loss, lifecycle controls, the real-OMP adapter) are recorded by their owners.

Date: 2026-11-14. Single host, loopback only, no model provider contacted.

| Item | Value |
|---|---|
| Host | macOS 27.0.1 (build 26A434), Apple Silicon `arm64`, 14 logical CPUs, 24 GiB |
| Toolchain | `rustc 1.99.0 (b940084d7 2026-09-28)` |
| Installed OMP | `omp/18.16.0` at `~/.local/bin/omp`. The team pins `18.20.0`. These tests use a scripted OMP stand-in; the installed binary was only asked for `--version` |
| Code under test | `crates/dacs-worker` at commit "worker: exercise the local capacity guard in the real worker loop", rebased on `origin/main` `894c14c` |

## What the tests do

- **`tests/crash_recovery.rs`** re-executes the test binary as a worker. That process launches real process groups through `Supervisor::persistent`: a live leader with a descendant, and a leader that exits immediately while its descendant keeps the group alive. The worker is then SIGKILLed, so no destructor or `KillOnDrop` runs. The test confirms the trees are still alive, restarts from the same data directory, and checks every PID with `kill(2)`. A supervisor-reported stop does not count as proof. The file also covers leaderless groups that DACS must not kill.
- **`tests/crash_reconnect.rs`** (harness in `tests/support/mod.rs`) runs the real `runtime::run` in a separate worker process. It talks to an in-process fake coordinator (axum, protocol v1 routes) and a `sh`/`awk` stand-in for the pinned OMP that answers the JSONL frames and never contacts a provider. Each attempt OMP leads a process group with a `sleep 600` descendant and keeps running after its RPC stdin closes. The fake coordinator records every command and, when a release (`ReconcileWorker` or `Stopped`) arrives, checks whether the watched PIDs are alive at that moment.
- **`tests/capacity.rs`** launches OMP-tool, build and compiler-daemon process trees under one reservation. It also runs `run()` with a declared capacity larger than the host. Its third test uses the same harness to admit two assignments whose combined reservations exceed the worker's declaration.

## Commands and results

| Command | Result |
|---|---|
| `cargo test -p dacs-worker` | Passed at `894c14c`: lib 36, `capacity` 3, `crash_reconnect` 2, `crash_recovery` 4, `windows_workflow` 0 (Windows-only), plus other owners' `journal_read_only` 6, `lifecycle` 6 and `omp_real` 12. The 3 ignored tests are helper processes their parent tests re-execute |
| `cargo test -p dacs-worker --test crash_reconnect -- --nocapture crash_then_fenced` | Passed. Restart trace below |
| Repeated runs (`cargo test -q -p dacs-worker --test <name>` in a loop) | `capacity` 60/60 (two-test version) and 15/15 (with the end-to-end test), `crash_recovery` 30/30, `crash_reconnect` 15/15, 5/5 and 10/10. Fixture processes seen by `ps` after a loop had all exited within seconds; other agents run the same tests on this host concurrently |
| Mutation: capacity guard disabled in `run()` (`if false && !fits(..)`) | `assignment_beyond_declared_capacity_waits_for_running_work_to_release` **failed** as intended: both OMPs launched (`left: 2, right: 1`). Source restored |
| Mutation: `supervisor.reconcile_previous()` commented out in `run()` | Both `crash_reconnect` tests **failed** as intended: the release arrived with the crashed tree alive (`watched_alive=[true, true]`), and the relaunch overlapped it. Source restored |
| Mutation: pre-fix `supervisor.rs` with the new `crash_recovery` test | **Failed** as intended: `prior process group remains or cannot be inspected` (orphaned group). The leaked fixture processes were killed by hand |
| `perl` zombie-only process group, `kill(0, -pgid)` before and after reaping | `EPERM` while the only member was an unreaped zombie, `ESRCH` after reaping. This explained a 1-in-40 `EPERM` failure of `Supervisor::terminate`, now fixed |
| `cargo clippy -p dacs-worker --all-targets` | No diagnostics in `supervisor.rs`, `capacity.rs` or the three test files. Warnings remain in other owners' files |
| `~/.local/bin/omp --version` | `omp/18.16.0` |

Restart trace from the fenced-attempt test (phase 2, after the crash; heartbeats, status reports and acknowledgements omitted):

```
<id-1> report_event usage    -> 409 watched_alive=[]           # refused (503) before the crash, replayed with the same id
<id-2> report_event settled  -> 409 watched_alive=[]           # same
<id-3> set_draining          -> 200
<id-4> reconcile_worker      -> 200 watched_alive=[false, false]
```

## Gate table

| Acceptance item (this scope) | Evidence | Status |
|---|---|---|
| Worker crash leaves live supervised trees; restart terminates and verifies them | `crash_recovery::worker_crash_with_live_process_trees_is_reconciled_on_restart` | **Passed** |
| Orphaned group (leader exited, descendant alive) is reconciled instead of blocking restart forever | Same test (`attempt-orphaned`). Fixed in "reconcile orphaned process groups after a crash" | **Passed** |
| Restart never kills a group it cannot attribute (PID reuse, another session, a member older than the launch, a legacy receipt) | `leaderless_group_in_another_session_is_never_killed`, `leaderless_group_is_killed_only_when_its_identity_matches_the_receipt`. The existing live-leader PID-reuse refusal is unchanged | **Passed** |
| Policy: live trees are terminated, never re-adopted (their RPC pipes died with the worker) | Supervisor restart path. Both `crash_reconnect` tests observe the kill | **Passed** |
| Results retained across the crash and replayed with the same message ids | `crash_recovery` (journal id and sequence after SIGKILL) and `crash_reconnect` fenced test (same `command_id` received before and after restart) | **Passed** |
| Fenced attempt on reconnect: processes verifiably gone, results reconciled, then allocations released | `crash_then_fenced_attempt_is_killed_replayed_and_reconciled_before_release`: the release comes after the replay, `watched_alive=[false, false]`, no relaunch, no receipts left | **Passed** (fake coordinator) |
| Valid attempt on reconnect is relaunched only after the crashed tree is gone | `crash_then_valid_attempt_relaunches_only_after_old_tree_is_gone`: the relaunched OMP itself saw no old PID alive; `open_session` sent | **Passed** (fake OMP) |
| Stop reported only after confirmed process termination (an abort response is not proof) | Release checks in both `crash_reconnect` tests. `exiting_group_members_are_polled_until_absent` covers the macOS zombie `EPERM` case | **Passed** |
| OMP tools, build/test processes and compiler allocations count against their reservation | `capacity::every_process_of_an_attempt_tree_counts_against_its_reservation` (`Supervisor::usage`, attribution by kernel process group, unreserved probe kept apart, release kills the whole tree) | **Passed** |
| Declared capacity cannot exceed the physical host | `capacity::worker_refuses_to_declare_more_than_the_physical_host`: `run()` refuses before launching OMP. Unit tests cover CPU, memory and disk limits | **Passed** |
| Local guard: an assignment that would exceed the declaration beside running attempts is held unstarted | `capacity::assignment_beyond_declared_capacity_waits_for_running_work_to_release`: over twenty 100 ms worker cycles the second assignment launched nothing and reported nothing. It started only after the first attempt's release, which arrived with `watched_alive=[false, false]`, and its OMP saw no first-tree PID alive | **Passed** (fake coordinator) |
| A VM is not counted as another machine | The coordinator folds capacity by `physical_host_id` (Person 1). The worker cannot detect virtualization and relies on the configured host id | **Not run** |
| The same behaviour on Linux and Windows | Linux not run. On Windows, restart still refuses while a previous job is active, and `Supervisor::usage` counts only job leaders | **Not run** |
| Crash with the real pinned OMP instead of the stand-in | Installed 18.16.0 does not match the 18.20.0 pin, and a live `dacs worker run` sends a provider-readiness prompt, which was not allowed | **Not run** |
| Real multi-PC assignments; native desktop interaction on macOS, Windows and Linux | Outside this host and scope | **Not run** |

## Limits and notes

- Process groups do not contain a descendant that calls `setsid` or `setpgid` to leave the group. Such a process escapes supervision and accounting. Containing it needs OS support (cgroups or job objects).
- A leaderless group is attributed by session and start time. That is strong evidence, not proof. Anything that fails the check is left running and reported as unreconciled, never killed.
- Restart reconciliation runs before the worker contacts the coordinator. After a restart, `terminate_attempt` on the new supervisor is trivially true, so the restart path is the only proof that crashed processes are gone. The mutation run shows the tests catch its removal.
- For the leases owner (code reading, not tested here): after a crash, `run()` reports an external-effect attempt as `Failed`. While its lease is unexpired, the coordinator accepts that and makes the task terminal. It then refuses `AbandonAttempt` ("only current unfinished attempts can be abandoned"), so the task never waits for explicit abandonment. This was not changed here.
