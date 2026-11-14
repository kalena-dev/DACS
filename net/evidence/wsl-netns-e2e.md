# Network e2e with real clients in WSL network namespaces

Date: 2026-11-14. Host: Windows 11 (10.0.26300, x64), WSL2 Ubuntu 24.04, kernel 6.6.87.2.
Contract `dacs-net/2026-11-14.1`, disclosure `host-admin-access/1`.
Software: Headscale v0.29.7, Tailscale 1.104.7 (static amd64 tarball), `dacs-netadmin`/`dacs-net`
built from this repository (debug profile, Linux).

This is a **single-machine** run. Three Tailscale nodes ran with kernel TUN devices in separate network
namespaces, joined by a Linux bridge. It exercises the real client, the real control server, real
WireGuard, real DERP over TLS and real ACL enforcement. It is **not** the fleet proof: there is no public
server, no public DNS, no Let's Encrypt, no NAT traversal across real networks and no second physical PC.
Throughput numbers are loopback numbers and predict nothing about real links.

Harness: [`wsl-e2e/setup.sh`](wsl-e2e/setup.sh) and [`wsl-e2e/scenario.sh`](wsl-e2e/scenario.sh)
(throwaway test scripts, not deployment). They expect `/root/hs` (Headscale binary, config, sqlite DB,
API key) and `/root/ts` (Tailscale tarball) from the earlier
[local Headscale run](local-headscale-integration.md), and a clone of this repository at `/root/dacs-src`.
No secrets are in the repository. The test CA, operation token and API key exist only in WSL.

## Topology

| Role | Where | Address | Notes |
|---|---|---|---|
| Infra host | WSL root namespace | bridge `10.99.0.1` | Headscale B (DACS) on `127.0.0.1:8080` behind a TLS front `https://hs.dacs.test:8443` (stand-in for nginx). Embedded DERP region 999 and STUN `10.99.0.1:3478`. Default-deny repository policy. `tag:dacs-infra` node `100.64.0.1`. `dacs-netadmin` with TLS on `100.64.0.1:9443` (CA-signed cert for the overlay IP). |
| Owner's existing tailnet | WSL root namespace | `https://hs.personal.test:8444` | Second Headscale (A), its own DERP region 998, allow-all policy. Stands in for a personal tailnet. |
| Coordinator PC | netns `dacs-c` | underlay `10.99.0.11` | Roles `coordinator`, `worker` (physical). |
| Linux compile guest | netns `dacs-g` | underlay `10.99.0.12` | Roles `compile_scheduler`, `compile_server`, `host_kind: guest` with `physical_host: coord-e2e`. Its own node. |

TLS everywhere the contract requires `https://`. The throwaway CA is in the distro trust store, so the Go
client verifies Headscale and DERP certificates. `curl --cacert` verifies netadmin.

## Results

| # | Check | Result |
|---|---|---|
| 1 | `dacs-netadmin check` (config, secret file modes, Headscale reachability, TLS) | PASS `{"config":"ok","headscale":"ok","tls":true}` |
| 2 | netadmin `/v1/health` over TLS on the infra overlay IP | PASS |
| 3 | Coordinator-first bootstrap: `assess` (no change) → `dacs-netadmin issue` on the infra host (grant file mode 600, key not printed) → `dacs-net join` | **First run FAILED**: `join` timed out although the client was `Running` (bug A). PASS after the fix: `LocalJoinResult` with overlay `100.64.0.3`. |
| 4 | Coordinator → netadmin over the overlay with the operation token (`/v1/health`, `/v1/contract`) | PASS (HTTP 200) |
| 5 | Guest logged in to its existing personal tailnet; `dacs-net assess https://hs.dacs.test:8443` | PASS: `requires_switch_consent: true`, `previous_control_url: https://hs.personal.test:8444`, disclosure text returned, client unchanged |
| 6 | Coordinator `POST /v1/enrollments` for the guest over the overlay | PASS: tags `tag:dacs-sccache-scheduler`, `tag:dacs-sccache-server`, 15-minute single-use key |
| 7 | Enrollment requesting role `infra` | PASS: `403 forbidden` |
| 8 | `join` with consent lacking `tailnet_switch` | PASS: refused (exit 1); still `Running` on the personal server; key unused |
| 9 | `join` with `tailnet_switch.previous_control_url` for a different server | PASS: refused; still on the personal server |
| 10 | `join` with explicit switch consent | **First run FAILED**: stuck in `Starting`, `login` timed out (bug B). PASS after the fix: joined in seconds, `displaced_control_url: https://hs.personal.test:8444`, `tailscale switch --list` keeps the personal profile |
| 11 | Membership query by `auth_key_id` | PASS: `joined`, node 7, online, DACS tags |
| 12 | `GET /v1/compiler-endpoints` | PASS: scheduler `100.64.0.5:10600`, server `100.64.0.5:10501` (overlay only) |
| 13 | `dacs-net compiler-check` from the coordinator (stand-in listeners bound to the guest overlay IP) | PASS: both reachable, `overlay_private: true`, path `direct 10.99.0.12:41641` |
| 14 | ACL default deny: guest → coordinator `:10501`, guest → infra `:9443` | PASS: both unreachable (timeout / no route) |
| 15 | Compiler port on the guest's underlay address | PASS: connection refused (listener bound to the overlay IP only) |
| 16 | Direct path transfer: `ping` + 64 MiB to `:10501` + `peers`/`traffic` | PASS: `direct`, 0.4 ms, 64 MiB in 0.17 s, peer tx delta 72,137,832 B |
| 17 | Relayed path: direct UDP between the two namespaces dropped (STUN/DERP to infra allowed) | PASS: `ping` path `derp` region `dacs`, 1.2 ms. 64 MiB in 25.7 s (≈2.5 MiB/s through a single-process socat TLS front), tx delta 72,492,496 B. `compiler-check` ok on `derp` |
| 18 | UDP unblocked | PASS: path back to `direct` (0.1 ms) |
| 19 | Revocation by node id (coordinator → netadmin) | PASS: `node_deleted: 7`, `verified_absent: true`; membership `revoked`; guest `NeedsLogin`; compiler endpoints unreachable from the coordinator |
| 20 | Reconnect attempt: guest tailscaled restarted with its old state | PASS (stays out): `logged_out`; not on the DACS network |
| 21 | Fresh approved enrollment and rejoin | PASS: new key id 9, joined as a **new node with a new overlay IP `100.64.0.6`**. `compiler-check` on the new endpoints passes (`direct`) |
| 22 | Secrets in logs | PASS: no `hskey-auth-` string and no operation token in the netadmin audit log or any harness log. The audit log has 6 entries (enroll/revoke) |

## Bugs found and fixed

**A. Overlay IPs never parsed from `tailscale status --json`** (`dacs-netclient/src/tailscale.rs`).
`#[serde(rename_all = "PascalCase")]` mapped `tailscale_ips` to `TailscaleIps`, but the CLI emits `TailscaleIPs`.
The Self and peer IPs were always empty. Effects: `join` never saw `Running` + IPs and always timed out
after a successful login, `status` reported `overlay_ips: []`, and `peers` reported no IPs. The fixture
contained the right key, but no test asserted the IPs. Fixed with an explicit rename; tests now assert
the parsed self and peer IPs.

**B. Join stalled after switching from another control server** (`dacs-netclient/src/lib.rs`).
Tailscale 1.104.7 kept the previous network's home DERP region (998), which is absent from the DACS DERP
map. The log showed `[unexpected] magicsock: derpMap.Regions[998] is nil` and `no-derp-connection`, and the
client stayed in `Starting` until `login --timeout` expired. A real personal tailnet uses Tailscale's
public DERP regions, so the same mismatch is expected there. A manual `tailscale debug restun` switched the
home region to 999 and reached `Running` within a second. `join` now nudges a client stuck in `Starting` on
the DACS control URL with a best-effort `debug restun` every 5 s while `login` runs. Success is still judged
by the existing `Running` + overlay-IP check.

## Constraints for other owners

- **Person 4:** overlay IPs are allocated sequentially and **change on re-enrollment** (`.5` → `.6` here).
  Address-bound compiler tokens and server registrations must be reissued after any re-enrollment. The
  host name (`compile-guest`, `compile-guest.dacs.internal`) is what stays stable. Bind sccache scheduler
  and server listeners to the node's overlay IP, not `0.0.0.0`. The ACL then limits callers, and the
  underlay address refuses (check 15).
- **Person 4:** the relayed path works but is much slower in this setup. Report the observed path per
  build (`dacs-net ping` / `compiler-check` / `peers` + `traffic`). Do not promise relay throughput.
- **Person 1:** `dacs-net join` accepts MagicDNS by default. Before the switch, `assess` returns the exact
  switch disclosure and `previous_control_url` for the setup UI's consent. A consent naming a different
  previous server is refused. Revocation returns the device to `logged_out`. Rejoining needs a new approved
  grant, and when the active profile is the revoked DACS one it requires no switch consent.
- **Harness note:** in network namespaces, give each namespace its own `/etc/netns/<ns>/resolv.conf`.
  Otherwise a tailscaled with accept-dns rewrites the distro-wide `/etc/resolv.conf` (seen in this run;
  fixed in `setup.sh`).

## Not run

- Public infrastructure: Linux server, DNS names, Let's Encrypt, the real nginx SNI router (a socat TLS
  front stood in for it) and public DERP/STUN reachability.
- Real fleet hosts, NAT traversal across real networks, and a VM guest on a second physical PC.
- The coordinator application's own `/api/network/*` bridge against this netadmin. Only netadmin was
  called directly, with the coordinator's operation token from the coordinator namespace.
- `leave` and switching back to the personal profile through DACS. A manual `tailscale switch` back
  worked while the harness was being reset.
- Real sccache traffic: stand-in TCP listeners were used on 10600/10501.
