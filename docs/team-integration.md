# Team integration checkpoint

Shared branch: `person-1-expanded-track`. This is an integration branch, not a release. It incorporates the worker/runtime foundation, build-pool contribution, release-evidence tools and `person3/network-enrollment` history. Use the root Cargo workspace and lockfile.

## Parallel ownership

| Owner | Work now | Owned files |
|---|---|---|
| Person 1 | Coordinator, shared wire contract, network bridge, setup/dashboard, application entry points and final merges | Root Cargo files, `dacs-protocol`, `dacs-core`, `dacs-app`, `dacs-source`, `dacs-setup`, `dacs-ui` |
| Person 2 | Connected worker loop over the existing journal, OMP adapter and supervisor; workspace/source transfer, durable reporting, host tools, cancellation/drain and local-control state | `crates/dacs-worker/`, except the two build modules below |
| Person 3 | Infrastructure deployment, bootstrap routing, TLS, real enrollment/revocation, tailnet-switch behavior and fleet reachability | `net/` (its former nested workspace is now integrated at the root) |
| Person 4 | Actual supervised build execution, output/action evidence, isolated compiler pools, platform packaging and release evidence | `crates/dacs-worker/src/build_execution.rs`, new `build_runner.rs`, `person-4/builds-release/` |

Person 2 owns changes to `ManagedChild` and `Supervisor`. Person 4 requests the needed capture/environment/process-lifecycle interface from Person 2. Person 1 owns shared DTOs and root dependencies. Request a concrete addition rather than making another protocol. Push small commits on your own branch from this checkpoint; report the commit SHA and commands actually run.

## Available coordinator API

`crates/dacs-protocol/src/lib.rs` is authoritative; `PROTOCOL_VERSION` is 1. JSON enum variants use snake_case. API clients use `Authorization: Bearer <device token>` with TLS verification enabled. Local HTTP is allowed only for loopback development.

| Method | Path | Contract |
|---|---|---|
| GET | `/health` | Service health |
| GET | `/api/invitations/{token}` | Invitation display; no enrollment side effects |
| POST | `/api/enroll` | Invitation plus `WorkerRegistration`; returns `Enrollment` with pending worker and device token |
| GET | `/api/state` | Authorized `Snapshot`; pending/unapproved devices cannot read project state |
| POST | `/api/command` | `CommandEnvelope { protocol_version, command_id, command }`; replay the same ID and body after ambiguous transport failure |
| GET | `/api/commands?after=0` | Durable recipient-specific `ControlMessage` queue; persist receipt before `AcknowledgeMessages` |
| GET/POST | `/api/projects/{id}/source` | Git bundle transfer; initial seed is administrator-only |
| GET/POST | `/api/projects/{id}/artifacts/{digest}` | SHA-256-addressed artifact transfer |
| POST | `/api/candidates/{id}/prepare` | Prepare exact integration SHA |
| GET | `/api/candidates/{id}/diff` | Authorized actual candidate diff |
| POST | `/api/candidates/{id}/publish` | Publish an approved exact integration SHA |

Do not log the enrollment response or device token. Persist credentials in the owner's private local state.

Worker admission requires individual approval, a non-draining worker, compatible declared capacity/capabilities and a reported provider-ready state for agent slots. Registration, approval, provider login, network membership and readiness are distinct. Heartbeats must not silently widen owner consent.

Use `ReportEvent` with monotonic per-attempt sequences and the assigned generation. `agent_end` is not OMP settlement. Preserve child obligations across restart. `Stopped` means process termination was confirmed; neither disconnection nor an abort response proves that. Upload source and submit `CandidateSubmission`; publication also requires positive settlement and exact-SHA review. Reservations remain held until stop reconciliation.

Person 2: expose the connected runtime as a library entry point and send its configuration schema to Person 1 for the `dacs` CLI/local UI. Keep the contributed OMP adapter and journal; do not build a second runtime. Person 4: send the admitted-build request/result schema before adding shared fields. Generic build scripts are host-effectful unless an enforced sandbox proves otherwise.

## Network handoff

Confirmed integration ports:

- Coordinator control: TLS tcp/7443.
- Coordinator bootstrap: TLS tcp/7444 through the existing infra SNI passthrough.
- Netadmin: TLS tcp/9443 on the infra overlay address.
- Compiler scheduler/server: retain the contributed defaults tcp/10600 and tcp/10501 pending actual engine configuration.

Person 1 is adding the separate bootstrap listener and coordinator-to-netadmin bridge. The application currently has a configurable single `--listen` address; do not claim both ports are active before that change lands. Keep Person 3's ACL and upstream layout.

Use `dacs-net-contract` revision `dacs-net/2026-11-14.1` and disclosure `host-admin-access/1`. The netadmin API and coordinator-first bootstrap sequence are documented in `net/README.md`. Person 3 can deploy and verify those independently of the complete dashboard/worker. Headscale API keys remain on the infrastructure host. The coordinator receives only its scoped operation token through a private file.

The contributed evidence records successful isolated Headscale/Tailscale checks. Public server/domain/DNS, public TLS/DERP, real fleet paths and the full device-side join are still unverified. Supply endpoint URLs, CA certificate paths, and private credential-file locations through the appropriate private channel; never commit credentials or signing keys.

## Focused checks

Run from the repository root after a coherent change:

- Worker: `cargo test -p dacs-worker`.
- Networking: `cargo test -p dacs-net-contract -p dacs-netadmin -p dacs-netclient`.
- Build/release evidence: `cargo test -p dacs-release-audit`.

Person 1 runs the combined verification after integration. Fixture tests do not establish a running fleet, actual provider authentication, distributed compilation or platform signing. Preserve failed and unrun gates in each handoff.
