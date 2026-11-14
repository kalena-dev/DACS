## Race conditions

Read-only review; nothing was built or run. Every entry below is confirmed by reading unless marked otherwise.

### 1. A definite netadmin rejection of issuance is recorded as "uncertain" and has no exit — high
- **Location:** `crates/dacs-app/src/network.rs:179-182`, `:281-295`; `crates/dacs-core/src/network.rs:283-290`, `:512-530`, `:78-83`, `:454-462`, `:178`
- **Interleaving:**
  1. A maintenance tick calls `network_begin_issue`, which commits `IssueUncertain`.
  2. `issue_locked` posts `v1/enrollments`. Netadmin answers 400/403/409 before creating any key (`net/crates/dacs-netadmin/src/service.rs:103-122`: validation and host-name collision precede `create_tagged_key`), or the TCP connect fails.
  3. `call` collapses every non-2xx and every send error into `unavailable()`, so the record stays `IssueUncertain`.
  4. `network_begin_issue` now returns `None` forever. `network_reconcile` demands a non-zero `auth_key_id`, which does not exist.
  5. Revoking moves it to `RevocationPending`, where `revocation_request()` errors on every 5 s tick and never reaches `Revoked`.
- **Impact:** the device can never get a grant, and its host label stays reserved. The only way out is to reconcile with a fabricated key ID.
- **Smallest fix:** have `call` surface netadmin 400/403/409 and `reqwest::Error::is_connect()` as a definite rejection. Add a store transition `IssueUncertain` → `AwaitingApproval` keyed on the matching `operation_id`. Let an administrator resolve "audit shows no issuance" so `network_revoke` can reach `Revoked`.

### 2. Grant expiry races the owner's manual join, and an expired grant has no reissue path — high
- **Location:** `crates/dacs-core/src/network.rs:251-259`, `:424-428`, `:283-290`, `:519-530`; `crates/dacs-app/src/network.rs:261-267`
- **Interleaving:**
  1. Consent is sent at registration. The administrator approves at T.
  2. The next tick (≤5 s) issues a key expiring at T + `key_ttl` (default 15 min, `net/crates/dacs-netadmin/src/config.rs:62-64`).
  3. The owner is not notified; onboarding only polls on a manual refresh. They return at T+16 min.
  4. `network_grant` returns "network grant expired; administrator reconciliation required".
  5. `network_reconcile` rejects with "only an unknown issuance may recover references" because `auth_key_id` is set.
  6. Maintenance records `KeyExpired` membership, but the phase stays `GrantIssued`. Nothing ever returns a record to `AwaitingApproval`.
- **Impact:** the only exit is revoking the worker and re-enrolling, which finding 3 blocks on the owner side. The error text advertises a reconciliation that the code refuses.
- **Smallest fix:** in `network_membership`, on a netadmin-verified `KeyExpired` for the recorded key, clear `operation_id`, `auth_key_id` and `encrypted_grant` and set `AwaitingApproval`. That is a verified reissue, not a blind one.
- **Caveat:** `docs/team-integration.md` says a lost grant needs revocation and fresh enrollment. If that is meant to cover expired grants too, this is a design decision, and only the misleading message at `:257` needs fixing.

### 3. Owner intent flags are set before the effect and can never be cleared — high
- **Location:** `crates/dacs-app/src/onboarding.rs:692-699`, `:832-836`, `:858-859`, `:875`, `:954-958`, `:1001-1004`, `:331-334`
- **Interleavings:**
  - **Join fails before touching Tailscale.** `join_started = true` is saved at `:1001-1002`. `dacs_netclient::join` then re-reads Tailscale state (`net/crates/dacs-netclient/src/lib.rs:250-253`). If the daemon stopped or the login changed since the check at `:996`, it returns `Prerequisite`, `SwitchConsentRequired`, `AlreadyJoined` or `Grant`. Line `:1004` maps every error to "did not confirm success", so the flag stays set with the key unused.
  - **Pre-send save fails transiently.** If `save` at `:859` or `:1002` fails, the error path at `:333` saves again with the flag still true. Intent is persisted for a request that was never sent.
  - **Ambiguous enroll.** After a timeout the flag stays set by design. But `:692` refuses any other invitation on the local flag alone, even after the coordinator has revoked the device.
- **Impact:** `registration_started` is only reset at `:875` and `join_started` is never reset. The owner must hand-delete `owner.json`, which holds the credential.
- **Smallest fix:** clear the flag when the pre-send save fails. Reset `join_started` for the pre-effect `NetError` variants. Add an explicit, owner-confirmed "abandon uncertain registration/join" operation.

### 4. Shutdown aborts maintenance in the middle of an issuance — medium
- **Location:** `crates/dacs-app/src/server.rs:1243` aborting `crates/dacs-app/src/network.rs:281-295`
- **Interleaving:** `network_begin_issue` has committed and `v1/enrollments` is in flight (up to 20 s). Ctrl-C or an update triggers `maintenance.abort()`, which drops the future. Netadmin has issued the key, but the response is discarded.
- **Impact:** the device is left `IssueUncertain` with a live key, needing audit reconciliation, revocation and re-enrollment. Jobs are drained on shutdown (`:1245`); this task is not.
- **Smallest fix:** signal the loop to stop only at `tick.tick()` and await it instead of aborting.

### 5. Automatic network revocation is gated on an unrelated contract check — medium
- **Location:** `crates/dacs-app/src/network.rs:254-260`, `:207-232`
- **Interleaving:** a worker is revoked through `Command::RevokeWorker` (`crates/dacs-core/src/engine.rs:239`), so only `maintain` performs the network step. If `contract()` fails validation (netadmin disclosure or contract revision skew, or an advertised URL that `endpoint()` rejects), `maintain` returns at `:256` before the loop.
- **Impact:** no `v1/revocations` call is ever attempted although the endpoint works. The node stays on the overlay, the phase stays `grant_issued`, and the ciphertext stays at rest. Application access is still denied.
- **Smallest fix:** process `worker.revoked` records before `contract()`, or independently of it. Run the store-only `network_revoke` step even when the transport is unconfigured.

### 6. Host-label collision is detected only after the irreversible enrollment, and the label cannot be changed — medium
- **Location:** `crates/dacs-app/src/onboarding.rs:856-894`, `:899-903`, `:439-443`; `crates/dacs-core/src/network.rs:175-186`
- **Interleaving:**
  1. Devices A and B both register with label `desktop`. A's consent commits.
  2. B's `/api/enroll` succeeds and its credential is saved. Then `send_consent` gets 409 "host label is reserved".
  3. `remote` rewrites every 409 to "approval… pending; refresh after the coordinator owner resolves it".
  4. The `consent` operation refuses any label other than the saved one, so B replays the same 409 forever.
- **Impact:** B is wedged with a misleading message. The coordinator would accept a changed consent while the phase is `AwaitingApproval`; only the client forbids it.
- **Smallest fix:** allow a new label in `consent` while `!consent_sent && !join_started`, replacing the saved consent only after the coordinator accepts. Surface the conflict reason distinctly.

### 7. Re-inspecting after registration needs a still-valid invitation and can erase the reviewed contract — medium
- **Location:** `crates/dacs-app/src/onboarding.rs:557-561`, `:732-747`, `:331-334`; `crates/dacs-core/src/lib.rs:602-604`
- **Interleaving:**
  1. A registered owner re-inspects while netadmin is briefly unreachable. Line `:735` sets `state.contract = None`, and the error-path save persists it.
  2. The invitation then expires or is revoked. `fetch_invitation` now always fails with 403.
  3. `inspect` is the only writer of `state.contract`, so `current_contract` fails with "Inspect the current network disclosure first".
- **Impact:** consent, assess and join are permanently blocked. The same happens if the contract changes after the invitation expires.
- **Smallest fix:** when `registration_started`, keep the saved invitation if the fetch reports expired or revoked, and never overwrite a reviewed contract with `None`.

### 8. A failed join attempt mutates the saved consent that "consent" replays — low
- **Location:** `crates/dacs-app/src/onboarding.rs:995-997`, `:910-914`, `:331-334`
- **Interleaving:** `consent.tailnet_switch` is set at `:995`. If `check_join` fails at `:996`, or the process crashes between the two saves, the modified consent is saved with `join_started` false. A later consent replay then differs from the coordinator's stored consent and gets 409.
- **Impact:** harmless, but it contradicts the comments at `:910` and `:998-1000`.
- **Smallest fix:** pass a cloned consent to the join, or store switch consent in a separate field.

### 9. The global `operations` lock is held across every device's netadmin call — low
- **Location:** `crates/dacs-app/src/network.rs:255-278`, `:420-429`, `:440`
- **Interleaving:** `maintain` holds the lock for N devices at up to 20 s each when netadmin is slow. Any approved device can also take it through bootstrap `device_refresh`. An administrator's revoke request waits behind them for its netadmin step.
- **Impact:** self-healing. Application revocation is not delayed, because `network_revoke` runs before the lock, and the next tick retries the network step.
- **Smallest fix:** take the lock per device inside the loop.

## Cleanup candidates

- **`crates/dacs-core/src/network.rs:296-300`**: `.ok_or_else(|| conflict("missing consent"))` is unreachable; `consent.is_none()` already returned `Ok(None)` at `:287-290`.
- **`crates/dacs-core/src/network.rs:533`**: `auth_key_id.is_some() &&` is always true after the check at `:512`. The `Option<u64>` parameter is effectively required.
- **`crates/dacs-app/src/network.rs:26`**: the `Serialize` derive on `NetworkConfig` is unused. A grep of `crates/` shows it is only deserialized (`server.rs:1087`) or constructed in tests.
- **`crates/dacs-app/src/network.rs:509-512`**: an orphan comment with escaped quotes after `bootstrap_router`, apparently left over from a string literal. It duplicates the docs.
- **`crates/dacs-app/src/network.rs:395-430`**: `state`/`device_state` and `refresh`/`device_refresh` differ only by one `device_only` call.
- **`crates/dacs-app/src/network.rs:254`** with `server.rs:1195-1196`: without `--network-config`, `maintain` returns `unavailable()` and logs an error every 5 s forever. Return `Ok(())` when unconfigured.
- **`crates/dacs-app/src/onboarding.rs:117`**: `file.flush()` on an `fs::File` is a no-op; `sync_all` follows.
- **`crates/dacs-app/src/onboarding.rs:102-131` and `:133-171`**: two near-identical temp-file-and-rename writers, plus a third in `local.rs:143`. `save` could call `save_private_bytes`.
- **`crates/dacs-app/src/onboarding.rs:106`, `:149`**: a crash between create and rename leaves `<uuid>.tmp` files that nothing removes. Those from `save` contain the full state including the device token.
- **`crates/dacs-app/src/onboarding.rs:583`**: the `id: String` parameter of `network_consent` is always `String::new()` at its single call site (`:856`), then overwritten at `:890`.
- **`crates/dacs-app/src/onboarding.rs:380-401`, `:493-519`**: these duplicate `endpoint` and the contract checks in `crates/dacs-app/src/network.rs:78-97`, `:208-216`, with different rules. Onboarding requires path `/`; the bridge allows paths and loopback HTTP for the login server.
- **`docs/team-integration.md`, "Local owner onboarding"**: it lists eight operations. The code also accepts `configure_worker` and `worker_control` (`onboarding.rs:313-314`).

## Checked and found sound

- **Issue ordering.** `network_begin_issue` commits `IssueUncertain` in one transaction before any I/O. Only `maintain` calls `issue_locked`, under the `operations` lock, and a second begin returns `None`. There is no double issue and no blind reissue after a crash.
- **Late issue response against revoke.** The revoke handler commits application revocation and `RevocationPending` before taking the lock. `network_finish_issue` accepts `RevocationPending`, records `auth_key_id` and stores no grant. `revoke_locked` re-reads the record under the lock, so it sees the key ID.
- **Refresh against revoke.** `network_revoke` clears `refresh_operation_id`, and `network_membership` rejects the superseded operation. All refresh paths run under the lock.
- **Revocation when the second step fails.** Application revocation and the network intent share one transaction. Unverified outcomes stay `RevocationPending` and are retried every tick. Netadmin's revoke is idempotent and finds a joined node by key ID when `node_id` is unknown (`service.rs:171-189`, `:265-320`).
- **Reconcile.** It runs under the lock, is one-shot, and checks reference uniqueness inside the transaction.
- **Rejected grants.** Dropping a mismatched grant without recording its key ID is deliberate (comment at `crates/dacs-app/src/network.rs:292-293`, test `escalated_grant_never_becomes_deliverable`). Not reported.
- **Concurrent consent for one device.** Serialized by the store mutex and transaction; the idempotent replay check precedes the phase check; the label-uniqueness scan is in the same transaction.
- **Grant at rest.** A random 96-bit nonce per write, with AAD binding the device ID. The key is shared with command dedup (`lib.rs:319`), but the AAD formats cannot collide. The ciphertext is cleared on revoke and never appears in snapshots.
- **Token and CA reads (bridge).** Read once at startup with a bounded `take(max+1)`, so no read-size race. There is no check that the token file is private or regular, which is not a race. The onboarding CA file is re-read on every job, so its contents can change after review (owner-controlled path, not reported).
- **`owner.json` writes.** Temp file, `sync_all`, rename, and a directory fsync on Unix. Readers never see a partial file. All writers go through the single serial jobs mutex, and no other code writes the file (grep).
- **CSRF and jobs.** Form posts need Origin, session and a constant-time CSRF compare. API posts need a bearer, or session plus Origin plus `x-dacs-csrf`. Both require `local_admin`, and the routes exist only on the control router. The review token and the `registration_started`/`join_started`/`assessment_id` guards are evaluated inside the job, so a queued stale request cannot act on changed state.
- **Job results.** They carry only the `status()` projection, with no tokens. I found no reachable panic in `onboarding.rs` that would poison the queue mutex; the `expect`s at `:104`, `:1003` and in `plan` are guarded.
- **Tests in these files.** They use `:memory:` or tempdir stores and bind `127.0.0.1:0`, with no sleeps or shared environment. The fixed `127.0.0.1:9` address is never contacted.

## Not covered

- Runtime behaviour: Windows rename semantics under antivirus or indexer locks, and powershell/`icacls` failures in `local::private_directory`, were reasoned about but not exercised.
- Callee internals were not read beyond their signatures: `dacs_setup::plan`/`apply`, `dacs_worker::local_control` and `runtime::WorkerConfig`. Whether they can panic inside a job is unverified.
- `net/crates/dacs-netadmin/src/headscale.rs`, `audit.rs` and most of `http.rs` were not read; only the status mapping and service-level ordering were.
- `crates/dacs-ui/src/onboarding.rs` was grepped, not read in full.
- `crates/dacs-app/src/local.rs` and `server.rs` were read only as far as needed to follow calls.