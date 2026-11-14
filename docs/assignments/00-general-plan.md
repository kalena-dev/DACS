# DACS: general four-person delivery plan

## Use and deadline

This is the shared plan for a four-person team building DACS with OMP agents. Read it alongside your numbered assignment. Person 1 is the owner with greater AI capacity and has a deliberately larger implementation track. The other three tracks remain independently owned.

The hackathon window is fixed at 210 elapsed minutes, including setup, downloads, approvals and checks. The full scope remains required. Deliver working increments early, and report every mandatory gate as passed, failed or not run at the deadline. Do not hide incomplete scope behind a demo or extend the clock implicitly.

Technical source: `C:\Users\PC\Downloads\message.txt`. That file is unchanged. These files define the team's new work allocation and retain its technical requirements. No application implementation or successful benchmark is supplied by these documents.

## Product and architecture

DACS coordinates OMP agents across 3-10 known teammates' PCs. It distributes agent work, workflow jobs and eligible invocations from one Cargo build, not model inference. DACS-owned application code is Rust. Windows, Linux and macOS application support, native desktop automation and universal setup remain required.

Use one Rust coordinator with local SQLite, a coordinator-local bare Git integration repository and a local artifact directory. Workers communicate through authenticated TLS APIs; they never access the database file directly. External-forge mirroring happens after local publication and acceptance.

Use one Rust worker application per physical host, with a durable journal, isolated workspaces and supervised OMP processes. The coordinator PC also runs the worker and contributes agent work. Account for VMs against their physical hosts.

Share a Rust/Leptos UI between the browser and native shell. Reuse OMP for provider login, models, ordinary tools and desktop control; Headscale/Tailscale for networking; Cargo/rustup and package managers for dependencies; sccache for remote compiler scheduling. No separate broker, PostgreSQL or object-storage service is required.

## Ownership

| Person | Implementation ownership | Boundary |
|---|---|---|
| 1: higher-capacity owner | Shared contracts and integration; coordinator/admission/messages/storage/Git publication; dashboard/native shell/thin CLI; installer engine/catalogue, dependency planning, invitation UX and update client | Uses several disjoint agent workstreams. Owns shared types and workspace-level files. Does not own worker execution, network infrastructure or compiler adapters. |
| 2: worker and OMP | Worker journal, RPC adapter, asynchronous host-tool routing, workspaces, lifecycle, reservations, owner-identity elevation and desktop controls | Supplies process supervision and local-control APIs to Persons 1 and 4. |
| 3: network and enrollment | Reachable bootstrap/network services, Headscale/DERP, device-network enrollment adapter, overlay connectivity, revocation and tailnet-switch handling | Person 1 owns project/device authorization and UI; Person 3 owns network operations and network credentials. |
| 4: builds and release | Build/test runners and sharding, sccache pools/daemons, toolchain profiles, compile evidence, native build/package validation and controlled signing workflow | Person 2 supervises processes. Person 1 writes installer/update logic; Person 4 produces and verifies platform release artifacts. |

More AI capacity is used on disjoint code, not several agents editing the same files. Extra usage does not grant access to someone else's account, signing keys, desktop or machine. Each owner confirms their provider plan permits the intended automation.

## Contracts and integration

Person 1 publishes the initial shared Rust types and wire contract by minute 15 after collecting each owner's needs. Include assignment/event identity, attempt generations, effect class and lease duration, pinned source/policy/contract revisions, reservations, asynchronous job/delegation IDs, candidate/publication records, capability and consent states, requirements manifests and release metadata.

Agree file ownership, including database migrations and the workspace manifest/lockfile, before parallel edits. Person 1 owns the delivery branch. Submit small working commits with the contract revision, observed checks and open issues. Other owners request shared changes rather than forking the schema.

The first integration checkpoint uses real device credentials, TLS, dispatch, one OMP run, result submission and local Git publication on another PC. Until the overlay is ready, control-plane development may use any routable authenticated coordinator address. The final demonstration uses the overlay. Person 3's network readiness is a real prerequisite for Person 4's remote compiler proof; compiler traffic stays private throughout.

Use existing OMP installations to build DACS. Do not require the unfinished product to orchestrate its own implementation. The final acceptance workload must use the completed DACS path.

## Execution rules to preserve

- Every scheduled child is a DACS task. Disable native spawning paths in managed OMP profiles. Delegation and long-running job tools return an admission/refusal and identifier promptly; results arrive later through durable messages.
- Persist pending children independently of a live RPC call. An OMP session can be settled while its DACS attempt is unfinished. Quietness never substitutes for an explicit result and completed obligations.
- Reserve runnable child/job capacity before letting a parent wait. Refuse when unavailable. Keep waiting-parent reservations and account for all physical-host allocations without double counting.
- Classify effects explicitly. After lease expiry, replayable workspace-only work may get a new fenced attempt. External/host-level effects retain uncertain ownership until explicit human abandonment. A missed heartbeat alone is never a retry trigger. Hold an unreachable worker's reservations until reconciliation; stop its fenced attempt when it reconnects.
- Serialize cancellation/invalidation with the transaction entering Git publication. Publish with an expected-head check, reconcile the local bare ref after a crash, and record the integrated revision before releasing dependants. Keep unresolved publication intents blocking retries/reassignment.
- Keep owner OMP credentials local. Respect existing OMP cost caps and report cost-capped sessions distinctly. DACS imposes no additional spend cap and must not describe extra provider accounts as a hardware speedup.

## Consent and installation

Public installation and merely opening an invite start no worker, join no network and grant no host access. Review names the host and explains administrator-level access. Installation/enrollment/execution follow explicit local consent and individual host approval.

Default execution uses the owner's identity with elevation available, retaining their profile and interactive desktop. Use the owner's elevated Windows token; on Linux/macOS, disclose and obtain consent for any removable non-interactive escalation rule. If elevation is unavailable, report that capability instead of switching accounts. Restrictions in elevated mode are advisory, not a sandbox.

Keep Headscale administration credentials and application-update signing keys off PCs running agents. If hardware forces Headscale onto an agent/coordinator PC, record the explicit accepted risk. Do not silently switch an existing Tailscale login: joining another control server displaces it. Each Linux compile guest joins as its own reachable overlay node.

Every enrolled host/client requires pinned OMP and the network client. Provider login is manual before agent work. Users choose optional tools. Existing-project requirements come from a read-only evidence-backed OMP scan; new projects use host presets/package selection. Map requirements to a reviewed, data-driven catalogue and show installed/missing/incompatible tools before approved automatic installation. Cargo manages project crates.

The required catalogue categories are Rust toolchains, rustfmt/Clippy, Rust web tooling, Git, debuggers, editors, browsers and databases. These are user-selectable options, not mandatory installations on every device. Implement install/detect/repair/update operations and at least one verified entry for each category on each supported platform. Do not silently omit categories to obtain a pass. Changed requirements require a visible approved diff. Custom scripts need explicit review and have no generic rollback guarantee.

Workers launch manually; ordinary close and permission reduction drain admitted work. Keep emergency cancellation separate. Show active desktop control and a local pause action. Application updates require authenticated compatible artifacts and state reconciliation; dependency upgrades stay owner-controlled. Signed packages use existing identities. An unavailable identity produces a labelled unsigned preview and a failed signing requirement, not a passed release gate.

## Opening checks

Check by minute 15: actual repository and fleet; pinned OMP/login on agent machines; Windows/macOS/Linux desktop and SDK access; reachable Linux/BSD bootstrap/Headscale/relay service; Linux build initiator plus two other physical PCs with reachable Linux environments; a real workspace with independent eligible library crates; existing signing/notarization access. Missing inputs are named immediately with affected gates and an owner. The clock keeps running.

## Shared schedule

| Time | Required checkpoint |
|---|---|
| 00:00-00:15 | Check inputs, allocate files and publish shared contracts. Person 1 starts its independent software workstreams; all other owners start their real mechanisms. |
| 00:15-00:50 | Persons 1 and 2 demonstrate one real remote OMP assignment through candidate publication. Person 1's UI reads real state. Person 3 establishes the overlay. Person 4 proves a stock sccache build with both remote servers before automating it and produces platform package builds. |
| 00:50-01:50 | Merge working increments continuously. Add coordinator/runtime recovery, durable delegation, network enrollment automation, managed compiler pools, installer/catalogue, dependency planning, invitation review and updates. |
| 01:50-02:15 | Complete invitation-to-readiness and assignment-to-publication across at least three PCs. Attach the reserved compiler pool through DACS admission. No simulated backend state qualifies. |
| 02:15-03:00 | Run the four-agent demonstration and mandatory acceptance in parallel. Capture revisions, real desktop/browser observations, compile placement and platform/setup outcomes. |
| 03:00-03:20 | Fix observed failures within ownership boundaries; integrate and rerun affected checks. Never weaken an assertion or change the workload just to pass. |
| 03:20-03:30 | Check final integrated artifacts and report each gate with its evidence, failures and unrun cases. |

Use narrow checks after coherent changes. Person 1 owns the combined build after integration; other owners do not repeatedly run the whole suite. Report blockers as they occur and continue reachable work. No scope reduction or deadline change is implicit.

## Demonstration and completion

Use a private Rust/Axum task board with redb persistence and a client-side Leptos UI served as static assets. Avoid adding SSR/hydration. Four useful implementation sessions must overlap across at least three physical PCs: Person 1 contributes preview packaging while coordinating integration, Person 2 implements backend/persistence, Person 3 implements the frontend, and Person 4 supplies browser acceptance/build evidence. Reserve any separate lead session too.

Through the actual UI: create, edit, change status, filter, restart and confirm persistence, then delete. Include title/description, Todo/In progress/Done, keyboard access and a narrow viewport. Observe native desktop interaction separately.

The compilation proof uses one native Linux Cargo build, a cold result cache, clean outputs and disabled incremental compilation. Both remote physical PCs must execute distinct eligible invocations whose outputs that build consumes. Ordinary local checks stay local. A cache hit, separate builds or the WebAssembly frontend do not replace this proof. Report worker-removal behavior, timings, cache state, transfer costs and local fallback without inventing acceleration.

All gates remain mandatory: feasibility; execution foundation; agent collaboration; admission/publication ordering; integration/compute; failure/correctness; supported release. Include replayable lease replacement, effectful abandonment, stale-result rejection, pending delegation surviving restart, cancellation/publication races, unchanged owner sccache daemons, consent, installer coverage and platform signing status. The website alone does not complete the product.
