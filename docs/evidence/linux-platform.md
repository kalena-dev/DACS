# Linux platform row (container)

Date: 2026-11-14, 20:28–20:50 UTC. One Linux container in a local VM on one macOS host, loopback and
bridge networking only. Scope: the Person 2/3 crates (`dacs-worker`, `dacs-net-contract`,
`dacs-netadmin`, `dacs-netclient`), plus `cargo build -p dacs` and one full workspace test run.

| Item | Value |
|---|---|
| Host | macOS 27.0.1, Apple Silicon `arm64` |
| VM | Colima 0.10.3, default profile (`vz`, 4 CPUs, 8 GiB, no network address, `colima start` needed no `sudo`); Docker Engine 29.5.2 `linux/arm64` |
| Kernel | `Linux 6.8.0-117-generic aarch64` (Ubuntu VM kernel, shared by the container) |
| Image | `rust:1.99-bookworm`, `rust@sha256:e6f2cff35d0f309acf8433b4f05878ed77092602c5065cc81801177da0bb3a9c` (`linux/arm64`) |
| Container OS | Debian GNU/Linux 12.15 (bookworm), glibc 2.36, `awk` = mawk 1.3.4 20200120, perl 5.36.0 |
| Toolchain | `rustc 1.99.1 (58cf8f9c3 2026-10-13)`, `cargo 1.99.1`, `clippy 0.1.99`, host `aarch64-unknown-linux-gnu`, LLVM 23.1.1 (workspace `rust-version` is 1.89) |
| Container settings | `docker run --init` (PID 1 `docker-init`), bridge network, not privileged, no host networking; worktree bind-mounted read-only and copied to container-local `/work`; `CARGO_TARGET_DIR` and the Cargo registry on named volumes, so host `target/` was untouched |
| User | Commands ran as a non-root user (`uid=1000`), not as root |

Commits (`origin/main`):

- **`3e655e1`**: first run, which exposed the two Linux bugs below.
- **`23b67b6`** plus the two fixes (pushed as `1fce754` and `c04317d`): fixes verified, repeated runs.
- **`894c14c`**: final run of every check in the table below.

## Commands and results at `894c14c`

All `cargo` commands used `--locked`.

| Check | Command | Result |
|---|---|---|
| Person 2/3 tests | `cargo test -p dacs-worker -p dacs-netclient -p dacs-netadmin -p dacs-net-contract --no-fail-fast` | **Passed**: 187 passed, 0 failed, 2 ignored (helper processes that the crash tests re-execute). 12 of the passes are `omp_real` tests that skip without `DACS_TEST_OMP` (see "Not run") |
| Crash tests, repeated | `cargo test -p dacs-worker --test crash_recovery --test crash_reconnect`, 5 times | **Passed** 5/5 (4 + 2 tests each time) |
| Workspace tests | `cargo test --workspace --no-fail-fast` | **Passed**: 316 passed, 0 failed, 2 ignored. Unlike macOS, `dacs-setup` (26) and `dacs` `http_contract` (7) pass, because Linux `/tmp` is not a symlink |
| Clippy, network crates | `cargo clippy -p dacs-net-contract -p dacs-netadmin -p dacs-netclient --all-targets -- -D warnings` | **Passed**, no diagnostics |
| Clippy, worker | `cargo clippy -p dacs-worker --all-targets` | Passed with 29 `dacs-worker` warnings and 3 `dacs-core` warnings |
| Clippy, worker, strict | `cargo clippy -p dacs-worker --all-targets -- -D warnings` | **Failed**: the same 29 + 3 lints as errors. They are not Linux-specific; see failure 3 |
| Coordinator build | `cargo build -p dacs` | **Passed**. One rustc warning (failure 4). `dacs --help` runs; the binary is an `aarch64` ELF PIE linked against glibc |
| Native-shell build, stock image | `cargo build -p dacs --features native` | **Failed** (at `23b67b6`): `The system library gdk-3.0 required by crate gdk-sys was not found` (`pkg-config --libs --cflags gdk-3.0 'gdk-3.0 >= 3.22'`). The image has no GTK/WebKitGTK development files |
| Native-shell build, with GTK/WebKitGTK | `apt-get install libwebkit2gtk-4.1-dev libgtk-3-dev pkg-config` (inside the container), then the same build | **Passed** (with `libwebkit2gtk-4.1-dev 2.50.6-1~deb12u2` and `libgtk-3-dev 3.24.38-2~deb12u3`). Build only; the shell was not launched |

Per-crate counts at `894c14c`:

| Crate | Targets (passed) |
|---|---|
| `dacs-net-contract` | lib 8 |
| `dacs-netadmin` | lib 16, `auth_and_tls` 12, `key_issuance` 12, `secret_hygiene` 4, `unknown_outcomes` 12 |
| `dacs-netclient` | lib 36, `fake_tailscale_cli` 9, `join_preconditions` 5, `measure_cli` 4 |
| `dacs-worker` | lib 37, `capacity` 2, `crash_reconnect` 2 (+1 helper ignored), `crash_recovery` 4 (+1 helper ignored), `journal_read_only` 6, `lifecycle` 6, `omp_real` 12 (skipped, see "Not run"); `windows_workflow` has no tests outside Windows MSVC |

## Linux bugs found and fixed

### 1. Restart refused to reconcile DACS's own orphaned process groups (`supervisor.rs`, fixed in `1fce754`)

At `3e655e1`, `crash_recovery::worker_crash_with_live_process_trees_is_reconciled_on_restart` failed 5 times out of 5:

```
prior process group 880 is unreconciled: member 881 started before the recorded leader; owner must inspect it before restart
```

Cause: on Linux, `sysinfo` reports a start time as `/proc/stat` `btime` plus whole seconds since boot,
each rounded down, so it can be up to a second behind the wall clock. The receipt's `launched_at` bound
was taken from the wall clock. A child started in the same second therefore usually looked older than
its own launch, and the leaderless-group identity check refused it. That check runs when the worker
restarts after a crash and the group leader has already exited. The check failed closed, so nothing
was killed wrongly, but the worker could not restart without the owner reconciling by hand.

How often it happens depends on the fractional second of the boot time. On this VM (boot time ≈ x.97 s),
169 of 200 shell launches reported a start time before the launch second. macOS reports exact start
seconds and was not affected.

Fix: on Linux, take the bound as `System::boot_time() + System::uptime()`. That is the same clock and
rounding `sysinfo` uses, and it is read before spawn, so it can never be later than a child's reported
start time. Other platforms are unchanged. The new Linux-only unit test
`launch_bound_never_exceeds_a_later_childs_reported_start_time` failed on the first iteration with the
old bound (`child start 1794688529 precedes its launch bound 1794688530`) and passes with the fix.
Afterwards `crash_recovery` passed 10/10, and 5/5 again at `894c14c`.

### 2. `crash_reconnect` fake OMP never answered on Debian (test fixture, fixed in `c04317d`)

At `23b67b6`, both `crash_reconnect` tests failed with `timed out waiting for first OMP turn reported`;
the worker logged `OMP RPC request timed out`. The scripted fake OMP uses `awk`, which is mawk on
Debian and Ubuntu. mawk buffers piped input unless it runs with `-W interactive`; with a FIFO, the
first line was held until more input arrived (measured: 3 s delay without the flag, none with it).
The fixture now passes `-W interactive` only when `awk -W version` reports mawk. This was a portability
bug in the test, not in the product. The test then passed 3/3 at `23b67b6` and 5/5 at `894c14c`. It
still passes on macOS (BSD awk).

## Environment finding: PID 1 must reap orphans

The first run used a container whose PID 1 was `sleep infinity`, which never reaps processes. Two
`crash_recovery` tests failed: `timed out waiting for orphaned leader reaped` and
`prior process group has not terminated`. On Linux, `kill(-pgid, 0)` succeeds while a group still has
zombie members (verified: a zombie-only group returned success, then `ESRCH` after reaping). macOS
returns `EPERM` instead. So a group whose orphans are never reaped never counts as stopped. The
supervisor reports this correctly as "not stopped" and keeps the reservation. With `--init`, both tests
pass.

Deployment requirement: run the worker under an init that reaps orphans, such as systemd, a user
manager, or `docker run --init`/tini in a container. Don't make the DACS worker PID 1 of a container
or guest without one. Otherwise every stop that leaves an orphaned descendant stays unreconciled. No
code change was made for this.

## Failures and observations for other owners

3. **Clippy under `-D warnings`** (Person 2 `task_board.rs`, `runtime.rs`, `omp.rs`; Person 4
   `build_runner.rs`; Person 1 `dacs-core`). The worker has 29 lints: 15 `result_large_err` in
   `task_board.rs`, 9 `collapsible_if` (in `runtime.rs`, `omp.rs` and `build_runner.rs`), 4
   `too_many_arguments` and 1 `large_enum_variant` in
   `build_runner.rs:46`. `dacs-core` has 3: `collapsible_if` at `engine.rs:66`, `cmp_owned` at
   `engine.rs:840` and `collapsible_match` at `network.rs:425`. The same lints appear on macOS (see
   `macos-platform.md`). Suggested fixes are listed there.
4. **`unused_mut` warning** (Person 1, `crates/dacs-setup/src/util.rs:61`, `let mut link`). It is only
   mutated under `#[cfg(windows)]`, so Linux builds warn as macOS builds do. Suggested fix: as in
   `macos-platform.md` failure 2.
5. **Native-shell prerequisites** (application/setup owners). On Debian 12, `--features native` needs
   `libwebkit2gtk-4.1-dev`, `libgtk-3-dev` and `pkg-config` at build time, and the matching runtime
   libraries at run time. Linux packaging and setup should declare these.

## Residual risk (not observed)

The receipt's PID-reuse check compares the leader's `sysinfo` start time for exact equality. On Linux
that value includes `/proc/stat` `btime`, which moves when the wall clock is stepped (an NTP step, or a
VM clock resync after the host sleeps). A step between launch and restart would make restart report
`PID reused` and fail closed. `btime` stayed at `1794688096` throughout this session, so this was not
observed. A more robust Linux identity would be the raw boot-relative start ticks
(`/proc/<pid>/stat` field 22) together with `/proc/sys/kernel/random/boot_id`.

## Not run

- `omp_real` against a real OMP. `DACS_TEST_OMP` was unset, so its 12 tests printed a skip and
  returned. No OMP, provider login or model call was used.
- Launching the native shell. The container had no display, so desktop or Wayland capture, input,
  clipboard and pause/resume were not run.
- A live worker against a real coordinator, Tailscale/Headscale membership (only the fake CLI and
  in-process mocks ran), sccache or distributed compilation, systemd/bubblewrap enforcement, packaging
  and install.
- An x86_64 Linux build or tests (only `aarch64` ran), the MSRV toolchain 1.89 (only 1.99.1 ran), and
  running the tests as root.

## Limits

This is one Debian container on a Linux VM, on one macOS host. It is not a native Linux desktop
installation, so it does not certify native Linux desktop, X11 or Wayland capture. It is not a
separate physical host, so it says nothing about multi-PC, NAT or physical-host accounting. It shares
the VM kernel (Ubuntu 6.8) rather than running a distribution kernel on hardware. The results show
that these crates build and their tests pass on Linux `aarch64` with glibc. They do not show live
Linux worker behaviour.
