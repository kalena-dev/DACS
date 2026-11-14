# macOS platform row

Date: 2026-11-14, 20:25–20:34 UTC. Single host, loopback only.

| Item | Value |
|---|---|
| Host | macOS 27.0.1 (build 26A434), Apple Silicon `arm64`, 14 CPUs |
| Toolchain | `rustc 1.99.0 (b940084d7 2026-09-28)`, `cargo 1.99.0`, host `aarch64-apple-darwin`, LLVM 23.1.1; rustup 1.29.1, toolchain `stable-aarch64-apple-darwin` |
| Targets | `aarch64-apple-darwin`, `wasm32-unknown-unknown` |
| SDK | Command Line Tools (`/Library/Developer/CommandLineTools`), macOS SDK 27.0 |
| Web tooling | Trunk 0.22.1 with cached `wasm-bindgen` 0.2.130 (matches `Cargo.lock`) and `wasm-opt` version 131 |
| Clients | `/usr/bin/curl` 8.7.1 (SecureTransport, LibreSSL 3.3.6), OpenSSL 4.0.3 |

Commits:

- **`56422f5`**: full `cargo test --workspace`, Trunk build and every binary smoke below.
- **`8787088`** (`origin/main` after a rebase): repeated all three `cargo build` commands, tests for Person 2/3 crates, both Clippy runs and the `dacs-setup` failure check.
- **`23b67b6`** (`origin/main` after a later rebase): repeated tests for Person 2/3 crates and both Clippy runs. It adds `dacs-worker` physical-host capacity accounting and new `dacs-netadmin` and `dacs-netclient` tests.
- Between `56422f5` and `8787088`, the only source change in the crates the smoke used is `crates/dacs-worker/src/supervisor.rs`. The coordinator, TLS, task-board and `worker status` paths don't call it. The other changes are manifest lint inheritance, tests and docs.

## Results

| Check | Command | Result |
|---|---|---|
| Coordinator build | `cargo build -p dacs` | Passed at both commits. One rustc warning (Person 1, see below) |
| Native-shell build | `cargo build -p dacs --features native` | Passed at both commits. Links system `WebKit.framework` and `AppKit.framework` (`otool -L`); the default build links neither |
| Network binaries | `cargo build -p dacs-netclient -p dacs-netadmin` | Passed at both commits, no warnings. Produces `dacs-net` and `dacs-netadmin` |
| Workspace tests (once) | `cargo test --workspace --no-fail-fast` | **Failed** at `56422f5`: 203 passed, 13 failed, 1 ignored (a helper-process test). All 13 failures are in Person 1 code and share one macOS cause (failure 1 below) |
| Person 2/3 tests | `cargo test -p dacs-worker -p dacs-net-contract -p dacs-netclient -p dacs-netadmin -p dacs-task-board-web --no-fail-fast` | Passed at `8787088`: 96 passed, 0 failed, 2 ignored. Passed at `23b67b6`: 120 passed, 0 failed, 2 ignored. The ignored tests are helper processes that their parent tests run as subprocesses |
| Clippy, network crates | `cargo clippy -p dacs-net-contract -p dacs-netclient -p dacs-netadmin --all-targets -- -D warnings` | Passed at `56422f5`, `8787088` and `23b67b6`, no diagnostics |
| Clippy, worker | `cargo clippy -p dacs-worker --all-targets -- -D warnings` | **Failed**: 31 lint errors at `56422f5` and `8787088`. At `23b67b6` it passes without `-D warnings` but reports 29 warnings, so it would still fail (failure 3) |
| Frontend | `cd examples/task-board-web && trunk build --release` | Passed at `56422f5` in 33 s. `dist/`: JS 37,270 B, WASM 498,795 B, CSS 7,070 B |
| Coordinator smoke | `dacs serve --listen 127.0.0.1:17401 --data-dir <tmp>/data --local-setup` | Passed. Logs an ERROR every 5 s when no network is configured (observation A) |
| Coordinator under `$TMPDIR` | `dacs serve --listen 127.0.0.1:17403 --data-dir "$(mktemp -d)/data"` | **Failed**: exits 1 with `Error: Symlink/reparse path is not allowed: /var` (failure 1). The same directory given as its `/private/var/...` path served `/health` 200 |
| Development TLS | `dacs tls --hostname localhost --hostname 127.0.0.1 --output-dir <tmp>/certs` and `dacs serve --listen 127.0.0.1:17402 --tls-cert … --tls-key …` | Passed. Verifies only with explicit trust. `server.key` is written with mode 0644 (observation B) |
| Task board (API only) | `dacs task-board --listen 127.0.0.1:17410 --data <tmp>/tb.redb --assets examples/task-board-web/dist` | Passed for assets, create, edit, status, filters, delete, origin checks, a SIGTERM restart and a SIGKILL restart. Tested with curl, no browser |
| Worker status | `dacs worker status --data-dir <empty tmp>` | Sane: exits 1 with `Error: worker has not reported status`. Creates `worker.sqlite` as a side effect (observation C) |
| Setup detection | `RUSTUP_AUTO_INSTALL=0 dacs setup detect` (read-only `--version` probes) | Passed. Reports `macos`/`aarch64` with the `brew` manager. Detected 9 of 10 catalogue entries; `geany` is missing |
| Network client | `dacs-net probe 127.0.0.1:17420 127.0.0.1:17421 --timeout-ms 1000` against a loopback `python3 -m http.server` | Passed: open port reachable (exit 0 when alone), refused port reported with `os error 61` and exit 2 |

Test details from the full workspace run at `56422f5`:

| Crate (owner) | Passed | Failed |
|---|---|---|
| `dacs` lib / `tests/http_contract.rs` (P1) | 17 / 2 | 0 / **5** |
| `dacs-core` (P1) | 40 | 0 |
| `dacs-setup` (P1) | 18 | **8** |
| `dacs-source` (P1) | 14 | 0 |
| `dacs-ui` (P1) | 10 | 0 |
| `dacs-protocol` (P1) | 0 | 0 |
| `dacs-worker` lib / `crash_recovery` / `windows_workflow` (P2/P4) | 29 / 2 / 0 | 0 |
| `dacs-net-contract` (P3) | 8 | 0 |
| `dacs-netadmin` (P3) | 16 | 0 |
| `dacs-netclient` lib / `fake_tailscale_cli` / `join_preconditions` (P3) | 20 / 7 / 5 | 0 |
| `dacs-task-board-web` (P3) | 3 | 0 |
| `person-4/builds-release` (`dacs-release-audit` lib/bin) (P4) | 7 / 5 | 0 |

At `8787088` the Person 2/3 rerun adds the new `crash_reconnect` tests, which passed (2 passed, 1 helper ignored). `crash_recovery` passed 4 and `fake_tailscale_cli` passed 9.

At `23b67b6` the rerun adds these suites, which all passed on macOS:

- `dacs-worker` `tests/capacity.rs`: 2 passed.
- `dacs-netadmin` `tests/auth_and_tls.rs`: 12 passed.
- `dacs-netclient` `tests/measure_cli.rs`: 2 passed.

The `dacs-worker` lib now has 32 tests and the `dacs-netclient` lib 25; both passed.

## Failures

### 1. Symlinked macOS system directories rejected (Person 1, `crates/dacs-setup/src/util.rs`)

`reject_links` walks every ancestor of a state path and rejects any symlink. macOS ships `/var -> private/var` and `/tmp -> private/tmp`, and `$TMPDIR` is `/var/folders/...`. As a result:

- The 8 `dacs-setup` unit tests fail. They use `tempfile` under `$TMPDIR` and fail with `ApiError { code: Invalid, message: "Symlink/reparse path is not allowed: /var" }`:
  - `engine::tests::interrupted_journal_round_trip_is_not_completed`
  - `scan::tests::conflicting_toolchain_evidence_remains_unresolved`
  - `scan::tests::scan_does_not_execute_build_scripts`
  - `update::tests::{completed_running_executable_does_not_request_helper_or_shutdown, interrupted_backup_move_recovers_and_rolls_back, next_stage_does_not_destroy_previous_release_rollback, startup_reconciliation_never_applies_an_unapproved_stage, startup_recovers_prepared_and_already_installed_transactions}`
- The 5 `crates/dacs-app/tests/http_contract.rs` tests fail with `Coordinator exited before serving HTTP`. They spawn `dacs serve --data-dir <tempdir>`, and startup update reconciliation (`dacs_setup::reconcile_update` on `data_dir/updates`) rejects `/var`. The failing tests are `workflow_completion_is_bound_to_the_uploaded_result_and_owned_attempt`, `browser_commands_require_origin_and_csrf_and_deduplicate_delivery`, `tls_bootstrap_never_exposes_control_or_local_owner_routes`, `bootstrap_enrollment_shares_identity_without_approving_the_device` and `network_routes_reject_browser_cookies_even_with_valid_control_session`.
- In real use on macOS, `dacs serve` refuses any `--data-dir` under `/tmp`, `/var` or `$TMPDIR`.

Confirmation: with `TMPDIR` set to the real `/private/tmp/...` path, `cargo test -p dacs-setup --lib -p dacs --test http_contract` passed: `dacs-setup` 26 of 26, `http_contract` 7 of 7. At `8787088`, `cargo test -p dacs-setup --lib` with the default `TMPDIR` still fails 8 tests.

Suggested fix: only reject links that an unprivileged user could have planted. On Unix, accept a symlink ancestor when the link itself and its parent directory are owned by root and the parent isn't group- or world-writable. That covers `/var`, `/tmp` and `/etc` on macOS. Keep rejecting every other link, and Windows reparse points, as today. A narrower alternative is to canonicalize only those root-owned system prefixes before the walk. Don't weaken the tests; they are correct on macOS once the check stops treating system links as attacker-controlled.

### 2. `unused_mut` warning on non-Windows (Person 1, `crates/dacs-setup/src/util.rs:61`)

`let mut link = meta.file_type().is_symlink();` is only mutated inside `#[cfg(windows)]`, so every macOS build warns. Suggested fix: compute `let link = meta.file_type().is_symlink() || windows_reparse(&meta);` with a cfg-split helper, or add `#[cfg_attr(not(windows), allow(unused_mut))]`.

### 3. Worker Clippy under `-D warnings` (Person 2, plus Person 4 in `build_runner.rs`)

These are cross-platform style lints from Clippy 1.99, not macOS-specific behavior, so they were recorded rather than changed here. There were 31 errors at `56422f5` and `8787088`. At `23b67b6` there are 29: upstream removed the two in `supervisor.rs`. Line numbers below are at `23b67b6`:

| Lint | Count and location |
|---|---|
| `clippy::result_large_err` | 15 in `task_board.rs`. `BoardError` carries redb errors of at least 160 bytes |
| `clippy::collapsible_if` | 9 in total: `runtime.rs` 792, 793, 967, 1108; `omp.rs` 317, 318; `build_runner.rs` 266, 1014, 1396 (P4) |
| `clippy::too_many_arguments` | 4 in total: `runtime.rs` 1021, 1198; `build_runner.rs` 492, 1512 (P4) |
| `clippy::large_enum_variant` | 1 in `build_runner.rs:46` (P4) |

Suggested fixes:

- Box the redb source inside `BoardError`, or return `Box<BoardError>`.
- Collapse the nested `if`s into let-chains.
- Group the long argument lists into context structs, or add a reasoned `#[allow]`.

## Smoke details

All listeners used 127.0.0.1 ports 17401–17421. Every process was stopped afterwards with SIGTERM, apart from the intentional SIGKILL restart. A final `lsof -iTCP:17400-17499 -sTCP:LISTEN` showed no listeners. Data lived in temporary directories.

**Coordinator** (`--local-setup`, port 17401):

- `/health` returned 200 `{"protocol_version":1,"status":"ready","version":"0.1.0"}` with CSP, `nosniff` and `no-store`.
- Without a session, `/`, `/setup` and `/onboarding` redirect 303 to `/login`. `/network`, `/builds` and `/api/state` return 401. `/login` and `/assets/dacs.css` return 200.
- The data directory is 0700 and `admin.token` is 0600.
- `GET /api/state` with the bearer token, and `dacs snapshot --url http://127.0.0.1:17401 --token-file <tmp>/data/admin.token`, both returned the empty state.
- Logging in through the login form (CSRF nonce cookie plus `Origin`) returned 303 to `/`. With that session these pages rendered 200: `Dashboard · DACS`, `Local setup · DACS`, `Local owner onboarding · DACS`, `Network · DACS` and `Compiler pools · DACS`.
- SIGTERM stopped the process and freed the port.

**TLS** (port 17402):

- The generated certificate has SANs `DNS:localhost` and `IP Address:127.0.0.1`.
- With `curl --cacert server.pem`, both `https://localhost:17402/health` and `https://127.0.0.1:17402/health` returned 200, with `ssl_verify_result` 0.
- Without trust, curl failed with exit 60 (`self signed certificate`).
- `dacs snapshot --url https://localhost:17402 --ca server.pem` succeeded. Without `--ca` it failed with `invalid peer certificate: UnknownIssuer`.
- `openssl s_client -CAfile` negotiated TLSv1.3 / `TLS_AES_256_GCM_SHA384`, `Verify return code: 0`.
- Plain HTTP to the TLS port got no HTTP response (curl exit 1, `Received HTTP/0.9 when not allowed`).
- Login cookies over TLS carry `HttpOnly; SameSite=Strict; Secure`.

**Task board** (port 17410, API-level with curl, not a browser run):

- **Assets.** The `index.html` that was served references hashed JS, WASM and CSS. All three were served byte-identical to `dist/`, with correct MIME types (`application/wasm` for the WASM), a CSP carrying the exact script hash, `no-store` and `nosniff`. Unknown paths, and `../`/`%2e%2e` paths to the database, return 404.
- **Create.** Three tasks returned 201.
- **Edit.** `PUT /api/tasks/{id}` returned 200 with the new title and description.
- **Status.** `todo → in_progress → done` returned 200 at each step.
- **Filters.**
  - `q=estimates` returned 1 task.
  - `status=done` returned 1 task.
  - `q=report&status=todo` returned `[]`.
  - `status=todo` returned 2 tasks.
  - `q=REPORT` matched case-insensitively.
  - `status=bogus` returned 400.
- **Delete.** Returned 204. Deleting again returned 404.
- **Origin checks.**
  - A whitespace-only title returned 400.
  - A POST without `Origin`, a cross-origin POST, a foreign `Host` header and a DELETE without `Origin` all returned 403.
- **Persistence.**
  - After SIGTERM, the port was closed while the process was down (curl exit 7).
  - After a restart on the same `--data`, the task list was byte-identical to the list before the restart, and the deleted task stayed gone. A status change after the restart succeeded.
  - After SIGKILL and a third start, that change was still present.
- **Refusals.**
  - A second instance on the same database was refused with `Database already open. Cannot acquire lock`.
  - `--listen 0.0.0.0:…` was refused: `task-board preview must bind a loopback address`.
  - Passing the uncompiled source directory as `--assets` was refused.

**Worker status:**

- On an empty directory: exit 1, `Error: worker has not reported status`.
- On a missing directory: exit 1, with the raw SQLite error `unable to open database file: …/worker.sqlite`.

**Native shell:**

- `dacs --features native` builds and its `desktop --help` works.
- The default build's `dacs desktop` exits 1 with the "built without the native shell" message.
- The window itself was not opened.

## Observations (not counted as failures)

- **A (Person 1, `crates/dacs-app/src/server.rs` maintenance loop).** Without `--network-config`, `network.maintain()` returns `network infrastructure unavailable`, which is logged at ERROR every 5 s. Suggested fix: skip network maintenance when no transport is configured, or log once at INFO.
- **B (Person 1, `crates/dacs-app/src/main.rs` `write_download`).** `dacs tls` creates `server.key` with the umask default, 0644 here. The output directory itself is set to 0700, but the key's mode travels with it when moved, for example to `/private/server.key` as the README shows. Suggested fix: on Unix, open with `OpenOptionsExt::mode(0o600)` for the key, or for all downloads.
- **C (Person 2, `crates/dacs-worker/src/local_control.rs` `status`).**
  - `Journal::open` creates `worker.sqlite` (0644) in the directory it was asked to read, and a missing directory surfaces as a raw SQLite error.
  - Suggested fix: when `worker.sqlite` is absent, return "no worker state in DIR" without creating it.
  - This isn't macOS-specific, so it wasn't changed here.
- **Network client.** `net/evidence/macos-device-client.md` holds the separate read-only macOS `dacs-net status/assess/preflight` evidence against the host's stopped Tailscale login. This run didn't invoke Tailscale.

## Earlier macOS crate-test evidence (folded in from `macos-crate-tests.md`)

On the same host model, `cargo test -p dacs-worker -p dacs-netclient -p dacs-netadmin -p dacs-net-contract` ran at these commits:

- **`f55ff7f`:** `dacs-worker` 28, `dacs-netadmin` 16, `dacs-netclient` 14 and `dacs-net-contract` 6 passed, 0 failed.
- **`2438f07`:** `dacs-worker` 29 passed; the network crates were unchanged.

Installed OMP was `omp/18.16.0`; the team pins 18.20.0.

## Not run on macOS

- **Native desktop interaction.** No `dacs desktop` window, WKWebView session, capture, accessibility or input permission grant, and no OMP desktop control.
- **Provider-backed worker run.** No `dacs worker run`, no OMP provider login or readiness turn, and no task admission or publication. An `omp` binary is at `~/.local/bin/omp`. This run didn't execute it; the earlier note recorded 18.16.0, not the pinned 18.20.0.
- **Enrollment.** No invitation, consent, netadmin or Headscale, `dacs-net join` or `leave`, and no Tailscale state change.
- **Multi-PC.** No remote worker, distributed compilation, cross-host reachability or relay paths.
- **Browser UI.** No browser UI run of the task board or coordinator on macOS; the checks above were API-level only.
- **Setup and packaging.** No setup `apply`, install, repair or update, and no macOS package, signing, notarization or update application.
