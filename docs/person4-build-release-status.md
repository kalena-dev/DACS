# Person 4: build, pool and release status

Date checked: 2026-11-14. This is evidence from the current checkout and host;
unobserved requirements are recorded as not run.

## Implemented in this increment

`crates/dacs-worker/src/build_execution.rs` adds a Person 4 build adapter over
Person 2's `ManagedChild` interface:

- Validates private pool registration, HTTPS scheduler URLs, unique server
  addresses and one pool allocation per physical-host fingerprint.
- Pins source revision, `Cargo.lock` SHA-256, exact Cargo/rustc version strings,
  package, target, features/default-feature selection, rustc flags and declared
  environment values.
- Produces a Cargo `build --locked` descriptor with a fresh target directory,
  `CARGO_INCREMENTAL=0`, a profile-specific sccache config, daemon port and local
  cache path. A pre-existing target directory is rejected.
- Writes separate sccache scheduler, server and client configurations with
  address-bound JWT server authentication. Config files are created exclusively
  with mode `0600` on Unix; secrets are excluded from evidence records.
- Provides compile-action evidence fields and a gate validator requiring a
  successful cold-cache build plus uncached, consumed invocations from both
  distinct remote physical hosts.
- Labels Cargo builds and sccache daemons/scheduler/servers as host-level effects
  because build scripts and compiler processes are not sandboxed by this adapter.

The sccache config shape follows Mozilla's [distributed configuration guide](https://github.com/mozilla/sccache/blob/main/docs/Distributed.md)
and [quickstart](https://github.com/mozilla/sccache/blob/main/docs/DistributedQuickstart.md).
The server token must be produced for the registered overlay address by
`sccache-dist auth generate-jwt-hs256-server-token`. Build servers need Linux,
`bubblewrap` 0.3.0 or newer, and the documented privileged builder setup. The
adapter does not grant elevation or start remote machines.

## Observed environment

- Host: macOS 27, Apple Silicon `arm64`.
- Rust: `rustc 1.99.0`, `cargo 1.99.0`.
- `sccache`: not installed/on PATH.
- Linux initiator and two other physical Linux hosts: unavailable in this
  checkout session; no overlay addresses or device credentials were provided.
- `Cargo.lock`: absent.
- `00-general-plan.md`: absent from the checkout; the plan text supplied in the
  conversation was used.
- Workspace integration: `cargo check -p dacs-worker` stops before compilation
  because `crates/dacs-coordinator/Cargo.toml` is missing. The workspace also
  lists `dacs-net` and `dacs-console`, which are absent. The protocol crate
  declares modules not yet present in the checkout.
- Existing sccache daemon/configuration: not inspected or changed. No global
  Cargo or sccache configuration was edited.

## Mandatory gates

| Gate | Status | Evidence / blocker |
|---|---|---|
| Feasibility: Linux initiator plus two physical Linux workers | **Failed** | Current host is macOS arm64; required hosts and overlay addresses are absent. |
| Real stock sccache build dispatched to both remote servers | **Not run** | sccache and the three-host Linux pool are unavailable. |
| One cold native Linux Cargo build consuming uncached output from both physical remotes | **Not run** | No Linux pool, lockfile, workspace build, or action-level placement logs. |
| Remove one compiler worker and observe fallback/retry behavior | **Not run** | No compiler pool exists to remove a worker from. |
| Cache/local/distributed timing and transfer comparison | **Not run** | No actual build or scheduler/server telemetry. |
| DACS admission, reservation and process accounting for compiler work | **Not run** | Coordinator and reservation APIs are not present in this checkout. |
| Compiler concurrency and memory enforcement | **Not run** | No Linux server or cgroup/backend evidence. Pool slot declarations alone do not enforce process limits. |
| Windows/Linux/macOS native package builds | **Not run** | No release pipeline/artifact definitions or platform SDK matrix in the checkout. |
| Per-platform installer catalogue coverage and setup/update verification | **Not run** | Installer/catalogue are Person 1 scope and are not present. |
| Signing/notarization | **Not run** | No signing identity or controlled signing environment was supplied. |

## Integration required

- Person 1 owns `Cargo.toml` and must register an owned build crate/module boundary
  when the missing workspace crates and shared contracts are integrated. This
  increment is placed under `dacs-worker` only to reuse the existing process
  descriptor without changing the workspace manifest.
- The present supervisor does not clear inherited environment variables or
  capture process output. A production build runner needs an explicit environment
  boundary and durable stdout/stderr/action evidence through the supervisor.
- Placement records must be reconciled from actual sccache server/action logs;
  scheduler totals or cache statistics cannot satisfy the per-host acceptance
  gate by themselves.
- `sccache-dist` resource limits must be enforced by a verified host mechanism
  (for example the reviewed systemd/cgroup profile); the pool model's slot count
  is accounting metadata and is not itself a process or memory limit.
