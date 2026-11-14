# Single-host control-plane end-to-end (macOS, fake OMP)

Date: 2026-11-14. Host: macOS 27.0.1 (26A434), Apple Silicon `arm64`, `rustc 1.99.0`, Git 2.54.0 (Apple Git-157), Python 3.9.6.
Code: `a5da49f` (on top of `origin/main` `23b67b6`); `cargo build -p dacs` debug binary. Loopback only, ports 27443/27444.

The DACS control plane ran end to end on one Mac with real binaries, TLS, device-token authentication, a SQLite coordinator, a bare Git integration repository, the worker journal, supervised process groups and isolated workspace clones. One task was dispatched, edited, submitted, reviewed and published into the coordinator's bare repository. Two `kill -9` crash drills followed. A replayable workspace attempt was reattached under the same attempt and generation and published. An external-effect attempt was failed without replay. Final run: **32 gates passed, 1 blocked, 0 failed**.

**This is not provider evidence, not a multi-PC proof and not network enrollment.** The OMP executable was a labelled stand-in ([`fake-omp`](single-host-e2e/fake-omp)) that never contacts a model provider. Coordinator and worker shared one physical host and loopback addresses.

## What was real and what was fake

| Component | Status in this run |
|---|---|
| `dacs tls`, `dacs serve` (TLS control + TLS bootstrap listeners, `--local-setup`) | Real |
| Coordinator store, commands, admission, reservations, leases, events, message queue | Real (`dacs-core`) |
| Source import/export, quarantine, prepare/review/publish into `repository.git` | Real (`dacs-source`) |
| Invitation, `/api/enroll` on the bootstrap listener, `ApproveWorker`, device bearer tokens | Real |
| `dacs worker run` journal/outbox, heartbeat, runtime status, supervisor, workspace clone, host tools, crash reconciliation, drain | Real (`dacs-worker`) |
| `dacs worker status`, `dacs worker control --action drain` | Real |
| Git operations by worker, coordinator and agent | Real (`/usr/bin/git`) |
| OMP executable | **FAKE**: `fake-omp` speaks the JSONL RPC framing from `crates/dacs-worker/src/omp.rs`. It makes a deterministic edit, calls `dacs_pending` and `dacs_submit`, emits `agent_end` and `prompt_result` |
| Provider-readiness turn at worker start | **FAKE**: answered locally with `prompt_result completed`. `provider_authenticated=true` here is NOT provider evidence |
| Usage/cost | None reported: the fake returns no token counts, so no `Usage` event was produced |
| Owner onboarding (`/api/onboarding/*` → `configure_worker`) | **Blocked**: requires a netadmin network contract (see findings). `worker.json` was written by the harness in the exact `WorkerConfig` schema that `configure_worker` produces |
| Network enrollment (netadmin/Headscale/Tailscale) | Not run (out of scope) |

The harness runs everything with `HOME=<run>/home` and `GIT_CONFIG_NOSYSTEM=1`. The worker, fake OMP and Git never saw the real home directory, `~/.omp`, provider credentials or global Git configuration.

## Reproduce

```sh
docs/evidence/single-host-e2e/run.sh
# options: DACS_BIN=<prebuilt dacs>  DACS_E2E_RUN_DIR=<dir>  DACS_E2E_CONTROL_PORT/DACS_E2E_BOOTSTRAP_PORT (27400-27499)
```

Prerequisites: Rust toolchain (unless `DACS_BIN` is given), `git`, `jq`, `curl`, `python3`, `openssl`. The script prints a gate table and exits non-zero if any gate failed. Evidence lands in `<run>/evidence/` (gates, requests/responses, snapshots, worker statuses, `ps` captures, fake-OMP traces). Logs land in `<run>/logs/`. Secrets (admin/device/invitation tokens) stay in 0600 files under `<run>/secrets/` or the coordinator data directory and are redacted in evidence. Every process the script starts is stopped on exit. Run time after the build is about 40 s.

Three runs were made. Run 1, before the rebase and before crash drill B was added, failed one gate because of a harness bug: the export clone lacked `-b main` (finding 3). Run 2, before the rebase, and run 3, after rebasing onto `23b67b6` and rebuilding, passed every gate except the blocked onboarding gate. All three produced identical integrated SHAs. The figures below are from run 3.

## Flow, commands and observed states (final run)

Abbreviated from `run.sh`. `$C=https://127.0.0.1:27443`, `$B=https://127.0.0.1:27444`, `$CA=<run>/tls/server.pem`.

1. **Development TLS.** `dacs tls --hostname localhost --hostname 127.0.0.1 --output-dir <run>/tls` produced SAN `DNS:localhost, IP Address:127.0.0.1`. Clients trusted it explicitly. A request without `--cacert` was refused by certificate verification.
2. **Coordinator.** `dacs serve --listen 127.0.0.1:27443 --bootstrap-listen 127.0.0.1:27444 --public-url $C --host-name dacs-e2e-coordinator --data-dir <run>/coordinator --local-setup --tls-cert … --tls-key …`. Both listeners returned TLS `/health` `{"protocol_version":1,"status":"ready","version":"0.1.0"}`. `$B/api/state` returned 404 (control routes absent from bootstrap) and `$C/api/state` without a bearer returned 401.
3. **Project + seed.** `dacs request --file` with `create_project` (`target_branch: main`). A fixed-date seed commit `f4d5fb0c243c2e482ed3d6825cea4e42883073bf` was bundled and imported: `dacs source import --project <id> --file seed.bundle --seed`, giving `target_head` = seed.
4. **Invitation and onboarding.** `create_invitation` for host `dacs-e2e-coordinator`. `POST $C/api/onboarding/inspect` (admin bearer, loopback, local setup) ran as a job and **failed**: `conflict: Invitation inspected, but network infrastructure/disclosure is unavailable. Configure the coordinator bridge and inspect again before registration`.
5. **Enrollment.** `GET $B/api/invitations/<token>`, then `POST $B/api/enroll` with a `WorkerRegistration` (`os: macos`, `architecture: arm64`, `capabilities: ["git"]`, capacity 1 agent slot / 4000 mCPU / 8192 MB / 20480 MB, `omp_version: 0.0.0-dacs-fake-omp`, `owner_consented: true`). Result: worker pending, `approved=false`, `draining=true`. With that device token, `GET $C/api/state` returned **403** until `approve_worker`.
6. **Worker.** `dacs worker run --config <run>/worker/worker.json` with `omp_executable` = `fake-omp`, `ca_certificate` = `$CA` and `poll_interval_ms: 500`. The worker checked `fake-omp --version` against the pin and ran the (fake) readiness turn with `--no-tools`. It then heartbeated, reported runtime status and sent `SetDraining{false}`. `dacs worker status`: `running=true, draining=false, provider_authenticated=true (fake), elevated=false, desktop_supported=false`. The fake recorded the managed overlay the worker passed (`async/launch/task recursion/eval/computer disabled`, `update.auto: false`).
7. **Task 1.** `create_task` (agent execution, `effect_class: workspace`, `required_capabilities: ["git"]`, 1 agent slot, lease 300 s) then `admit_task` placed attempt `c8046aad…` (generation 1) on the worker. Its `assignment` was control message sequence 1. Observed coordinator events, in seconds from project creation:

   | t | Event |
   |---|---|
   | 2 s | `attempt_admitted` (reservation taken) |
   | 3 s | worker `started` (attempt `executing`) |
   | 4 s | worker `settled` (after fake `agent_end` and `prompt_result sessionSettled`) |
   | 5 s | `candidate_submitted`: commit `73b765e141ef32301feb64456eda48fd546d854c` with fake check evidence bound to that SHA |
   | 7 s | worker `stopped`: OMP process group terminated and reservation released (worker `reserved` all zero); attempt stays `result_submitted` |
   | 8 s | `POST /api/candidates/<id>/prepare` → `integration_sha` = candidate (fast-forward); `candidate_staged` |
   | 8 s | `review_candidate` approve with a reviewer check the harness actually ran: `git --git-dir <bare> show <sha>:greeting.txt` matched |
   | 9 s | `POST /api/candidates/<id>/publish` → `published`; task `accepted`, `integrated_sha` = `73b765e1…`; `refs/heads/main` in `coordinator/source/projects/<id>/repository.git` = `73b765e1…` |

   Workspace: `workspaces/<attempt>` is a separate `git clone --no-checkout --no-hardlinks` of the coordinator source bundle on branch `dacs/<attempt>`. It has its own object store (no alternates), `.dacs-source-pin` = seed, a clean status after commit, and HEAD = candidate. `dacs source export` followed by `git clone -b main` gave a tree containing `greeting.txt`.

## Crash drill A: replayable workspace attempt (kill -9 mid-attempt)

Task 2 (base `73b765e1…`, `effect_class: workspace`). The fake wrote `recovery.txt`, left it uncommitted and held, with a `sleep 300` child, as if inside a long tool call.

| Observation | Value |
|---|---|
| Before kill | `dacs worker status` `active_attempt_ids=[c4c8f298…]`; one process receipt in `state/processes/`; workspace `?? recovery.txt`; group: fake OMP (ppid = worker) + `sleep 300` |
| `kill -9 <worker>` | Fake OMP and its child survived as an orphaned group, ppid 1 |
| `dacs worker status` 17 s later | `running=false` (observation older than 15 s), last `active_attempt_ids` retained |
| `dacs worker run` restart #2 | Startup `reconcile_previous` killed the orphaned group (fake OMP and `sleep` both gone, `ps` empty) and the attempt resumed. Coordinator: no new attempt or generation (1 attempt, generation 1); events `started(1) → progress "Reattached durable OMP attempt after local process reconciliation"(2) → settled(3) → stopped(4)` |
| Resumed OMP session (fake trace) | `open_session` received; prompt contained the restart notice; `dacs_pending` returned the same attempt (`state: executing`, `last_sequence: 1`); edit found already present and **not repeated**; committed and submitted |
| Result | Candidate `e5735afcc8464709425e34f0b2f5ae3d2278bb18` prepared, reviewed and published; task `accepted`; reservation released |

The worker was down about 17 s, within the 300 s lease. A lease expiry during downtime was not exercised here.

## Crash drill B: external-effect attempt (kill -9 mid-attempt)

Task 3 (base `e5735afc…`, `effect_class: external`), held the same way with `external.txt` uncommitted. After `kill -9` and restart #3, the orphaned group was killed and the worker **did not relaunch OMP** for the attempt (one session in its trace). It reported `failed: "Worker restarted during external-effect execution; automatic replay is forbidden. Inspect effects and explicitly abandon/reassign."` and then `stopped` (sequences 2, 3). Task `failed`, reservation released, no candidate, `refs/heads/main` unchanged. The workspace was kept with `?? external.txt` for owner inspection.

## Drain and shutdown

`dacs worker control --data-dir <run>/worker/state --action drain`: the worker exited 0 by itself. Status was `running=false, draining=true` and the coordinator showed the worker `draining=true`. `state/processes/` was empty and no fake-OMP process remained. `dacs serve` exited 0 on SIGINT.

## Integrated Git revisions

`refs/heads/main` of the coordinator's bare repository after the run:

```text
e5735afcc8464709425e34f0b2f5ae3d2278bb18 DACS fake OMP <fake-omp@dacs.invalid> fake-omp: write recovery.txt
73b765e141ef32301feb64456eda48fd546d854c DACS fake OMP <fake-omp@dacs.invalid> fake-omp: write greeting.txt
f4d5fb0c243c2e482ed3d6825cea4e42883073bf DACS e2e seed <seed@dacs.invalid> Seed single-host e2e fixture
```

Both integrations were fast-forwards (integration SHA = candidate SHA). Seed and fake commits use fixed identities and dates, so these SHAs are reproducible run to run (observed identical in all three runs).

## Gate table

| Gate | Result | Observation |
|---|---|---|
| `dacs tls` dev certificate | passed | SAN `DNS:localhost, IP:127.0.0.1` |
| Coordinator TLS control + bootstrap listeners | passed | both `/health` over TLS |
| Bootstrap listener omits control routes | passed | `$B/api/state` → 404 |
| TLS trust is explicit | passed | no-CA request refused |
| Unauthenticated state read refused | passed | 401 |
| Project created and target seeded | passed | `main` = `f4d5fb0c…` |
| Owner onboarding inspect → register → configure_worker | **blocked** | `conflict: … network infrastructure/disclosure is unavailable …` (no netadmin) |
| Enrollment via TLS bootstrap `/api/enroll` | passed | pending, `approved=false`, `draining=true` |
| Unapproved device cannot read project state | passed | 403 |
| Owner approval (`ApproveWorker`) | passed | |
| `dacs worker run`: heartbeat, runtime status, undrain | passed | `provider_authenticated=true` is **fake** |
| Task admitted (dispatch + reservation) | passed | attempt generation 1 on this worker |
| Assignment delivered; worker reported Started | passed | |
| Explicit `dacs_submit` → `CandidateSubmission` | passed | twice (tasks 1, 2) |
| Settled + Stopped reconciled, reservation released | passed | twice |
| Prepare → review → publish into bare repo | passed | twice; `main` advanced to exact staged SHA |
| Isolated workspace clone per attempt | passed | own object store, `dacs/<attempt>` branch, source pin |
| Authenticated source export contains integrated change | passed | |
| Crash A: worker killed, OMP group orphaned | passed | ppid 1 |
| Crash A: `dacs worker status` after crash | passed | `running=false` after 15 s staleness |
| Crash A: restart reconciles orphaned group | passed | group killed before resume |
| Crash A: same attempt/generation resumed, not replaced | passed | `Reattached` progress event |
| Crash A: resumed session uses `open_session`, edit not repeated | passed | fake trace |
| Crash B: worker killed, OMP group orphaned; restart kills it | passed | |
| Crash B: external attempt not replayed | passed | `failed` + `stopped`, no candidate, `main` unchanged |
| Owner drain: worker exits after reconciling | passed | exit 0 |
| No supervised process receipts / fake OMP processes remain | passed | |
| Coordinator graceful shutdown (SIGINT) | passed | exit 0 |
| Real provider-backed OMP turn or submission | not run | stand-in only; see the worker handoff for earlier provider evidence |
| Network enrollment, consent, grant, join (netadmin/Headscale/Tailscale) | not run | |
| Multi-PC dispatch, remote workers, compiler placement | not run | single host |
| Lease expiry during downtime → fencing / replacement admission | not run | downtime stayed within lease |
| Coordinator crash/restart; publication crash between ref update and SQLite | not run | |
| Delegation/child tasks, mailbox, workflow tasks, cancellation, cost cap | not run | |
| Native desktop capability | not run | `desktop_supported=false` |
| Windows/Linux | not run | macOS only |

## Findings and requests

For Person 1 (`dacs-app`, `dacs-source`):

1. **Onboarding cannot configure a worker without a network contract.** `POST /api/onboarding/inspect` against a loopback coordinator with no `--network-config` fails with `conflict: Invitation inspected, but network infrastructure/disclosure is unavailable. Configure the coordinator bridge and inspect again before registration` (observed). By code reading, not run: `register` then fails at `current_contract`. `configure_worker` needs that registration and a contract, and takes `coordinator_url` from `contract.coordinator.overlay_url` (`crates/dacs-app/src/onboarding.rs`). The README describes loopback development, but the only working single-host path is the raw `/api/enroll` + `approve_worker` + hand-written `WorkerConfig` used here. Request one of two fixes. (a) Add a loopback-only development path where, with no bridge configured, `register` enrolls without network consent and `configure_worker` uses the inspected endpoint as `coordinator_url`, clearly labelled as no network membership. (b) Document the raw path as the supported development route.
2. **An unconfigured network bridge logs an ERROR every 5 s.** `dacs::server: Network maintenance failed: network infrastructure unavailable; any pending operation remains unresolved` (`server.rs` maintenance loop calling `network.maintain()`) appears even when `--network-config` was never given. Request: skip or log once when unconfigured.
3. **The export bundle has no HEAD.** `export_bundle` bundles only `refs/heads/<target>`. A plain `git clone <export>` checks out nothing unless the client's `init.defaultBranch` equals the target branch. It works on stock macOS only because Apple's system gitconfig sets `main`. With `GIT_CONFIG_NOSYSTEM=1` the checkout was empty. Request: document `git clone -b <target>` or include a HEAD reference.
4. Minor: no CLI subcommands exist for `/api/enroll` or `/api/candidates/{id}/prepare|diff|publish`, so the harness uses `curl`.

For Person 2 (`dacs-worker`), observations only (no change made): `dacs worker run` writes nothing to stdout/stderr in normal operation, even with `RUST_LOG=dacs_worker=info`. Liveness is visible only through `dacs worker status`. The coordinator treats `provider_authenticated` as the worker's self-report of the readiness turn. Whoever controls the configured OMP executable controls that signal, as this run demonstrates with a stand-in.

## Limitations

- One physical host, loopback addresses, a development certificate trusted explicitly, and coordinator and worker sharing the machine. No NAT, overlay or remote paths.
- The OMP stand-in proves the RPC framing, host-tool routing, explicit submission, settlement handling and session reattachment as DACS implements them. It does not prove real OMP compatibility, provider behaviour, cost caps or agent quality.
- The reviewer check ran in the harness against the exact staged SHA. It is not an independent CI system.
- Both crash drills interrupt the worker while OMP holds before commit. Crashes during upload, between submission staging and delivery, or during publication were not exercised.
