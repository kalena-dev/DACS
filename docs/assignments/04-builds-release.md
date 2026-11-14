# Person 4: builds, compiler pools and release evidence

## Implementation handoff

The implementation is in `C:/Users/PC/Documents/GitHub/DACS`, shared branch `main`. Build execution and evidence collection are in `crates/dacs-worker/src/build_runner.rs` and `build_execution.rs`; pool ownership and authenticated telemetry are connected through the coordinator. `person-4/builds-release/` contains release validation, pinned sccache instrumentation, the rustc provenance wrapper and native packaging/signature-verification commands.

Verified: the baseline combined worker/UI/application check passed 62 tests; the native application and task-board WebAssembly release builds passed. Four focused packaging tests passed. A clean Windows x64 release at `13bc70b0c819b9e2cb6f89a8f7cf752bb9b76c31` produced an unsigned MSI; extraction confirmed the executable and both required MSVC runtime DLLs. Its SHA-256 is `804f7a2167d9765bf512d31fdac0983c712c1c56844ad838bc66933cf91bdaab`. Full paths, tool pins and evidence are in the release status document.

Use repository `docs/person4-build-release-status.md` for the exact supported sccache revision, profile pins, provenance rules and packaging commands. `dacs build-sysroot-digest --sysroot <path>` produces the profile's sysroot digest. Compiler pool ownership is visible at `/builds`.

Still unverified: one Linux build consuming distinct outputs from two other physical hosts, live resource limits and worker-removal/cache comparisons, Windows ARM64/Linux/macOS package production, installation/repair/update, platform catalogue coverage and signing/notarization. The Windows x64 MSI is an unsigned preview, not a trusted update. The available Docker Linux daemon was unreachable; no multi-host fleet or signing identities were used. The original acceptance requirements below remain required.

Read `00-general-plan.md` first. Own build/test execution, real distributed Rust compilation and platform release production/verification. Person 1 owns installer/update code and the larger software track; Person 2 supervises your processes; Person 3 owns overlay networking.

The event window is fixed at 210 elapsed minutes. Prove external mechanisms by hand early, then automate the proven configuration. A manual compiler proof reduces risk but does not replace the final DACS-managed proof.

## Ownership and boundaries

Own build/test runners, shard isolation, sccache pool configuration and private daemons, toolchain/execution profiles, action evidence, cache/failure comparisons, native package builds and release verification/signing procedure.

Use Person 1's admission/reservation/result schemas and Person 2's launch/accounting/termination interface. Person 1 writes the installer engine, catalogue schema, dependency-plan logic and update client. You supply platform prerequisites, reviewed build/tool entries, actual package artifacts and verification results, without independently editing its UI/setup modules.

## Prove and configure the supported path

Use one Linux initiating environment and two other physical PCs with compatible 64-bit Linux compile environments for the first proof. Each environment needs its own private reachable address from Person 3. Confirm the real workspace has independent eligible library crates before choosing it.

Pin Cargo.lock, source, compiler/sysroot, target, features, flags, relevant environment and dependency inputs. Disable incremental compilation for the distribution profile. Binaries, build scripts, linker work and unsupported invocations remain local as required. `cargo check` and Clippy are not this compilation proof. Cross-host procedural-macro dependencies and Windows/macOS/WebAssembly targets require separate profile evidence.

First configure stock sccache by hand over the private overlay and consume outputs from both remote servers in one build. Then implement DACS ownership of that setup. Do not debug unproven network/compiler mechanisms inside a new abstraction layer at the end of the event.

## Pool and daemon rules

Each pool has a separate scheduler process, server registrations and authentication configuration, with one active distributed build. Disjoint pools may run concurrently. A physical host's compiler allocation belongs to one pool at a time; agents may use separately reserved capacity there.

Verify that every registered server belongs to the reserved pool. Stock sccache does not consult the DACS resource ledger. Register servers under stable overlay addresses/ports, matching their address-bound tokens. Drain actions before membership/credential/ownership changes.

Own a local sccache daemon per simultaneously active distribution profile with private configuration, socket/port and cache namespace. Configure before startup, route the build's wrapper to it and reuse only the identical profile. Do not restart an owner's unrelated daemon or alter global Cargo configuration.

Only explicitly submitted distributed builds use the pool. Ordinary agent builds/checks stay local under their own reservations, rather than queueing behind integration builds. Show the pool queue and holder.

Reserve initiating Cargo, local scripts/linking and remote rustc processes. Person 2 supervises and counts them. Verify backend concurrency/memory behavior before advertising the profile; database reservations alone do not limit processes. Include bubblewrap/privileged backend prerequisites in reviewed installer data.

On coordinator loss, continue only the admitted build in its existing pool, then reconcile processes, assignments and outputs before reuse. Test shards need independent databases, ports and temporary state. Retries must not conceal compile or test failures.

## Evidence and release responsibilities

Record action/crate/target, source/toolchain, enrolled worker, remote successes, local work, cache hits, fallback, failures, repeated attempts, timings and transferred bytes separately. Store DACS artifacts separately from compiler-result caches.

The final proof is one native Linux Cargo build with clean outputs and a cold result cache. Both other physical PCs must execute at least one distinct eligible invocation whose output the build consumes. Link and exercise the result. Cached retrieval, registered workers and unrelated builds do not count. No equal-share or ten-actions-per-host quota applies.

Repeat worker removal and record fallback/retry behavior. A distribution-required build without the required remote contributions fails that requirement even when a local binary succeeds. Report local/distributed/warm-cache comparisons from actual runs; leave unrun comparisons unrun.

Produce installable artifacts for required Windows/Linux/macOS architectures and verify Person 1's real setup/update paths. For each supported platform, verify at least one catalogue entry in every required category: Rust toolchains, rustfmt/Clippy, Rust web tooling, Git, debuggers, editors, browsers and databases. Record install/detect/repair/update outcomes and share them with the setup agent. These are selectable tools, not mandatory installations on every device. Missing category coverage remains failed or not run; do not hide it by removing the category.

Use only existing signing/notarization access. Application-update signing keys stay off every machine that runs agents, including the coordinator. Obtain signed approved artifacts through the trusted signing environment; do not copy keys into a build agent. Without a required identity, produce a clearly labelled unsigned preview and record the signing requirement as failed. Test authenticated update handling without treating unsigned previews as trusted updates.

## Schedule

| Time | Your work |
|---|---|
| 00:00-00:15 | Check initiator/two remote physical hosts, toolchains/library graph, platform SDKs and existing signing access. Agree process and build/release contracts. |
| 00:15-00:50 | Prepare toolchains while Person 3 brings up networking; prove a stock private sccache build on both servers and produce native package artifacts. |
| 00:50-01:50 | Implement DACS pool/daemon ownership, runners, evidence and platform packaging around the proven mechanisms. |
| 01:50-02:15 | Connect DACS admission, Person 2's supervision and Person 3's stable addresses. Confirm an owner's existing daemon is unaffected. |
| 02:15-03:00 | Through DACS, supply task-board browser acceptance and backend distributed-build evidence; run pool, failure, cache and platform release cases. |
| 03:00-03:20 | Fix observed adapter/package failures and rerun affected scenarios. |
| 03:20-03:30 | Deliver final artifacts, checksums, signing/platform status, source revisions and observed build evidence. |

## Demonstration and handoff

Person 2 builds the private task-board backend/redb persistence; Person 3 builds its client-rendered Leptos UI; Person 1 coordinates and contributes preview packaging. Inspect the real browser: create, edit, change state, filter, restart and verify persistence, then delete. Check keyboard/narrow-viewport behavior. Native desktop interaction is a separate Person 2 gate.

Use the backend's native Linux dependency graph for remote compilation if eligible. The frontend is not the substitute. Otherwise select another real Rust objective with independent eligible work; do not pad action counts artificially.

Submit small working commits, the contract revision, logs/artifact locations, platform catalogue results and failed/not-run cases to Person 1. Run narrow checks after coherent changes; Person 1 owns combined integration checks. No fake compile placement, package success or passing release status.
