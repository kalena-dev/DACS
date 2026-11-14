# Person 3: private network and enrollment

## Implementation handoff

The implementation is in `C:/Users/PC/Documents/GitHub/DACS`, shared branch `main`. `net/` contains the network contract, netadmin service, device client and deployment configuration. Coordinator authorization, separate TLS bootstrap/control listeners and owner onboarding are connected. The private task-board frontend is in `examples/task-board-web/`; `dacs task-board` serves its compiled assets and persistent backend on loopback.

Verified: 14 network-client tests passed. The redesigned Rust/Leptos frontend passed its WebAssembly release build, 11 coordinator UI tests and the native application build. Real browser checks covered dashboard login/navigation, setup and candidate-review layouts, and task-board create/edit/status/filter/delete, keyboard operation and persistence through backend restart. Dashboard, setup, onboarding, network, compiler-pool and task-board pages fit a 390px viewport without horizontal overflow. The baseline combined worker/UI/application check passed 62 tests. These were local checks; no four-agent construction run is claimed.

Use repository `net/README.md` for infrastructure deployment and `docs/team-integration.md` for endpoint/authentication contracts. Control/bootstrap ports are 7443/7444; netadmin is 9443; compiler defaults are 10600/10501. Headscale administration credentials stay on the infrastructure host.

Still unverified: public DNS/TLS/DERP, real fleet enrollment/revocation, explicit switching of an existing tailnet, and direct/relay compiler reachability. These require reachable infrastructure and consenting devices. No production network was joined by these checks. The original acceptance requirements below remain required.

Read `00-general-plan.md` first. Own network infrastructure and network enrollment. Person 1 owns coordinator authorization, UI and installer code; Person 2 owns worker execution; Person 4 owns compiler/build adapters and release artifacts.

The event window is fixed at 210 elapsed minutes. Hardware, DNS, TLS, credentials and owner permissions are real dependencies. Record missing inputs during the first 15 minutes and continue reachable work without claiming unavailable infrastructure exists.

## Ownership and API boundary

Own Headscale/Tailscale/DERP setup and verification, the network enrollment adapter, device-network credentials, overlay reachability, network revocation and existing-tailnet consent/status. Implement DACS-owned adapter code in Rust. Reuse upstream network software.

Person 1 decides project/device authorization and exposes the bootstrap-facing application API. Your adapter performs network operations only after that authorization and the owner's consent. Person 1's UI shows your statuses and approval requirements. Do not add another project database, invitation-authority service or installer engine.

Keep Headscale administrative credentials off all agent PCs, including the coordinator. The existing infrastructure must hold them, with a protected path for approved enrollment operations. If hardware forces Headscale onto an agent PC, obtain and record the accepted risk rather than calling it isolated. Do not place administration credentials in invitation links or worker configuration.

## Required implementation and checks

1. Verify a reachable Linux/BSD server, DNS/TLS and self-hosted relay configuration. A native application install cannot make a residential coordinator publicly reachable automatically.
2. Make the initial HTTPS bootstrap reachable before the device joins the overlay. A reusable invitation is revocable and carries no permanent shared device/admin credentials.
3. After named-host consent and individual host approval, issue unique device/network enrollment material and establish the worker's normal authenticated connection. Installation, project authorization and network membership remain separate states.
4. Keep application-layer TLS/device authentication on control traffic; overlay membership is not identity proof. During development, any routable authenticated coordinator address is acceptable so Persons 1 and 2 need not wait for you. The final demo uses the overlay.
5. Verify compiler client-to-scheduler, client-to-server and scheduler-to-server connectivity over the private overlay. Do not expose compiler endpoints publicly. Give Person 4 stable overlay addresses/ports for server registration and address-bound tokens.
6. A Linux VM or other compile environment joins as its own reachable overlay node. Host NAT or merely installing the host's client does not make that guest reachable.
7. Detect an existing Tailscale control-server login. Explain that joining DACS displaces it and require approval; never switch silently. Provide the consent/state contract to Person 1's setup UI.
8. Verify direct and relayed paths, revocation and reconnect behavior. Report which path a build used and measured transfer/availability behavior. Do not promise relay throughput or build speedup.

Opening an invitation only displays it. No network change occurs until the owner accepts the named host and its administrator-level access disclosure. The public installer grants no host access by itself.

## Handoffs

- Person 1: enrollment operation/status contract, local-approval requirements, reachable bootstrap/coordinator addresses, revocation outcomes and prerequisite blockers. Keep project approval in its coordinator.
- Person 2: device-specific enrollment result and reachable coordinator address, without provider or administrative credentials.
- Person 4: stable compiler-node addresses, confirmed reachability, direct/relay status and token/address constraints. Its stock compiler proof depends on your overlay being ready.
- Person 1's setup agent: reviewed network-client prerequisites and explicit tailnet-switch/service disclosures; do not modify its installer files yourself.

## Schedule

| Time | Your work |
|---|---|
| 00:00-00:15 | Check public server/DNS/TLS/admin access, relay requirements, existing tailnets and ownership of credentials. Agree the enrollment boundary. |
| 00:15-00:50 | Prove the actual overlay and private reachability by hand. Unblock Person 4's stock distributed compiler proof. |
| 00:50-01:50 | Implement approved network enrollment/revocation around the proven mechanism; expose real state and consent requirements. |
| 01:50-02:15 | Complete invitation-to-network readiness on the fleet, including independently reachable Linux compile guests. |
| 02:15-03:00 | Run enrollment/direct/relay/revocation acceptance. Through DACS, implement the private task-board's Leptos frontend against Person 2's API. |
| 03:00-03:20 | Fix observed networking/integration failures and rerun affected checks. |
| 03:20-03:30 | Deliver adapter commit, network configuration/evidence and unresolved infrastructure limits without secrets. |

## Acceptance and proof contribution

Demonstrate that public installation and invitation opening do not join a network. Show named-host acceptance, individual device approval, unique credentials, explicit tailnet switching and successful authenticated worker connection. Verify revocation and the final overlay route on real hosts.

Retain evidence of private compiler reachability and the direct/relay paths Person 4 uses. Missing reachable infrastructure is a failed/not-run gate, not a reason to fake network enrollment.

For the four-agent demonstration, own the task-board frontend in its own workspace: client-side Rust/Leptos assets served by Person 2's Axum backend, with no SSR/hydration build. Provide create/edit/delete, title/description, Todo/In progress/Done, text/status filtering, keyboard access and a narrow viewport. Person 4 checks the actual browser flow, including persistence through backend restart. Keep the preview private.

Submit commits, contract revision, observed outcomes and configuration references to Person 1. Do not put credentials in logs or handoff documents. Run narrow checks after coherent changes; Person 1 owns integration and combined builds.
