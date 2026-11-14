# Person 4: build, compiler pool and release status

Updated 2026-11-14 for the current Windows checkout. Earlier macOS/missing-workspace observations have been superseded. This file distinguishes implemented source from execution evidence.

## Implemented source

- `build_runner::execute_admitted` parses strict workflow requests and verifies the external-effect admission, generation, source commit, clean checkout, lockfile, Cargo/rustc versions, verbose compiler identity and sysroot content digest. All child commands use the worker supervisor and attempt reservation.
- Real Cargo builds and test shards run with private clones, target directories, temporary directories, database paths and ephemeral-listen hints. Shards run sequentially within admitted resources; a failing command is not retried or hidden. Applications must honor the shard database/listen environment when they use those facilities.
- Logs are drained concurrently and durably bounded. Actual exit status, durations, checksums, Cargo-produced executables, requested artifacts and behavioral command results populate immutable workflow evidence. Cancellation fences subsequent child launches; uncertain process stops remain unreconciled.
- Compiler scheduler/server workflows generate private stock sccache configurations from owner-local credential files. Their independently admitted reservations cover resources. Server workflows run in actual systemd CPU/memory scopes; instrumented server compilation slots are bounded. Missing systemd/bubblewrap/elevation fails execution rather than claiming enforcement.
- Coordinator pool ownership validates all admitted services, enrolled physical hosts and exact pool membership. It prevents simultaneous ownership across pools and keeps ownership until the initiating process and every service report actual stop. Build stop requests service cancellation; heartbeat loss does not release ownership. Authenticated pool/evidence APIs expose real state.
- Each initiating distribution profile owns its local daemon, port, cache and configuration. Occupied ports are refused; no owner daemon is restarted and no global Cargo/sccache files are modified.

## Supported action provenance

The supported instrumented engine is upstream sccache **v0.10.0**, commit
`e3d1ed560a28d114bd26299b0f717b372e7fc3d2`. Its reviewed compiler-server path is native x86_64 Linux. This is not a claim that all native DACS platforms support remote compilation.

`dacs-release-audit instrument-sccache <clean-checkout>` applies exact, unique source anchors only after checking the clean pinned revision. It does not download, compile, install or start the engine. On its release host, build the instrumented engine with:

```text
cargo build --locked --release --no-default-features --features dist-client,dist-server
```

Pin the produced engine executable digests and the separately built `dacs-rustc-provenance` wrapper in workflow requests. Engine service stderr emits real job completion/output SHA-256 records; the worker journals and uploads these with authenticated worker/attempt identity. The initiator records received output digests. The wrapper records actual rustc `--extern` inputs and produced output digests around the real command.

The runner joins service job identities, reserved server addresses, received compiled-library bytes, and the transitive input/output graph ending in an executable that the runner actually exercised. It reuses the contributed release validator to require consumed uncached invocations from both distinct remote physical hosts. Missing telemetry, altered digests, replayed jobs, unrelated workers, unconsumed libraries and cache statistics cannot pass the requirement. Dependency-file rewrites are not treated as compiled-library provenance. Evidence is not a sandbox against a malicious elevated host owner.

Profile sysroot pins are available through:

```text
dacs build-sysroot-digest --sysroot <rustc --print sysroot result>
```

Workflow `operation.kind` is `build`, `test` (with named filters), or `compiler_service`. Distributed requests include `pool`, `claim.service_attempt_ids`, private client-token path, engine/wrapper SHA-256 pins, enrolled initiator physical identity and the two required remote physical identities. `exercise` entries select an executable actually reported by Cargo and its arguments. Compiler services require owner-provisioned credentials and the reviewed `systemd-run`/bubblewrap executable paths. Runtime transport credentials never appear in request or result artifacts.

## Native release production

`dacs-release-audit package <request.json>` performs a real pinned `cargo build --locked --release --package dacs --features native` and invokes native packaging tools:

| Native host | Installable output | Required external tool |
|---|---|---|
| Windows x64/ARM64 | MSI | WiX 4 `wix` and the target MSVC SDK |
| Linux x64/ARM64, Debian-compatible runtime | DEB | `dpkg-deb`, GTK/WebKit native build dependencies |
| macOS x64/ARM64 | PKG installing the CLI/native-shell executable | Xcode SDK and `pkgbuild`/`pkgutil` |

Request fields: `workspace`, a new `output` directory outside the checkout, `target`, numeric `version`, `source_commit`, `cargo_lock_sha256`, `rustc_version`, `cargo_version`. A dirty/unpinned source, unavailable tool or foreign platform fails; stale output cannot be reused. Packages and reports are explicitly **UNSIGNED-PREVIEW**, not trusted updates. Hashing reuses the existing release audit checks.

`dacs-release-audit verify-signed <package> <new-log-directory>` verifies packages returned by an existing trusted signing environment with Windows `signtool` or macOS `pkgutil`, Gatekeeper and stapler. It never acquires/imports signing keys or signs from an agent. Successful platform signature verification does not establish installation or DACS authenticated-update acceptance.

## Verification status and required external gates

Parent integration has passed `cargo test -p dacs-worker -p dacs-ui -p dacs --features dacs/native` (62 tests) and `cargo build -p dacs --features native`. The task-board WebAssembly release build also passed. No live compiler fleet, native package production or signing command has been verified. Added regressions cover nonzero supervised exits, bounded stream draining, exclusive locks, confined artifacts, actual consumption paths, worker/digest mismatches, replay rejection, package metadata validation and exact instrumentation anchors.

| Required gate | Current evidence |
|---|---|
| Coherent code compilation and focused regressions | Combined worker/UI/application check: 62 passed; native application build passed |
| Instrumented engine compilation on pinned Linux toolchain | Not run |
| Linux initiator plus two other physical compiler hosts | Not available in this assignment |
| One cold build consuming both remotes, link and behavioral checks | Not run; source adapter now present |
| Worker removal, fallback/retry, local/distributed/warm timing and transfer comparisons | Not run; requires real pool observations |
| Actual systemd resource enforcement and unchanged unrelated daemon | Not run on Linux |
| Windows/Linux/macOS package production and install/repair/update | Not run; source production commands present |
| Every catalogue category on every supported platform | Not run; no category removed |
| Signing/notarization and trusted application-update acceptance | Not run; no signing identities or keys used |

Official engine sources: [distributed guide](https://github.com/mozilla/sccache/blob/v0.10.0/docs/Distributed.md), [server completion](https://github.com/mozilla/sccache/blob/v0.10.0/src/bin/sccache-dist/main.rs), [initiator output handling](https://github.com/mozilla/sccache/blob/v0.10.0/src/compiler/compiler.rs).
