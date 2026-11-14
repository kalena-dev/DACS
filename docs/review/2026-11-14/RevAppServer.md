## Race conditions

Read-only review; nothing was built or run. Ordered by severity.

**1. In-flight network enrollment is aborted at shutdown, stranding the device in `IssueUncertain`** — medium (rare, but stuck until manual reconciliation), confirmed by reading
- Location: `crates/dacs-app/src/server.rs:1187-1199`, `1243-1244`; follows `crates/dacs-app/src/network.rs:281-295` and `crates/dacs-core/src/network.rs:278-316`.
- Interleaving: the maintenance task is inside `network.maintain()` → `issue_locked`. `network_begin_issue` has committed phase `IssueUncertain`, and the `v1/enrollments` call is awaiting (up to 20 s). Shutdown reaches `maintenance.abort()` at 1243, which cancels the future at that await, so `network_finish_issue` never runs.
- Impact: netadmin may have issued the grant, but the store stays `IssueUncertain`. `network_begin_issue` only acts on `AwaitingApproval`, so it is never retried and needs audit-derived reconciliation. Ticks keep starting new issue calls during the 30 s graceful window.
- Guard checked: intent-before-call is deliberate crash protection, but nothing lets an in-flight call finish on an orderly stop.
- Fix: replace `abort()` with a cooperative stop — select on a shutdown signal only at `tick.tick()`, then `maintenance.await` so the current iteration completes.

**2. A second `dacs serve` on the same data dir rewrites the live instance's job journal before failing to bind** — medium (wrong durable status, needs a double start), confirmed by reading
- Location: `server.rs:1078-1091` (state opened) versus `1178-1186` (bind); `crates/dacs-app/src/local.rs:198-216`; `crates/dacs-app/src/main.rs:440-447`.
- Interleaving: instance A has job J on disk as `running`. Instance B starts with the same `--data-dir`. B's `Jobs::open` reads J as `running` (local.rs:206). A finishes and writes `completed` (local.rs:277). B writes `interrupted` (local.rs:213). B then fails at `TcpListener::bind` (1178) and exits.
- Impact: J is permanently recorded as "Coordinator restarted; inspect the operation journal…" although it completed and A never restarted. Even without the lost update, every queued or running job of A shows `interrupted` until it finishes.
- Guard checked: there is no data-dir instance lock in `dacs-app`. `dacs-setup` has its own `setup.lock`/`update.lock`, and the source project lock makes B's `reconcile()` wait, so only the job journal is unprotected. `task_board.rs:42-45` already binds before opening state for this reason.
- Fix: move the two binds (1178-1186) above `local::admin_token` (1080). Better, take an exclusive `try_lock` file in `data_dir` right after `private_directory`, which also covers a second instance on a different port.

**3. SQLite runs on async worker threads through `store.authenticate`** — medium (self-healing stall), confirmed by reading
- Location: `server.rs:150`, `163`, `179`, `272`, `307`, `541`, `784`, `842`; `crates/dacs-app/src/builds.rs:27`, `49`. Also `app.jobs.get` at `server.rs:834`, `844` and `app.jobs.start` at `868`, `882`, `900`, `915` (file read / fsync+rename inline).
- Interleaving: a blocking thread holds `Store`'s `std::sync::Mutex<Connection>` (`dacs-core/src/lib.rs:247`) in `snapshot`, `maintain`, or an `execute` commit with `synchronous=FULL`. N requests arrive, and each handler calls `authenticate()` inline and parks its tokio worker on that mutex. With N ≥ worker count, nothing is polled.
- Impact: `/health` on both listeners, TLS handshakes, the `stop` task and the maintenance tick all stall until the mutex frees. The mutex is unfair, so the stall can span several queued DB operations.
- Guard checked: there is no deadlock, because the holder is always on a blocking thread and no guard crosses an await.
- Fix: route authentication through the existing `blocking()` helper (make `authenticate`/`form_auth` async); do the same in `builds.rs` and for `jobs.get`/`jobs.start`.

**4. `/api/enroll` is not idempotent** — low, confirmed by reading
- Location: `server.rs:490-503`; `crates/dacs-core/src/lib.rs:495-541`.
- Interleaving: the client posts an enrollment; the transaction commits a new worker and credential. The response is lost (client timeout; the blocking closure still commits after the handler is dropped). The client retries and gets a second worker and a second token.
- Impact: an orphan pending worker with a credential nobody holds, and two identical pending entries for the admin to choose between.
- Guard checked: the invitation is not consumed and there is no uniqueness check on `physical_host_id` or any request ID. `onboarding.rs:443` says "must be reconciled, not blindly repeated", so this is known client-side but unenforced.
- Fix: accept a client-generated enrollment ID and return the existing record on a repeat.

**5. The job "queue" is a mutex, not a queue** — low, confirmed by reading
- Location: `local.rs:178`, `244-259`.
- Interleaving: job A is running. `update/stage` then `update/apply` are submitted; each parks its own blocking thread on `serial.lock()`. When A ends, either may win, so `apply` can run before `stage`.
- Impact: the apply fails, or applies an older staged release.
- Related: each queued job pins one blocking-pool thread, so enough queued jobs stall every `blocking()` handler. A panic inside any `action()` poisons `serial`, after which every later job fails with "Local operation queue failed" until restart.
- Fix: one dedicated worker thread fed by an mpsc channel.

**6. The session flash is a single slot** — low, confirmed by reading
- Location: `server.rs:680-687` (write), `310-315` (take).
- Interleaving: tab 1 posts `/ui/invitation` and its handler sets `flash = A`. Tab 2 posts another action and sets `flash = B` before tab 1's redirected `GET /` arrives. Tab 1 renders B; A is gone.
- Impact: A is the only place the invitation token/URL is ever shown (the store keeps only its hash), so that invitation is unusable.
- Fix: make `flash` a `Vec<String>` and drain it on render.

**7. Shutdown signal handling** — low, confirmed by reading
- Location: `server.rs:1202-1207`, `1245`.
- The `stop` task exits after the first Ctrl-C, but tokio's handler stays installed, so a second Ctrl-C is swallowed. The process then sits with listeners closed in the unbounded `jobs.drain()` and must be killed.
- SIGTERM is not handled at all (unlike `task_board.rs:56-62`), so `kill` or a service-manager stop skips the graceful shutdown and the drain.
- Fix: add SIGTERM to the select on Unix, and keep listening so a second signal exits.

**8. Store maintenance is serialised behind network maintenance** — low, confirmed by reading
- Location: `server.rs:1190-1197`.
- Each tick awaits `network.maintain()`, which makes sequential netadmin calls with a 20 s timeout per device (`network.rs:135-136`). With N unresponsive devices, `store.maintain()` runs every ~N×20 s instead of every 5 s, delaying lease expiry and cancellation finalisation.
- Fix: run the two loops as separate tasks.

**9. `admin.token` is created non-atomically** — low, confirmed by reading
- Location: `local.rs:115-127`.
- A crash after `create_new` (124) and before `write_all` (125) leaves an empty file. Every later start fails with "Invalid admin token format" (107) until someone deletes it.
- Fix: write a temp file, fsync, then rename, as `save_json` does.

**10. CLI downloads are written in place** — low, confirmed by reading
- Location: `main.rs:375-384`, used at lines 746-753 for `tls`.
- A failed or interrupted write leaves a truncated file at the requested path, and the retry is refused by `create_new`. For `dacs tls`, a failure between `server.pem` and `server.key` leaves a certificate with no key.
- Fix: write a sibling temp file, then a no-clobber rename.

**11. Task-board CSP is computed once; `index.html` is read per request** — low, confirmed by reading
- Location: `crates/dacs-app/src/task_board.rs:115-118` versus `217-250`.
- After a Trunk rebuild while the server runs, the new inline loader is served but the startup hash no longer matches. The browser blocks the script and the board stays dead until restart.
- Fix: cache the index bytes at startup and serve those.

**12. `http_contract.rs` releases reserved ports before the child binds them** — low (parallel-run flake), confirmed by reading
- Location: `crates/dacs-app/tests/http_contract.rs:28-32`, `66-72`, `508-513`, `542`.
- The window is long because the child does `private_directory` (PowerShell and icacls on Windows), `Store::open` and TLS load before binding.
- Interleaving: test A frees port P. Parallel test B's `bind("127.0.0.1:0")` receives P. Either one child dies ("Coordinator exited before serving HTTP"), or A's `wait_ready` gets 200 from B's server, reads A's own `admin.token`, and then sees 401s.
- Line 542 also asserts the port is free again, which fails if another test took it.
- Fix: hold a process-wide `static Mutex<()>` from reservation until the child is ready or has exited.
- Suspected, not determined: the 3 s client timeout (lines 51, 148) on requests that fsync SQLite could trip under six parallel coordinators.

**13. Blocking work outlives the graceful timeout** — low, suspected
- Location: `server.rs:1205`, `1233`, `1245-1251`; `main.rs:482-484`.
- After 30 s, axum-server 0.7.3 force-closes connections (`handle.rs:95-97`), dropping handler futures. Their `spawn_blocking` closures (publish, import, execute) keep running, and `jobs.drain()` does not cover them.
- `run()` returns and `main` launches the update helper, but runtime drop then waits for those closures while the helper waits only 60 s for the executable lock (`dacs-setup/src/update.rs:511`).
- I could not determine the helper's behaviour on timeout.
- Related, also suspected: `pending_update` (`server.rs:928-930`) is a single slot, and jobs queued behind an update keep running during the drain, so a later job can replace or contradict the handoff.

**Not a race, but found on the way: `/ui/command` bypasses workflow-result validation** — high, confirmed by reading
- Location: `server.rs:710`, `751` versus `384-385`.
- `/api/command` runs `validate_workflow_artifact` before `store.execute`; the browser path calls `store.execute` directly.
- `login_submit` (`server.rs:272`) accepts any valid token, including a device token. An approved worker can therefore sign in and post `ReportEvent{WorkflowCompleted}` with a digest for a missing or mismatching artifact.
- The engine (`dacs-core/src/engine.rs:500-533`) checks only the digest format and self-reported evidence, then sets the task to `Accepted`.
- Fix: call the same validation inside the closure at `server.rs:750`.

## Cleanup candidates

- `crates/dacs-app/Cargo.toml:36` — `tower-http` is unused. Grep for `tower_http` under `crates/` finds nothing, and no other manifest references the workspace entry at `Cargo.toml:38`.
- `local.rs:151-157` — the `#[cfg(windows)]` and `#[cfg(not(windows))]` branches are the same `fs::rename` call; collapse to one.
- `local.rs:142-157`, `200-205` — `save_json` never removes its `*.tmp` file on failure, and `Jobs::open` only inspects `.json`, so the temps accumulate.
- `builds.rs:15-25` and `37-47` — identical bearer-header extraction twice. Lines 28-30 and 50-57 re-implement `server::blocking` (`server.rs:92`); make that helper `pub(crate)` and reuse it.
- `server.rs:1195-1196` with `network.rs:254` — without `--network-config`, `maintain()` returns `unavailable` at once, so an ERROR line is logged every 5 s forever. Skip the call when no transport is configured.
- `crates/dacs-ui/src/lib.rs:2-3` says "Never render secrets in notices", but `server.rs:681-686` and `758-772` put the invitation token and URL in the flash notice. One of the two is wrong.
- `crates/dacs-app/src/lib.rs:1,3` — `pub mod builds` and `pub mod network` have no users outside the crate. The only external paths are `dacs::{local, server}` (`main.rs:3`) and `dacs::task_board` (`main.rs:519`); `pub(crate)` would do.
- `task_board.rs:39` versus `45` — the containment check uses the canonical `data_path`, but `TaskBoard::open` uses the raw `data`. Open `data_path`.
- `http_contract.rs` — the login-and-extract-cookie sequence is duplicated in `browser_commands_require_origin_and_csrf_and_deduplicate_delivery` and `network_routes_reject_browser_cookies_even_with_valid_control_session`; a `Server::sign_in()` helper would remove about 30 lines.

## Checked and found sound

- **Locks across awaits (`server.rs`)**: `sessions`, `login_nonces` and `pending_update` are `std` mutexes taken in sync scopes (120-126, 234-241, 262-267, 281-290, 310-315, 343-346, 357-360, 680-687, 992-998). They are never nested, so there is no ordering inversion.
- **Login nonce and session cap**: the nonce is removed under the lock (262-267), so it is single-use. The cap check and insert share one critical section (281-290).
- **Revocation between auth and action**: `store.execute`, `snapshot` and `authorize_project` re-resolve the worker inside their own lock or transaction.
- **Command dedup**: lookup and insert happen in one transaction under the store mutex with a primary key on `(principal, id)`. Browser redelivery is covered by the first contract test.
- **Artifact upload**:
  - The digest is checked before any write, under the per-project OS lock.
  - The temp file is fsynced, then moved with `persist_noclobber`.
  - A concurrent same-digest upload sees the existing object and verifies it.
  - `get_artifact` re-hashes on read, so no partial file is observable.
- **Source bundle import**: sound at handler level only (buffered body, blocking thread, project lock); the quarantine internals are listed under Not covered.
- **Validate-then-execute on `/api/command`**: artifacts are immutable, and `sched::current` re-checks generation and attempt state inside the transaction. Replays still pass because validation uses `owned_attempt`.
- **Listener startup**: both sockets are bound before any server task spawns (1178-1186), a failure drops both, and TLS material is loaded first. Bind-relative-to-state ordering is finding 2.
- **Shutdown wiring**:
  - `notify_one` stores a permit, so the update job cannot lose the wakeup.
  - A sibling listener failure stops the other.
  - `Jobs::drain` registers `notified()` before reading `active`.
  - `RunningJob` decrements only after the final journal write.
- **Invitation and enroll**: validity check and worker insert are in one transaction, so revoke or expiry cannot interleave.
- **Job journal**: writes are temp + fsync + rename, so `Jobs::get` sees the old or the new document, never a partial one.
- **Task board**:
  - Bind precedes redb open, and redb's file lock rejects a second owner.
  - Edits read and write in one write transaction (`dacs-worker/src/task_board.rs:182-204`).
  - Static paths are canonicalised and contained; the residual check-then-read window needs local write access to the assets directory.
  - redb runs inline on async workers, which costs latency only.
- **`task_board.rs` tests**: they pass a `:0` listener straight in and use `tempdir`.
- **`http_contract.rs`**:
  - Every server has its own `tempdir`.
  - There is no `set_var` and no child `.env(...)`.
  - `127.0.0.1:1` is rejected by validation before any bind.
  - Readiness is polled against `/health` with a deadline.
- **`builds.rs`**: stateless handlers; only item 3 applies.
- **`lib.rs`**: module declarations only.
- **`main.rs`**: client subcommands are single-shot, `write_download` uses `create_new`, and artifact download verifies the digest before writing.

## Not covered

- `network.rs` and `onboarding.rs` internals beyond the calls followed from `server.rs` (maintenance loop, job start).
- `dacs-source` quarantine and Git internals, and the `dacs-setup` update helper (needed to settle item 13).
- Windows behaviour of `fs::rename` replacing a job file that a concurrent `Jobs::get` has open; not verified.
- Nothing was executed, so the test flake in item 12 is argued from the code, not reproduced.