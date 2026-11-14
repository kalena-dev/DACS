# Team integration checkpoint

Shared implementation: `main`. The integration branch `person-1-expanded-track` is retained. This is an integration checkpoint, not a certified release. It incorporates the worker/runtime foundation, build-pool contribution, release-evidence tools and `person3/network-enrollment` history. Use the root Cargo workspace and lockfile. The assignment handoffs and shared requirements are in [`assignments/`](assignments/).

## Current implementation handoff

All three tracks now have connected implementations in this workspace:

| Track | Delivered implementation | Evidence and remaining limits |
|---|---|---|
| Worker/OMP | Connected runtime, durable journal/outbox, isolated source workspaces, host tools, supervision, workflow delivery and local controls | Real provider-backed OMP submission and publication passed. Windows checkout, native workflow and JobObject cleanup regressions passed. The final integrated worker smoke passed provider readiness, native compilation/execution, immutable result acceptance, process stop, reservation release and owner-requested drain. See `assignments/02-worker-runtime.md` for evidence IDs and remaining multi-PC, pending-child recovery and native desktop requirements. |
| Network/enrollment | Network service/client/deployment files, coordinator bridge, separate TLS listeners, owner consent/onboarding and private task-board frontend | 14 network-client tests passed. The redesigned frontend passed its WebAssembly release build and browser CRUD, combined filters, keyboard, 390px layout and backend-restart persistence checks. Public infrastructure, real fleet join/revocation and compiler routes remain unverified. |
| Builds/release | Admitted build/test/service runner, compiler pool ownership, authenticated action provenance, release audit and native packaging commands | Four focused packaging tests passed. A clean Windows x64 native release at `13bc70b` produced an unsigned MSI; extraction verified the executable and both required MSVC runtime DLLs. See `person4-build-release-status.md` for its checksum and the unrun installation, multi-host, other-platform and signing requirements. |

The baseline combined command was `cargo test -p dacs-worker -p dacs-ui -p dacs --features dacs/native && cargo build -p dacs --features native`. The HTTP/CLI source-publication smoke also passed, including final stop reconciliation, artifact authorization and request replay.

The frontend checkpoint at `3515a8b` uses neutral dark surfaces, compact navigation and flat task rows. `cargo test -p dacs-ui --features native` passed 11 tests; the native application and task-board release builds passed. Browser checks covered dashboard login/navigation, setup and candidate-review layouts, task-board editing/status/filtering, keyboard focus, persistence across backend restart and deletion. Dashboard, setup, onboarding, network, compiler-pool and task-board pages fit a 390px viewport without horizontal overflow. No package installation or network enrollment was performed by these UI checks.

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

The application accepts `--listen`, optional TLS-only `--bootstrap-listen`, and `--network-config`. Control and bootstrap bind before readiness; failure to bind either aborts startup. Bootstrap exposes health, invitation/enrollment and device-scoped network operations, without browser sessions, administrator commands, source/artifact access or local setup. Both listeners use the supplied certificate/key and share shutdown. Keep Person 3's ACL and upstream layout.

Use `dacs-net-contract` revision `dacs-net/2026-11-14.1` and disclosure `host-admin-access/1`. The netadmin API and coordinator-first bootstrap sequence are documented in `net/README.md`. Person 3 can deploy and verify those independently of the complete dashboard/worker. Headscale API keys remain on the infrastructure host. The coordinator receives only its scoped operation token through a private file.

The contributed evidence records successful isolated Headscale/Tailscale checks. Public server/domain/DNS, public TLS/DERP, real fleet paths and the full device-side join are still unverified. Supply endpoint URLs, CA certificate paths, and private credential-file locations through the appropriate private channel; never commit credentials or signing keys.

### Coordinator network routes

Network operations use Bearer credentials, never browser cookies. The control listener exposes:

| Method | Path | Authorization and result |
|---|---|---|
| GET | `/api/network/contract` | Public safe projection of the configured netadmin contract; unavailable without infrastructure |
| POST | `/api/network/consent` | Authenticated device's typed `HostConsent` |
| GET | `/api/network/grant` | That device's encrypted-at-rest grant; pending/uncertain state returns a conflict |
| GET | `/api/network/state` | Safe device-scoped or administrator snapshot |
| POST | `/api/network/devices/{id}/refresh` | Own device or administrator; records queried membership |
| POST | `/api/network/devices/{id}/revoke` | Administrator; application authorization is revoked before network revocation |
| POST | `/api/network/devices/{id}/reconcile` | Administrator; body `{"auth_key_id":123,"node_id":456}` uses audit-verified references |

Bootstrap retains contract, consent and grant routes, with device-only state/refresh. It excludes revoke and reconcile. A lost grant requires audit reconciliation, revocation and fresh enrollment; there is no blind key reissuance.

`--network-config FILE` reads `{"netadmin_url":"https://INFRA:9443","operation_token_file":"/private/netadmin.token","ca_file":"/private/ca.pem"}`; omit `ca_file` when the endpoint uses the system trust store. Tokens and grants never appear in snapshots or UI/job results.

### Local owner onboarding

`/onboarding` and POST `/ui/onboarding/{operation}` or `/api/onboarding/{operation}` require local setup mode on loopback and authenticated owner access. Operations are `inspect`, `plan`, `apply`, `register`, `consent`, `refresh`, `assess`, and `join`. Browser forms use CSRF and the shared durable Jobs queue. GET does not install, enroll or join.

The invitation's reviewed requirements object is `{"requirements_version":"1","required_packages":[{"id":"rust-stable","version":"1.98.0"}]}`. Versions are optional; IDs must come from the reviewed setup catalogue. Unknown manifests remain blockers. Registration uses explicit consent and individual coordinator approval. Joining is separate; replacing an existing tailnet needs an additional locally bound consent.

Private state is in `data_dir/onboarding/owner.json`; protect it as a credential file. An ambiguous enrollment is not retried blindly. `/setup/jobs/{id}` shows the safe operation result and links back to onboarding.

## Focused checks

Run from the repository root after a coherent change:

- Worker: `cargo test -p dacs-worker`.
- Networking: `cargo test -p dacs-net-contract -p dacs-netadmin -p dacs-netclient`.
- Build/release evidence: `cargo test -p dacs-release-audit`.

Person 1 runs the combined verification after integration. Fixture tests do not establish a running fleet, actual provider authentication, distributed compilation or platform signing. Preserve failed and unrun gates in each handoff.
