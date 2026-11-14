# Review: network enrollment adapter and device client (race conditions, then cleanup)

Scope: all files in `dacs-netadmin/src`, `dacs-netclient/src`, `dacs-net-contract/src` and `dacs-netclient/tests`. Read-only; nothing was built or run. `fake_tailscale_cli.rs` was re-read after commit 86a1d1b (398 lines) and its line numbers below are from that version.

One fact shapes the severities: the coordinator serializes every netadmin call under one `operations` mutex (`crates/dacs-app/src/network.rs:255,416,427,440`) and reserves host labels (`crates/dacs-core/src/network.rs:176-185`). Two requests from one coordinator therefore cannot overlap. The reachable races are against device joins (which go straight to Headscale), the local `issue` command, a second token holder, and a request cancelled by the coordinator's 20 s timeout (`network.rs:136`).

## Race conditions

### 1. Enrollment or revocation can change Headscale and leave no audit line
- **Severity:** medium (stuck state needing manual recovery). **Confidence:** confirmed by reading for the error paths; the dropped-handler path relies on hyper cancelling the handler on disconnect, which I did not exercise.
- **Location:** `net/crates/dacs-netadmin/src/service.rs:125,139` (enroll) and `:319,325,332,338` (revoke); upstream timeout at `headscale.rs:99`.
- **Interleaving, enroll:**
  1. The coordinator commits `IssueUncertain` and posts the enrollment with a 20 s total timeout.
  2. Netadmin makes two Headscale calls, each allowed 15 s. Headscale creates the key at `:125`, but the response times out, fails to decode, or the coordinator hangs up first.
  3. Line 139 is never reached, so the key exists in Headscale with no audit line.
- **Interleaving, revoke:** the key is expired (`:319`) and the node deleted (`:325`), then the verification `list_nodes().await?` at `:332` fails and returns 502 before the audit at `:338`. The coordinator's retry 5 s later finds nothing and audits `node_deleted: null, verified_absent: true`.
- **Impact:**
  - Enroll: the coordinator will only recover an uncertain issuance from an "audit-verified issuance key ID" (`dacs-core/src/network.rs:79-82,503-513`). With no line, the device stays in `IssueUncertain` or `RevocationPending` until someone finds the key by hand in Headscale.
  - Revoke: the log never records who deleted the node.
- **Smallest fix:**
  - Revoke: treat a failed verification list as `verified_absent = false` and still audit and return the outcome; the coordinator already retries on `false`.
  - Enroll: write an audit line on the `create_tagged_key` error path (device, host, error class, "key may exist"), or an intent line before the call.
  - Both: run the mutate-then-audit section in a spawned task so a client disconnect cannot cut it.

### 2. `join` reports failure while the daemon may still complete the join
- **Severity:** medium (wrong result, manual recovery). **Confidence:** client side confirmed by reading; daemon behaviour after a CLI timeout is from Tailscale's documented `--timeout` semantics (it only bounds the wait for Running), not exercised.
- **Location:** `net/crates/dacs-netclient/src/lib.rs:280-292`; contradicting comment at `:329-331`.
- **Interleaving:**
  1. `tailscale login --timeout=90s` hands the key to tailscaled, which registers the node (key consumed).
  2. The client sits in `Starting` — the stall the restun nudge exists for — and the CLI exits non-zero at 90 s, or `run` kills it at 100 s.
  3. `result?` at `:292` returns the error before the Running check at `:294-323` is reached.
  4. A later restun or DERP reconnect brings the profile to Running.
- **Impact:** the caller is told "join failed" for a host that is on the overlay with a consumed key. `onboarding.rs:1003-1004` then latches `join_started` and requires manual reconciliation. The comment says the outcome is "still judged by the Running + overlay-IP check after login", which is false on this path.
- **Smallest fix:** on `CliError::Failed` or `Timeout` from login, run the state poll once and return `Ok` if the profile is Running on DACS with overlay IPs; otherwise return the login error.

### 3. A membership query racing a join can report `Revoked`
- **Severity:** low (rare, heals on the next 5 s poll; the coordinator only stores the state, `dacs-core/src/network.rs:432`). **Confidence:** confirmed by reading.
- **Location:** `service.rs:207` then `:239,245`.
- **Interleaving:** `list_nodes` at `:207` returns no node; the device registers; `list_keys` at `:239` shows `used = true`; line 245 returns `Revoked`.
- **Impact:** a device that just joined is shown as revoked for one poll.
- **Smallest fix:** in the `k.used` arm, re-list nodes and re-run `find_node` before concluding `Revoked` (or read keys before nodes).

### 4. A revoke racing a join with the same key leaves the node and can misreport `key_expired`
- **Severity:** low (the outcome honestly says `verified_absent: false`, and the coordinator retries within 5 s). **Confidence:** confirmed by reading.
- **Location:** `service.rs:291,302,319,324,332-336`.
- **Interleaving:**
  - `list_nodes` at `:291` finds no node, so there is no delete target.
  - The device then registers either before `find_key` at `:302` (key seen as used, nothing expired) or between `:302` and `:319` (a consumed key is "expired" and `key_expired = true` is returned).
  - The re-list at `:332` sees the node but nothing deletes it.
- **Impact:** a revoked device stays on the overlay until the next coordinator retry, and `key_expired: true` contradicts its contract meaning ("was unused and is now expired").
- **Smallest fix:** if the post-list finds a DACS-tagged node for the request, delete it and re-verify once; set `key_expired` only if that node is absent.

### 5. The duplicate host-name check in `enroll` does not cover issued-but-unused keys
- **Severity:** low (not reachable from a single coordinator; needs the `issue` CLI or a second token). **Confidence:** confirmed by reading; Headscale's handling of the name collision is my assumption.
- **Location:** `service.rs:113-125`.
- **Interleaving:** two enrolls for host `x` both pass `list_nodes` at `:113` and both create keys at `:125`. The same happens sequentially at any point in the 15-minute key lifetime, because the check only sees registered nodes.
- **Impact:** two devices hold valid grants for one approved host name. The second node gets a different name, and `membership` still echoes `host_name: x` (`:226`) while its rename at `:218` fails and warns on every poll.
- **Smallest fix:** hold a `tokio::sync::Mutex` across list-then-create and keep an in-memory map of host name to unexpired key id, refusing a second issue until the first key is used, expired or revoked.

### 6. An audit line is written with many `write` syscalls and never synced
- **Severity:** low. **Confidence:** confirmed by reading (serde_json 1.0.154 `value/mod.rs:222-233` forwards every token to `write_str`; `File` is unbuffered).
- **Location:** `net/crates/dacs-netadmin/src/audit.rs:37`; the same file is opened by `serve` and `issue` (`main.rs:86,141`).
- **Interleaving:**
  - (a) The process is killed mid-record. The next start appends onto the unterminated line, and both records become unparseable.
  - (b) `issue` runs while `serve` records. The in-process mutex does not span processes, so fragments interleave within a line.
- **Impact:** a torn `enroll` line can lose the `auth_key_id` that finding 1's recovery depends on. `f.flush()` on a `File` is a no-op, so nothing is durable across an OS crash.
- **Smallest fix:** build `format!("{line}\n")` first and issue one `write_all`; replace `flush()` with `sync_data()` if durability is wanted.

### 7. `issue` creates the key before knowing the grant file can be written
- **Severity:** low. **Confidence:** confirmed by reading.
- **Location:** `net/crates/dacs-netadmin/src/main.rs:92-93,179`.
- **Crash point:** `enroll` creates and audits the key at `:92`; `write_private` then fails with `create_new(true)` because `--out` exists or is unwritable.
- **Impact:** an audited, valid, undeliverable key with nothing printed. A rerun issues a second key for the same host (finding 5).
- **Smallest fix:** create the output file (`create_new`, 0600) before calling `enroll`, write into it afterwards, and remove it if `enroll` fails.

### 8. The restun nudge runs inside the `select!` arm, so the join timeout is not enforced while it runs
- **Severity:** low (delay only; no state is lost, since `login` is polled by `&mut` and never dropped). **Confidence:** confirmed by reading.
- **Location:** `lib.rs:283-288`, `:332-341`.
- **Interleaving:** the tick fires and `nudge_stalled_start` awaits up to three CLI calls of 15 s each, during which `login` is not polled. The interval's default `Burst` behaviour then queues the missed ticks, and each later round has an even chance of nudging again instead of observing `login`.
- **Impact:** with a slow daemon, `join` can run well past `timeout + 10 s` and notices a finished login late.
- **Smallest fix:** add `biased;` with `login` first, set `MissedTickBehavior::Delay`, and wrap the nudge in a short `tokio::time::timeout`.

### 9. Smaller device-side items
All low.
- **Key file survives a kill** (`lib.rs:256-291`, confirmed). Ctrl-C or SIGKILL on `dacs-net join` during the login (up to 100 s) skips `Drop`, leaving `dacs-enroll-*` in the temp directory with a possibly unused key, mode 0600 on Unix. Fix: handle `ctrl_c` in `main.rs` so the join future is dropped, or unlink once the CLI has read the file.
- **Check-then-act on the active profile** (`lib.rs:349→353` for `leave`, `:252→280` for `join`, suspected). A `tailscale switch` between the state read and `logout`/`login` logs out, or displaces, a profile the owner did not approve. The CLI has no profile-targeted logout, so this cannot be closed without LocalAPI.
- **`traffic` diff** (`net/crates/dacs-netclient/src/main.rs:203-206`, confirmed). Snapshots are matched on a non-unique `host_name`, and a counter reset between them (tailscaled restart) saturates to 0, so the evidence silently under-reports. Fix: carry the node key in `PeerSnapshot`, or flag `after < before`.

## Cleanup candidates
- `net/crates/dacs-netclient/src/tailscale.rs:89` — `Tailscale::bin()` has no callers (grep for `.bin()` across `net/crates`, `dacs-app`, `dacs-worker`, `dacs-setup`, `person-4`: none).
- `tailscale.rs:35` — `Output::stderr` is set at `:124` and never read; the `ping` failure path uses `CliError::Failed.stderr`.
- `tailscale.rs:159` — `PeerStatus::dns_name` is deserialized and never read (grep for `dns_name`: definition only).
- `tailscale.rs:231` — `Pong::node` is set at `:264` and never read outside derives.
- `net/crates/dacs-netadmin/src/main.rs:152` — "dacs-netadmin serving" is logged before the certificate load (`:156`) and bind (`:161/:166`). Nothing consumes it (`net/evidence/wsl-e2e/setup.sh:159-161` sleeps then curls), so moving it is safe.
- `net/crates/dacs-netadmin/src/audit.rs:37` — the `f.flush()` no-op reads as a durability step (see finding 6).
- `net/crates/dacs-netclient/src/lib.rs:329-331` — the comment contradicts line 292 (see finding 2).

## Checked and found sound
- **Preflight DNS-then-connect** (`preflight.rs:170-187`). The resolved set is checked and then pinned with `resolve_to_addrs`, so there is no second resolution. Verified against reqwest 0.12.28: `client.rs:2278-2281`, lookup at `dns/resolve.rs:144`. The key and lookup name derive from the same `Url` host; `no_proxy` and no redirects; IP literals bypass DNS; private endpoints are rejected before any connection.
- **`select!` cancellation and child lifetime.** The child is spawned on first poll (tokio 1.54.1 `process/mod.rs:1065-1071`). `kill_on_drop` (`tailscale.rs:98`) fires on the `run` timeout and when the `join` future is dropped.
- **Key file on normal paths.** `tempfile` gives exclusive create, a random name and mode 0600. It is removed on every `?` return and before the result is inspected (`lib.rs:291`). The key is never on argv (asserted at `fake_tailscale_cli.rs:298`).
- **Operation tokens.** Loaded once (`main.rs:146`) into an immutable `Vec` in `Arc<AppState>`. There is no reload path, so no reload race; `net/README.md:424-425` documents restart to revoke. `check_private` (`config.rs:106`) is a metadata-then-read check, but only root or the service account can swap the file.
- **In-process audit appends.** One `Mutex<File>` in append mode, not held across an await, poison recovered (`audit.rs:36`).
- **Listener and TLS startup.** Bind and certificate errors propagate to exit 1, and the unit restarts on failure every 3 s. Plaintext is loopback-only (`config.rs:131`).
- **Revoke idempotence.** Tag and mismatch checks precede any mutation (`service.rs:291-312`). A retry after a partial run converges: an expired key is skipped at `:315-318` and a 404 is tolerated at `:327`. `HeadscaleClient` never auto-retries a POST.
- **Read-only snapshots.** `peers` is one `status --json` snapshot. `read_state` makes two CLI calls, but a failed prefs read falls back to the SaaS URL, which fails closed for `leave` and still requires consent for `join`.
- **Pure modules.** `compiler.rs` probes sequentially with no shared state; `placement.rs` and the contract `lib.rs` have no concurrency; `http.rs` state is immutable.
- **Tests, parallel safety.**
  - `DACS_TAILSCALE_BIN`, `FAKE_TS_DIR` and `FAKE_EXPECT_KEY` are set on the child only (`fake_tailscale_cli.rs:108-113`); there is no `set_var` anywhere in the slice.
  - Each test has its own `TempDir`.
  - The two new tests (`:356-398`) follow the same pattern, and `calls()[before..]` is sequential within one test.
  - Netadmin unit tests are in-memory with no ports.
  - The preflight test rejects before connecting.
  - `tcp_probe_reports_reachability` (`lib.rs:610`) is the only port user in its binary.
  - An ETXTBSY on the freshly written fake script is possible only in theory, because a full `dacs-net` startup separates the write from the exec.

## Not covered
- Real Headscale v0.29.7 behaviour: rename uniqueness, `DeleteNode` returning 404 vs 500, when `used` flips relative to node creation, expiring a used key. No Headscale source is in the repo, so findings 3–5 rest on the API shapes the code assumes.
- Real tailscaled behaviour after a CLI timeout or kill (finding 2), and profile switching (finding 9).
- Windows: temp-file ACLs, and the process-level tests are `#![cfg(unix)]`.
- axum-server 0.7.3 internals: handler cancellation on disconnect and bind timing were not read.
- `net/deploy` and `net/evidence` scripts were only searched for consumers of readiness and reload behaviour.