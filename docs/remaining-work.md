# DACS: remaining work

This is the consolidated handoff for finishing DACS from one computer. Implementation work is paused at this handoff. Use the existing code on `main`; do not rebuild completed components or treat every unchecked item as missing code. Most remaining items need real integration/acceptance runs and fixes for whatever those runs expose.

A single computer can host the coordinator, multiple agent sessions and isolated Linux development environments. Those runs can complete local integration, but cannot establish the original requirement for three physical PCs or two distinct remote physical compiler hosts. Keep that distinction in the final report.

Integration note: this handoff preserved incoming `main` changes through `86a1d1b`, including supervisor/crash-recovery tests, compiler-network/placement code and packaging changes. This session did not rerun those incoming changes. Check their focused tests and owner evidence when pinning the next baseline; earlier passes below apply to their recorded revisions.

## Already done: do not repeat without a relevant change

- Coordinator, worker, source/artifact transport, setup/onboarding, UI and build/release adapters are integrated. The restrained UI redesign is on `main`.
- Real provider-backed OMP execution submitted a candidate and published commit `085ae4925675b5c7b1aacc39e2f840bbc41ee37e`. Worker crash/lease fencing, stale-result rejection and replacement admission have single-PC evidence.
- The final Windows worker smoke passed enrollment/heartbeat, provider readiness, pinned native Cargo compilation, executable assertions, immutable result acceptance, process stop, reservation release and owner-requested drain. Windows Git path/LF handling, shortened workflow paths, explicit submission evidence fields and JobObject cleanup were fixed.
- Browser task-board CRUD, status/text filtering, keyboard interaction, narrow layout and persistence through backend restart passed. Later frontend-owner checks also covered focus restoration and inline title errors. This was not a four-agent construction run or native desktop proof.
- Real WSL namespace clients passed TLS netadmin access, bootstrap/device join, switch-consent checks, revocation/rejoin, compiler-port discovery/reachability, ACL denial and direct/DERP paths. Compiler ports used stand-in TCP listeners, not sccache.
- An unsigned Windows x64 MSI was produced and its executable/runtime DLL payload verified. It is pinned to `13bc70b`, before the final worker fixes: rebuild from the chosen current revision before using it for release acceptance. Installation, repair, upgrade and trusted signing were not established.
- Worker/network crate tests passed on a teammate's macOS machine. They do not establish live macOS worker, desktop or networking behavior.

## 1. Worker, collaboration and coordinator recovery

Owners/code: `crates/dacs-worker/`, `crates/dacs-core/`, `crates/dacs-source/`, `crates/dacs-app/`.

- [ ] Run a real parent/child/job workflow through DACS host tools. Admission must return an ID promptly; accepted results must arrive later through durable messages. Restart OMP while a child is pending, then restart the worker/coordinator separately. Recover pending obligations, deduplicate commands/results and wake the parent without relying on a surviving RPC call.
- [ ] Prove the settled parent remains unfinished while children/jobs are outstanding. Retain its reservation; reserve child capacity before allowing it to wait, and refuse unavailable capacity without creating an unserviceable child. Keep native OMP spawning disabled in managed sessions.
- [ ] Exercise coordinator loss during admitted work: no new cross-machine admissions or expanded reservations; retain completed results and reconcile after reconnect. Verify physical-host accounting includes all local sessions, workflow processes and any Linux guests.
- [ ] Complete the remaining lifecycle cases with actual processes: external-effect lease expiry and explicit abandonment; graceful close/permission downgrade with work still active; emergency cancellation; real cost-capped status without bypassing the owner's OMP cap. Reuse existing replay/fencing evidence rather than rerunning it indiscriminately.
- [ ] Exercise cancellation and contract invalidation at the Git publication boundary, including crashes between ref update and SQLite completion. Expected old head, intended new head and unrelated head must reconcile distinctly. Uncertain publication must block retries/reassignment and dependent admission.
- [ ] Run four useful overlapping DACS-managed sessions on this computer in isolated workspaces, including the coordinating/packaging session when applicable. Show messages/delegation, review, exact-candidate publication and a working integrated change. A single text-file proof or replay of the already completed task board is not the original construction demonstration. Record the local run honestly; the three-PC version remains a separate requirement.

## 2. Native desktop and owner controls

Owners/code: worker `runtime.rs`, `local_control.rs`, `supervisor.rs`; application onboarding and native shell.

- [ ] Prove native Windows interaction through OMP against an owned test window: capture, accessibility/input and clipboard where supported. Browser DOM automation does not count. Observe an unlocked interactive session and actual tool capability, not a configured boolean or model claim.
- [ ] Verify one exclusive desktop slot, a persistent local activity indication, local pause that stops active control, and explicit local resume. Remote messages must not grant consent or resume a paused desktop. Distinguish requested controls from observed completed state.
- [ ] Verify owner identity and elevation behavior. Keep the owner's profile; use the owner's elevated Windows token only through the OS approval flow. Advertise unavailable elevation accurately. Linux/macOS escalation rules require separate disclosed consent and must be removable.
- [ ] Launch and exercise the native shell; handle its real platform prerequisites. The Windows MSI does not bundle WebView2. Linux needs the chosen GTK/WebKit/capture environment; macOS needs its actual permissions. Certify macOS and Linux desktop interaction, including Wayland where supported, on those environments later.

## 3. Network and invitation-to-readiness integration

Owners/code: `net/`, `crates/dacs-app/src/network.rs`, `onboarding.rs`, coordinator network state and setup UI.

- [ ] Connect the actual coordinator `/api/network/*` bridge to real TLS netadmin and Headscale. The WSL evidence called netadmin directly, so it does not close this application integration requirement. Exercise both the public bootstrap listener and private control listener with their intended credentials and route restrictions.
- [ ] Complete the real UI path: inspect invitation and requirements, explicit named-host/admin-access consent, installation approval, individual device approval, network consent/grant/join, manual provider login, worker configuration/manual launch, readiness and an admitted task through publication. Opening an invite or installing the app must not start execution or join a network.
- [ ] Exercise ambiguous issuance/revocation recovery without blind retries or replacement keys. Test DACS `leave` and return to the previous personal tailnet; the recorded manual Tailscale switch-back is not that proof.
- [ ] Verify the automatic stale-DERP `restun` correction against the failure it targets. Manual `restun` cleared the observed stall, but the stall did not recur on the successful rerun, so the automatic correction remains unverified.
- [ ] Keep compiler listeners overlay-only. Re-enrollment changed overlay IPs in the real run: refresh endpoint registrations and reissue address-bound compiler credentials before reuse.
- [ ] Deploy and prove public Linux infrastructure, DNS, Let's Encrypt, the real nginx SNI router and publicly reachable DERP/STUN when an authorized host/domain is available. Then exercise fleet enrollment/revocation, NAT traversal and direct/relay paths across real networks. Local namespaces do not replace this.

For local development, use isolated consented services/guests. Do not silently switch the owner's existing Tailscale login or repurpose an unrelated server. Headscale administration keys stay off agent PCs; if co-location is unavoidable, obtain and record acceptance of that risk.

## 4. Actual distributed compilation and workflow acceptance

Owners/code: worker `build_runner.rs`/`build_execution.rs`, coordinator build ownership, `person-4/builds-release/`.

- [ ] Obtain a working native Linux environment and compatible toolchains. Prove stock sccache transport/configuration before debugging the DACS adapter. Local guests can prove mechanics but all count against this physical host. The supported instrumented engine is sccache v0.10.0, commit `e3d1ed560a28d114bd26299b0f717b372e7fc3d2`; its pinned Linux compilation is still unverified.
- [ ] Run the actual DACS-admitted scheduler/server services and initiating build. Pin source, lockfile, Cargo/rustc/sysroot, target, features, flags, environment and engine/wrapper digests; use clean outputs, a cold result cache and disabled incremental compilation. Choose real independent eligible library invocations. `cargo check`, Clippy, the WASM frontend, cache hits and separate builds are not this proof.
- [ ] Verify authenticated service telemetry, received output digests and the rustc consumption graph all correlate to the reserved workers and the final linked executable. Exercise that executable. For original acceptance, one Linux build must consume distinct uncached outputs from two other physical PCs; do not manufacture remote actions or count two VMs as those PCs.
- [ ] Verify actual process CPU/memory/concurrency enforcement, including systemd/bubblewrap prerequisites. Ledger reservations alone are not enforcement. Test pool exclusion, guest/physical-host accounting, server membership, private per-profile daemon/cache/config/port ownership and unchanged unrelated owner daemons/global Cargo settings.
- [ ] Exercise worker removal, coordinator loss, cancellation and reconciliation before pool reuse. Record retries/fallback without hiding failures. A required-distribution run must fail that requirement if its remote contributions are missing even when a local executable succeeds.
- [ ] Record measured local/cold-distributed/warm-cache comparisons, action placement, remote successes, local work, failures, elapsed time, cache state and transferred bytes. Separate artifact storage from compiler caches; do not promise speedup from loopback measurements.
- [ ] Exercise real build/test shards with independent databases, ports and temporary directories, correct application use of shard settings, bounded logs, artifact integrity and visible nonzero failures.

## 5. Setup, packaging and updates

Owners/code: `crates/dacs-setup/`, application local/setup/onboarding routes, `person-4/builds-release/`.

- [ ] Produce a current pinned Windows x64 preview containing the final runtime fixes. Test installation, launch, repair, upgrade and uninstall without overwriting an unrelated existing installation. Verify state/credential preservation, process drain/reconciliation and no automatic worker or network startup. Capture versions, checksums and results.
- [ ] Produce and test the other supported Windows/Linux/macOS architecture packages on compatible SDK/runtime environments. Native builds or extracted payloads alone do not establish install/repair/update behavior. Keep unverified platforms explicit.
- [ ] Exercise the real setup UI and engine for installed-tool reuse, approved missing installation, incompatible/unknown tools, declined capability, interrupted setup and recovery. Verify read-only evidence-backed OMP requirements for an existing project, host-selected requirements for a new project, catalogue validation and visible approval of changed requirements. Custom scripts require their own review; do not claim generic rollback.
- [ ] Complete a platform/category matrix with at least one real entry per category and detect/install/repair/update outcomes: Rust toolchains; rustfmt/Clippy; Rust web tooling; Git; debuggers; editors; browsers; databases. They remain selectable options, not mandatory installs on every device. Do not delete a category to make the matrix pass.
- [ ] Verify authenticated, platform-compatible application updates, rejection of tampered/incompatible artifacts, and recovery/reconciliation of interrupted updates before admitting new work. Dependency upgrades remain separately owner-approved.
- [ ] Obtain signed/notarized artifacts through an existing trusted signing environment and test actual update acceptance. Platform package signatures and DACS update authentication both need evidence. Never create/import signing keys into an agent/coordinator workspace to claim success. Without the required identities, retain unsigned-preview labeling and a failed/unrun signing requirement.

## 6. Final integrated delivery

- [ ] Pin the final source and artifact revisions, run the affected narrow regressions after fixes, then one combined integration check. Retain actual logs/results/screenshots and explicitly distinguish passed, failed and not-run cases.
- [ ] Keep UI state truthful throughout: provider readiness, membership, admission, active desktop control, requested versus observed pause/drain, cost-capped outcomes, pool ownership, candidate versus integrated revision and measured versus estimated usage.
- [ ] Keep publishing coherent changes to `main`, preserving teammates' commits. Update the handoff and evidence links when a requirement is genuinely closed. Do not rerun completed checks solely to reconfirm them.

## Inputs still required for the original full acceptance

| Input | What it unlocks |
|---|---|
| Three consenting physical PCs with the pinned OMP version and manual provider login | Original concurrent-agent and remote execution demonstration |
| Linux initiator plus compatible Linux environments on two other physical PCs | Original single-build distributed-compiler proof |
| Authorized public Linux infrastructure, DNS and TLS/relay configuration | Production bootstrap and real-network/NAT/DERP proof |
| Windows/macOS/Linux desktop permissions, SDKs and supported architecture hosts | Native desktop and installable-release matrix |
| Existing off-agent signing/notarization and update-signing environment | Trusted release and update acceptance |

Do not assume a teammate's WSL/macOS environment is available on this computer. A configured SSH alias is not permission to repurpose that server. Keep credentials in private files and out of this handoff.

## Existing evidence and implementation references

- [Integrated contracts and current handoff](team-integration.md)
- [Worker evidence and remaining platform limits](assignments/02-worker-runtime.md)
- [Real single-machine network run and its limits](../net/evidence/wsl-netns-e2e.md)
- [Frontend browser acceptance](../examples/task-board-web/ACCEPTANCE.md)
- [Package production, tool pins and release evidence](person4-build-release-status.md)
- [macOS platform row: builds, tests and loopback smoke](evidence/macos-platform.md)
- [Original shared requirements](assignments/00-general-plan.md)

Local proof artifacts, private tooling and MSI outputs under `.local/` are not committed and are not portable prerequisites. The runtime handoff and package-status document identify their retained evidence and exact source pins.
