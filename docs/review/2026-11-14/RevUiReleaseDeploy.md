**Verdict:** no critical or high-severity race found in this slice. There are five medium and five low findings, mostly in the release tooling and the deploy script. Nothing was executed (read-only), so every finding is from reading the code; two rest on upstream behaviour I could not confirm in-repo and are marked suspected.

## Race conditions

**1. Toolchain pin is checked once before the build and never re-verified — medium, confirmed by reading**
- Location: `person-4/builds-release/src/packaging.rs:253-257` (only check), `:197-212` (`verify_source`, no toolchain check), `:278` and `:421` (post-build calls).
- Interleaving: `produce` reads `rustc --version` and `cargo --version` at 253-254. The repo has no `rust-toolchain*` file, so the active toolchain is rustup's mutable default. A `rustup update` or `rustup default` that lands before `cargo build` at 260 makes the build use a different compiler.
- Impact: `verify_source` passes, and `release-evidence.json` records `request.rustc_version` / `request.cargo_version`, which are echoed from the request rather than observed. The evidence claims a compiler that did not build the artifact.
- Fix: move the two version comparisons into `verify_source` so they run at 278 and 421 as well.

**2. Before/after source checks cannot see a change that is made and reverted during the build — medium, confirmed by reading**
- Location: `packaging.rs:240-252`, `:260-278`, `:421`.
- Interleaving: the pre-check passes; `cargo build` runs for minutes in the live checkout; another process switches branch and back, or edits and restores a file. `git status` at 278 is clean and HEAD matches again.
- Impact: the binary was compiled from content that is not the pinned commit, and the evidence says it was. `docs/team-integration.md` describes a shared `main` checkout worked by several people or agents, so a concurrent writer is plausible.
- Fix: build from a private export of the pinned commit under the fresh output directory (for example `git worktree add --detach <output>/source <commit>`), not from `request.workspace`.

**3. Task edit silently reverts another client's text change — medium, confirmed by reading**
- Location: `examples/task-board-web/src/app.rs:63-70`, `:234`, `:386`; `examples/task-board-web/src/api.rs:103-111`; server `crates/dacs-worker/src/task_board.rs:116-129`.
- Interleaving: tab A loads task T (title X, description d0). Tab B saves description d1. Tab A clicks Edit, which fills the form from its row snapshot, changes only the title, and saves. The PUT always carries both fields, so it sends the stale d0.
- Impact: the server applies both fields with no version precondition. B's d1 is gone and A sees "Task changes saved."
- Fix (client only): send only the fields that differ from the snapshot; the server's `EditTask` already accepts partial input. A full fix needs a server precondition on `updated_at_ms`, which is outside this slice.

**4. Re-running the deploy script without `COORDINATOR_OVERLAY_IP` tears down a working bootstrap relay — medium, confirmed by reading**
- Location: `net/deploy/infra-setup.sh:454-463`, `:599-603`.
- Sequence: run 1 sets the variable and activates the passthrough. A later run for an unrelated reason (new netadmin binary, say) omits the optional variable. The script rewrites the `BOOTSTRAP_DOMAIN` map entry to `reject`, reloads nginx, and exits 0 with only an info line.
- Impact: new-host onboarding through the bootstrap domain is refused until someone notices. This contradicts the header's "Idempotent: safe to re-run". `PUBLIC_IPV6` has the same revert-on-omission behaviour with less impact.
- Fix: when the variable is empty and the installed `/etc/nginx/dacs.conf` already has a live `:7444` upstream, reuse that address or stop with an error.

**5. Compiler slot is taken before the job is marked Started — medium, suspected**
- Location: `person-4/builds-release/src/instrumentation.rs:61-64` (inserted `DacsSlot::acquire()`), `:42-50` (blocking condvar wait).
- Interleaving: a job waits for a slot while the scheduler still sees it as Ready. From memory of upstream sccache v0.10.0, the scheduler drops Ready jobs unclaimed for 60 s at the next server heartbeat. A wait longer than that makes the later Started update fail with an unknown-job error.
- Impact: the client falls back to a local compile, so remote work silently degrades and a distributed-build proof run loses a required remote action.
- Not determined: the upstream scheduler source is not in this repo, so the 60 s timeout and state transitions are unverified.
- Fix: anchor the acquire after the Started update instead of before it.

**6. Post-packaging recheck discards its result — low, confirmed by reading**
- Location: `person-4/builds-release/src/main.rs:76-77`; `packaging.rs:422`, `:440-446`.
- Interleaving: `produce` hashes the artifact and writes `release-evidence.json`. The package file is then modified. `inspect(report.artifact.clone())?` re-hashes it but never compares against `report.sha256` or `report.bytes`.
- Impact: the tool exits 0 and prints the stale digest; the "race-checked" comment only covers changes during the second hashing itself. A downstream digest comparison would still catch it.
- Fix: compare the `inspect` result with the report and fail on mismatch.

**7. Operation-token hash is persisted before the token is shown — low, confirmed by reading**
- Location: `infra-setup.sh:792` (install hash), `:793-801` (print to `/dev/tty`), `:776-781` (re-run guard).
- Crash point: the script dies or the tty write fails between 792 and 801. The hash file is non-empty, so every later run reports "no new token issued".
- Impact: nobody ever saw the token, and it stays unusable until the hash file is deleted by hand. The window is a few statements wide.
- Fix: print the token first, then install the hash. The opposite failure only yields a token that was never registered, and the next run issues a new one.

**8. No single-instance guard, and fixed temp names in the final directories — low, confirmed by reading**
- Location: `infra-setup.sh:633-638` (`headscale-api-key.tmp`), `:753-763` (`key.pem.tmp`, `cert.pem.tmp`), `:776-792`.
- Interleaving (token): two concurrent runs both see no hash, both generate a token, and the second `install` overwrites the first hash. The token shown on the first terminal is dead.
- Interleaving (TLS): run B's `openssl` truncates `key.pem.tmp` while run A is between its `openssl` and `mv`. A can install B's key with its own certificate. Both files are then non-empty, so no later run regenerates them.
- Fix: take `flock -n` on a lock file at the top of `main` and exit if it is held.

**9. netadmin binary is replaced before it is validated — low, confirmed by reading**
- Location: `infra-setup.sh:734-735`; `:189-201` checks ELF magic only.
- Sequence: `install_file` overwrites `/usr/local/bin/dacs-netadmin`, then `--help` is run. A wrong-architecture ELF passes the magic check, fails `--help`, and the script dies.
- Impact: the running service keeps the old in-memory binary and fails at its next restart or reboot.
- Fix: run `"$NETADMIN_BIN" --help` before `install_file`.

**10. Packaging test uses a temp path keyed only by PID and does not clean up on failure — low, confirmed by reading**
- Location: `packaging.rs:493-496`, `:501-508`.
- Sequence: any failed assertion leaves `dacs-package-output-<pid>` behind. A later run that reuses the PID panics at `create_dir(&root).unwrap()`.
- Fix: use a unique temp directory (the `tempfile` crate is already used elsewhere in the workspace).

## Cleanup candidates

- `net/deploy/infra-setup.sh:540` — the first `cmp -s "$NGINX_CONF" "$backup/nginx.conf"` can never differ. Nothing touches `nginx.conf` between the backup at 533 and this line; only `ensure_nginx_includes` at 541 does, and 542 covers that. Safe to delete.
- `net/deploy/infra-setup.sh:765` — suspected dead guard. From memory, `openssl x509 -checkip` only prints "does / does NOT match" and exits 0 either way, so this `die` cannot fire; I did not run it. The `--cacert` health check at 834-838 is what actually rejects a certificate that does not cover the IP.
- `net/deploy/infra-setup.sh:838` — hard-coded `dacs-net/2026-11-14.1` duplicates `CONTRACT_REVISION` at `net/crates/dacs-net-contract/src/lib.rs:27` and will drift on the next bump.
- `crates/dacs-ui/src/native.rs:27-35` — the allow-list entries `/login`, `/setup`, `/onboarding`, `/network`, `/builds` are unreachable. The only caller (`crates/dacs-app/src/main.rs:561-564`) first runs `validate_url` (`main.rs:307-329`), which requires path `/`.
- `person-4/builds-release/src/packaging.rs:240-252` — inline copy of the HEAD, status and lockfile checks in `verify_source` (197-212). Replace with a call; this also makes finding 1 a one-place fix.
- `person-4/builds-release/src/main.rs:23-55` — `inspect` duplicates `packaging::digest` (`packaging.rs:51-73`): same hashing loop and same length/mtime check.
- `person-4/builds-release/src/lib.rs:5-51` — `REQUIRED_TOOL_CATEGORIES`, `ToolVerification` and `missing_catalogue_coverage` have no callers outside their own tests (repo-wide grep). `dacs-worker` uses only `BuildEvidence`, `RemoteAction` and `verify_distributed_build`.
- `crates/dacs-ui/src/lib.rs:389` — `render_invitation` takes an unused `_csrf`. The single caller is `crates/dacs-app/src/server.rs:477`.

## Checked and found sound

**Task-board client (`app.rs`, `api.rs`, `main.rs`)**
- Overlapping requests cannot occur. Every entry point checks `busy_untracked` (`app.rs:64`, `:95`, `:110`, and the two form handlers), and `loading` / `pending` are set synchronously before `spawn_local` (`:100`, `:113`). A slow list response therefore cannot overwrite a newer one.
- `pending` stays true across mutation plus reload (`:113-143`), so there is no gap for a second action.
- `load` reads the applied filters at its start (`:80-81`), not the unsubmitted filter inputs.
- Stale closures: `For` is keyed by the whole task (`:322`), so any server-side change recreates the row and its captured id and status.
- Focus restoration: `tasks.set` and `pending.set(false)` both happen before `request_animation_frame` (`:44`). By my reading of Leptos' effect scheduling the DOM is rebuilt and re-enabled before the callback runs; not run in a browser.
- Disposal: every signal is owned by the root `App`, which is never unmounted. Row `NodeRef`s are only used synchronously from that row's own handlers.
- An uncertain create is not retried automatically; the error text tells the user to refresh first (`api.rs:132`).

**Native shell (`native.rs`)**
- It starts no server; `dacs desktop` only opens a window on an already-running URL, so there is no in-process readiness ordering.
- `launch` runs on the main thread under `#[tokio::main]`. Window and webview are both moved into the event-loop closure. The navigation handler holds no mutable shared state.

**SSR views (`dacs-ui` `lib.rs`, `onboarding.rs`, `setup.rs`)**
- These are pure string rendering with no signals, async or shared state.
- Approval forms carry the reviewed state (`plan_sha256`, `candidate_sha`, `assessment_id`, `previous_control_url`) for the server to check against.
- `command_id` is minted per render (`lib.rs:56`), so a double-submit of one rendered form replays one id. The server dedups it inside one transaction (`crates/dacs-core/src/lib.rs:293-368`).
- One non-timing observation: `physical_host_id` is a fresh random UUID on every render (`onboarding.rs:68`). Retries are safe because `registration_started` is saved before the remote call (`crates/dacs-app/src/onboarding.rs:858-859`). However, the same PC gets a new "physical host" identity per enrollment unless the owner overwrites it, and scheduling and the two-distinct-hosts build proof key on that field.

**Packaging and audit (`packaging.rs`, `main.rs`)**
- `create_output`: the parent is canonicalized and both sides are de-verbatimed by `tool_path`. `create_dir` refuses an existing directory or symlink, so nothing can be pre-planted. A trailing `..` is rejected.
- Residual in `create_output`: the canonical path returned at 233 is not re-checked for containment. Exploiting that needs someone to swap a parent directory for a link between 222 and 232.
- `digest` compares length and mtime before and after hashing.
- `execute` creates logs with `create_new`, shares one file offset for stdout and stderr, and syncs.
- `verify_signed` re-hashes after the verifier and compares (`:477`).
- Staged runtime DLLs are hashed from the copy inside the fresh private output directory.

**Provenance wrapper (`rustc_provenance.rs`)**
- Each record is one buffer written under an exclusive lock on an append-mode handle, then `sync_data` (`:93-103`). The size bound is checked under the lock.
- A record torn by a kill fails the proof closed, because `json_lines` (`build_runner.rs:1346-1355`) rejects any malformed line.
- Outputs are hashed only after the engine exits.

**Instrumentation (`instrumentation.rs`), `lib.rs`, `preflight.rs`**
- All anchors are validated before any write. The three writes (`:126-128`) are not atomic, but a partial patch is caught by the clean-checkout check on re-run.
- The slot condvar re-checks its predicate and releases under the lock.
- `eprintln!` holds the stderr lock for the whole record, and the consumer (`build_runner.rs:266-271`) drops or rejects malformed lines.
- `lib.rs` is pure evaluation; `preflight.rs` is sequential read-only probes.

**Deploy script (`infra-setup.sh`)**
- `WORK_DIR` is `mktemp -d` with an EXIT trap (`:68-70`).
- Secrets are created under `umask 077` or via `mktemp` before any content is written: API key (`:631-638`), TLS key (`:752-763`), pre-auth key (`:686-689`, passed as `file:`), token hash (`:786-792`). None go through argv.
- The API key uses tmp-then-`mv`. A crash after `headscale apikeys create` only orphans an unused key. The `-e` test at 622 would keep a zero-length file after power loss; `-s` would self-heal.
- The cert and key are regenerated together if either is missing.
- Restart ordering: Headscale restart is followed by a 60 s health loop (`:391-402`). nginx gets `nginx -t` with rollback before reload (`:549-559`). netadmin runs `check` before restart, then an active wait and a verified HTTPS health loop (`:814-843`).
- There is no wait between the nginx reload and certbot (`:559`, `:586`). I did not report it because the ACME round-trips take far longer than a reload.
- The `A && B` lines are not last-in-function, so `set -e` does not trip on them.

## Not covered

- `crates/dacs-ui/src/style.css`, and the templates the script renders (`nginx/dacs.conf`, `headscale/config.yaml`, `policy.hujson`, `netadmin/config.example.json`), were not read. Of the deploy assets only `dacs-netadmin.service` was read.
- Nothing was run: no browser check of the focus ordering, no `openssl` exit-status check, no Windows check that `canonicalize` normalises case on both sides of the containment comparison.
- Upstream sccache v0.10.0 is not in the repo, so I could not verify that the patched engine compiles or how its scheduler behaves beyond what finding 5 states.
- `crates/dacs-worker/src/build_runner.rs` and `crates/dacs-app` were read only as far as needed to follow calls from this slice.