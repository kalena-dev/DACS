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
| Windows x64/ARM64 | MSI | WiX 4 `wix`, target MSVC SDK and `VCToolsRedistDir` from its developer environment |
| Linux x64/ARM64, Debian-compatible runtime | DEB | `dpkg-deb`, GTK/WebKit native build dependencies |
| macOS x64/ARM64 | PKG installing the CLI/native-shell executable | Xcode SDK and `pkgbuild`/`pkgutil` |

Request fields: `workspace`, a new `output` directory outside the checkout, `target`, numeric `version`, `source_commit`, `cargo_lock_sha256`, `rustc_version`, `cargo_version`. A dirty/unpinned source, unavailable tool or foreign platform fails; stale output cannot be reused. Packages and reports are explicitly **UNSIGNED-PREVIEW**, not trusted updates. Hashing reuses the existing release audit checks.

Windows packages include the app-local `vcruntime140.dll` and `vcruntime140_1.dll` from the selected MSVC redistributable directory, recording their hashes and sizes. Windows tool paths are normalized before WiX receives them. The native desktop command still requires the external Microsoft Edge WebView2 runtime; it is not bundled or installed by this MSI. The optional task-board command takes separately compiled frontend assets through `--assets`; those example assets are not MSI payload.

`dacs-release-audit verify-signed <package> <new-log-directory>` verifies packages returned by an existing trusted signing environment with Windows `signtool` or macOS `pkgutil`, Gatekeeper and stapler. It never acquires/imports signing keys or signs from an agent. Successful platform signature verification does not establish installation or DACS authenticated-update acceptance.

### Observed Windows x64 unsigned preview — 2026-11-14

The first clean isolated build pinned to `3515a8b492f1df4651e6000b89a0d6298e7968d8` compiled the native release successfully, then WiX failed with `WIX0001 System.UriFormatException` because its XML reader rejected Rust's canonical `\\?\` paths. `dumpbin /dependents` also exposed two required VC runtime DLLs missing from the original package definition. The fixes were published as `13bc70b0c819b9e2cb6f89a8f7cf752bb9b76c31`; a second clean isolated clone at that commit produced the MSI through the normal clean-source-guarded `dacs-release-audit package` command.

- **MSI:** `C:/Users/PC/Documents/GitHub/DACS/.local/win-msi-final/package/dacs-0.1.0-x86_64-pc-windows-msvc-UNSIGNED-PREVIEW.msi`
- **Source:** `13bc70b0c819b9e2cb6f89a8f7cf752bb9b76c31`
- **MSI SHA-256:** `804f7a2167d9765bf512d31fdac0983c712c1c56844ad838bc66933cf91bdaab`; **7,835,648 bytes**
- **Cargo.lock SHA-256:** `52bf396616f89062fff0da2c5ee9462fb1738e9ab3713f7d1e017ee3f57e1c93`
- **Compiler:** `rustc 1.98.0 (88d9e12ae 2026-08-18)`; compiler commit `88d9e12ae178fab0fb5cc050a94da85685d449ea`, LLVM `22.1.8`, target `x86_64-pc-windows-msvc`.
- **Cargo:** `cargo 1.98.0 (797e8a9bc 2026-08-05)`.
- **Native tools:** MSVC tools `14.51.36231`, linker `14.51.36257.0`, Windows SDK `10.0.26100.0`, WiX `4.0.6+73c89738`. The driver called the installed `vcvars64.bat`, explicitly selected the MSVC `link.exe`, and set `CARGO_INCREMENTAL=0`; no custom Rust flags were set.
- **WiX acquisition:** no WiX command or global .NET tool was present. `dotnet tool install wix --tool-path C:/Users/PC/Documents/GitHub/DACS/.local/windows-release-3515a8b/tools --version 4.0.6` succeeded, using the [official .NET-tool distribution](https://docs.firegiant.com/wix/using-wix/). No system-wide tooling installation occurred.

Commands executed for the final production and inspection:

```text
cmd.exe /d /c C:\Users\PC\Documents\GitHub\DACS\.local\windows-release-3515a8b\package.cmd -root C:/Users/PC/Documents/GitHub/DACS/.local/win-msi-final -sourceCommit 13bc70b0c819b9e2cb6f89a8f7cf752bb9b76c31
powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:/Users/PC/Documents/GitHub/DACS/.local/windows-release-3515a8b/inspect.ps1 -root C:/Users/PC/Documents/GitHub/DACS/.local/win-msi-final
```

Both commands exited zero. The driver ran `cargo run --locked --package dacs-release-audit --bin dacs-release-audit --target x86_64-pc-windows-msvc --target-dir <root>/audit-target -- package <root>/request.json`. The producer ran `cargo build --locked --release --package dacs --features native --target x86_64-pc-windows-msvc --target-dir <root>/package/cargo-target` (finished in **1m 46s**) and `wix build <root>/package/dacs.wxs -arch x64 -o <MSI>` (empty diagnostic log, successful exit).

Inspection used `wix msi decompile <MSI> -o <root>/inspection/decompiled.wxs -x <root>/inspection/payload`, without installing the application. The extracted payload contained exactly three files, and every payload hash matched the built executable or recorded staged runtime:

| Payload | Bytes | SHA-256 |
|---|---:|---|
| `dacs.exe` | 25,734,656 | `ef90f528ef12e2048b818bd1010dc2e7e829c7a475605f4190070256ab555028` |
| `vcruntime140.dll` | 178,616 | `d1f4225df2cd877dbf130d5668a021dce3f94118455ff5ec952061c30afc9ce7` |
| `vcruntime140_1.dll` | 50,112 | `a7146c08f89fe5b04541ab507cdb59ff7b44534d4ba3c668a426c6450a03434e` |

Evidence root: `C:/Users/PC/Documents/GitHub/DACS/.local/win-msi-final/`. It contains `request.json`, `compiler.txt`, `cargo.txt`, `wix.txt`, `build-environment.json`, `package-driver.log`, `package/release-evidence.json`, build logs and `inspection/` with decompiled authoring, extracted hashes, dependency imports, signature result and successful payload/checksum assertions. The original failed attempt remains under `C:/Users/PC/Documents/GitHub/DACS/.local/windows-release-3515a8b/`.

`Get-AuthenticodeSignature` reported **NotSigned**. This proves package generation and payload inspection only: **no application installation, uninstall/repair, desktop launch, update acceptance or trusted signing was performed**. No signing keys were created or imported. The parent separately reported the four focused packaging tests passing before publication; this slice did not repeat broad workspace tests.

## Verification status and required external gates

Parent integration has passed `cargo test -p dacs-worker -p dacs-ui -p dacs --features dacs/native` (62 tests) and `cargo build -p dacs --features native`. The task-board WebAssembly release build also passed. The Windows x64 unsigned MSI production and payload inspection above passed; no live compiler fleet, other-platform package production or trusted signing was verified. Added regressions cover nonzero supervised exits, bounded stream draining, exclusive locks, confined artifacts, actual consumption paths, worker/digest mismatches, replay rejection, package metadata validation, Windows WiX path normalization and exact instrumentation anchors.

| Required gate | Current evidence |
|---|---|
| Coherent code compilation and focused regressions | Combined worker/UI/application check: 62 passed; native application build passed |
| Instrumented engine compilation on pinned Linux toolchain | Not run |
| Linux initiator plus two other physical compiler hosts | Not available in this assignment |
| One cold build consuming both remotes, link and behavioral checks | Not run; source adapter now present |
| Worker removal, fallback/retry, local/distributed/warm timing and transfer comparisons | Not run; requires real pool observations |
| Actual systemd resource enforcement and unchanged unrelated daemon | Not run on Linux |
| Windows/Linux/macOS package production and install/repair/update | Windows x64 unsigned MSI generated and extracted payload verified; installation/repair/update, Windows ARM64 and Linux/macOS package production not run |
| Every catalogue category on every supported platform | Not run; no category removed |
| Signing/notarization and trusted application-update acceptance | Windows MSI observed unsigned; required trusted signing absent; notarization/update acceptance not run; no signing identities or keys used |

Official engine sources: [distributed guide](https://github.com/mozilla/sccache/blob/v0.10.0/docs/Distributed.md), [server completion](https://github.com/mozilla/sccache/blob/v0.10.0/src/bin/sccache-dist/main.rs), [initiator output handling](https://github.com/mozilla/sccache/blob/v0.10.0/src/compiler/compiler.rs).
