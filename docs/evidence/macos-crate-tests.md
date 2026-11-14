# macOS crate tests: worker and network

Date: 2026-11-14. Commit: `f55ff7f` (`crates/dacs-worker` and `net/crates` unchanged since `7ac9ade`).
Host: macOS 27.0.1, Apple Silicon `arm64`, `rustc 1.99.0`. Installed `omp/18.16.0` (the team pins 18.20.0)
and Tailscale 1.102.4 (stopped; not used by these checks).

```sh
cargo test -p dacs-worker -p dacs-netclient -p dacs-netadmin -p dacs-net-contract
```

| Crate | Result |
|---|---|
| `dacs-worker` | 28 passed, 0 failed |
| `dacs-netadmin` | 16 passed, 0 failed |
| `dacs-netclient` | 14 passed, 0 failed |
| `dacs-net-contract` | 6 passed, 0 failed |

These are unit tests on one macOS host. They do not establish a provider-backed OMP assignment, a live
worker run, native macOS desktop interaction, Tailscale enrollment or multi-PC behaviour; those remain
not run on this host.
