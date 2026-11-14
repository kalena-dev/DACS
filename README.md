# DACS

Rust coordinator, local setup interface and supporting worker/network/build crates for a private team of PCs. The coordinator stores orchestration state in SQLite, source in local bare Git repositories, and immutable artifacts in a private data directory.

The shared integration branch is `person-1-expanded-track`. See [team integration](docs/team-integration.md) for ownership and wire contracts. This branch is not a certified release or evidence of a running distributed cluster.

## Build

Install a Rust toolchain compatible with the workspace manifest and Git. Building native dependencies requires the platform's linker and SDK. Prebuilt application users should not need a Rust SDK; producing and verifying those platform packages belongs to the release track.

```sh
cargo build -p dacs
cargo build -p dacs --features native
```

The optional native window uses the same server-rendered Leptos interface. It needs WebView2 on Windows, GTK3/WebKitGTK 4.1 on Linux, or system WKWebView on macOS. It exposes no privileged webview IPC bridge. The browser interface works without the native feature.

Network binaries are built from the same root workspace:

```sh
cargo build -p dacs-netclient -p dacs-netadmin
```

The connected worker runtime and compiler-pool adapter live in `crates/dacs-worker`. Build/release evidence utilities live in `person-4/builds-release`. Compiler ownership is visible at `/builds`; workflow tasks link to their immutable result artifacts.

## Local coordinator and setup

```sh
cargo run -p dacs -- serve --listen 127.0.0.1:7447 --data-dir .dacs --local-setup
```

Open `http://127.0.0.1:7447`. Sign in with the generated `.dacs/admin.token`; keep that file private. Do not put tokens in URLs or commits. The state directory is owner-restricted. Source import/export needs Git on the coordinator's PATH.

`--local-setup` enables owner-authenticated installation/onboarding controls only on a loopback listener. Public coordinator listeners never offer remotely invocable package installation. Browser mutations require their session CSRF token and the configured origin; native sessions use those same controls.

```sh
cargo run -p dacs --features native -- desktop --url http://127.0.0.1:7447/
```

Public installation or opening an invitation does not start a worker, join a network or authorize administrator access. Provider login happens manually through OMP. Elevated execution is full trust; its access preferences do not create a sandbox.

## Private-network deployment

Person 3's deployment, consent contract and recorded network evidence are in [net/README.md](net/README.md). Deploy Headscale/netadmin on the infrastructure host first. Headscale administrative keys stay there, outside agent machines. The coordinator holds only its scoped netadmin operation token in a private file.

The coordinator uses TLS control on port 7443 and a restricted TLS bootstrap listener on 7444. Both listeners use the supplied certificate/key, which must cover their control and bootstrap names. nginx passes bootstrap TLS through to the coordinator. Configure the declared `--public-url` to match the browser control origin.

```sh
dacs serve --listen 0.0.0.0:7443 --bootstrap-listen 0.0.0.0:7444 --public-url https://CONTROL_NAME --tls-cert /private/server.pem --tls-key /private/server.key --network-config /private/network.json --data-dir /private/dacs
```

`network.json` contains `netadmin_url`, `operation_token_file`, and optional `ca_file`. Use the actual infrastructure endpoint and private local file paths. TLS verification stays enabled. Development certificates can be generated with `dacs tls --hostname NAME --output-dir DIRECTORY`; clients must explicitly trust their certificate through the relevant CA option.

The bridge records consent and individual approval before requesting a device-specific grant. Issued keys are not proof of membership. Unknown issuance or revocation outcomes remain unresolved until reconciled; they are never reported as success. Network details are available at `/network`. Local owner onboarding is at `/onboarding`, under the same local-only boundary as setup. Its private state lives under `data_dir/onboarding`.

## Manual worker operation

After confirmed enrollment, `/onboarding` can prepare `data_dir/worker/worker.json` and its private device-token file. Supply an absolute path to the installed OMP executable and select the owner's profile. The configuration uses the coordinator's overlay endpoint and the enrolled OMP version. Complete provider login manually in that profile.

```sh
dacs worker run --config /private/dacs/worker/worker.json
dacs worker status --data-dir /private/dacs/worker
dacs worker control --data-dir /private/dacs/worker --action pause-desktop
dacs worker control --data-dir /private/dacs/worker --action drain
```

The same controls appear in local onboarding. Ctrl-C drains; `emergency-cancel` requests termination. The runtime journals commands and results, uses isolated Git workspaces, and supervises process trees. Startup performs a small real provider-readiness turn under the selected profile's limits. It never logs in automatically. Desktop work requires observed native capabilities, local opt-in and exclusive ownership; pending controls are distinct from observed state.

Workflow requests use `TaskSpec.execution` with `kind: "workflow"`. Their result artifact must match the admitted task, attempt, generation, source revision and reported checks. Read [build/release status](docs/person4-build-release-status.md) for pinned engine profiles, compiler telemetry and packaging commands. `dacs build-sysroot-digest --sysroot PATH` computes the sysroot pin used by a distributed-build profile.

## Private task-board preview

Build the client-rendered Rust frontend with the WebAssembly target and Trunk:

```sh
rustup target add wasm32-unknown-unknown
cd examples/task-board-web
trunk build --release
cd ../..
dacs task-board --listen 127.0.0.1:7448 --data .dacs/task-board.redb --assets examples/task-board-web/dist
```

Open `http://127.0.0.1:7448`. The board supports create/edit/status, combined filters and confirmed deletion. redb stores tasks across backend restarts. The preview binds only to loopback, checks mutation origins and serves assets separately from its database. A working board does not establish the required multi-agent or distributed-compiler proof.


## Source and execution contracts

- Workers communicate through authenticated APIs, never the SQLite database file.
- Persist command receipts before acknowledgment. Replay an unchanged command ID/body after ambiguous transport failure; changed-body reuse is rejected.
- Each attempt carries its own generation, source revision, lease and resource reservation. A missed heartbeat alone does not release resources.
- OMP turn completion is distinct from session settlement. Pending delegated work keeps its parent obligations alive. Stop reports require confirmed process termination.
- Source changes arrive as Git bundles. Imports validate in a bounded quarantine before promotion; imported refs cannot replace the integration target.
- Integration prepares an exact merge SHA, records review against that SHA, and publishes using an expected-head check. Publication/cancellation races and crash reconciliation are persisted.
- Artifacts use SHA-256 digests and project authorization. Git's objects and artifacts are separate stores.

Use `dacs --help` for the CLI. `request --file` accepts a typed command/envelope; `snapshot`, `source import/export`, and `artifact upload/download` use authenticated coordinator transport. The authoritative DTOs are in `crates/dacs-protocol/src/lib.rs`.

## Dependency setup and updates

The catalogue retains Rust toolchains, rustfmt/Clippy, Rust web tooling, Git, debuggers, editors, browsers and databases as selectable categories. It uses reviewed rustup/package-manager recipes. Cargo manages project crate dependencies; the installer does not install them globally.

`dacs setup catalog`, `detect`, `plan --request FILE`, `apply --plan FILE --consent FILE`, and `scan --repo PATH [--ai]` expose the engine. Inspect the plan and unresolved prerequisites before approving installation. AI scans are read-only proposals, not authority to execute repository instructions. Missing or declined tools disable affected capabilities rather than being reported as installed.

Signed application updates verify a locally configured trust key, version, platform, size and digest. `dacs update` supports verification, staging, application and rollback. A running Windows executable hands replacement to a helper, after accepted local jobs drain. Startup reconciles interrupted update transactions before accepting work. Application updates do not authorize unrelated dependency upgrades.

## Verification limits

Run checks against the components being changed, then perform combined integration verification. The root workspace includes the network crates; there is no separate `net/Cargo.lock`.

Unit/contract fixtures do not prove autonomous OMP execution, a multi-PC cluster, remote compiler placement, package installation across platforms or signing/notarization. The required distributed-build proof must show uncached compiler outputs from two distinct remote physical hosts consumed by one successful native build. Retain real action logs and failed/unrun gates. Never use build totals, configuration files or cache hits as a substitute.
