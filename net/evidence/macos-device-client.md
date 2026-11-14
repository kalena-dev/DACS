# macOS device client: read-only evidence with a stopped existing login

Date: 2026-11-14, 20:16–20:25 UTC. Contract `dacs-net/2026-11-14.1`, disclosure `host-admin-access/1`.
Host: macOS 27.0.1 (build 26A434), arm64. `dacs-net` debug build at `1e41096`, whose `net/crates`
tree is identical to `86a1d1b` on `main`. A first pass at 20:16 UTC with a build of `f55ff7f` gave the
same classification.

**Read-only.** Only `tailscale version`, `tailscale status --json`, `tailscale debug prefs` (piped
straight into a field filter) and `dacs-net status` / `assess` / `preflight` ran. No `tailscale
up/login/logout/switch/set`, no `sudo`, no daemon start, no network settings change, no server binaries.
`assess` names a placeholder login server (`https://hs.dacs.test`). It is only compared as a string and
never contacted. No DACS control server, netadmin, coordinator or Headscale existed for this run.

Redacted: tailnet name, host and DNS names, overlay addresses, node/user IDs and keys, peer names
and the public DERP region name. Kept: versions, the public Tailscale control URL, states, booleans and counts.

## Client installation

| Item | Observed |
|---|---|
| CLI on `PATH` | `/usr/local/bin/tailscale`: 68-byte `/bin/sh` wrapper running `/Applications/Tailscale.app/Contents/MacOS/Tailscale "$@"` |
| Variant | Standalone app ("macsys"). Its network system extension `io.tailscale.ipn.macsys.network-extension` was running. The GUI app was not running |
| `tailscale version` | `1.102.4`, tailscale commit `3caf7d9e7dca…`, long version `1.102.4-t3caf7d9e7-g084ee3b64`, go1.26.6. The WSL runs used 1.104.7 |
| `dacs-net` CLI discovery | Found through `PATH` (`DACS_TAILSCALE_BIN` unset). `Tailscale::locate()` also lists the app binary as a macOS default |

## Commands and outputs

### `tailscale status --json` (exit 0, stderr empty, 5743 bytes; summarized)

```json
{
  "Version": "1.102.4-t3caf7d9e7-g084ee3b64",
  "BackendState": "Stopped",
  "HaveNodeKey": true,
  "AuthURL": "",
  "TUN": true,
  "Self": {"OS": "macOS", "Online": false, "InNetworkMap": true,
           "TailscaleIPs": ["<100.64.0.0/10 address>", "<fd7a:115c:a1e0::/48 address>"]},
  "CurrentTailnet": {"Name": "<redacted>", "MagicDNSSuffix": "<redacted>", "MagicDNSEnabled": true},
  "Peer": {"<1 peer>": {"Online": true, "CurAddr": "", "Relay": "<public DERP region>"}},
  "Health": [
    "Tailscale is starting. Please wait.",
    "Tailscale is stopped.",
    "Tailscale could not connect to the '<public DERP region>' relay server. Your Internet connection might be down, or the server might be temporarily unavailable."
  ]
}
```

### `tailscale debug prefs | jq '{ControlURL, LoggedOut, WantRunning}'`

```json
{"ControlURL": "https://controlplane.tailscale.com", "LoggedOut": false, "WantRunning": false}
```

### `dacs-net status` (exit 0, stderr empty)

```json
{
  "state": "logged_in",
  "control_url": "https://controlplane.tailscale.com",
  "backend_state": "Stopped",
  "tailnet": "<tailnet redacted>",
  "self_name": "<host name redacted>",
  "overlay_ips": ["<100.64.0.0/10 address redacted>", "<fd7a:115c:a1e0::/48 address redacted>"]
}
```

### `dacs-net assess https://hs.dacs.test` (exit 0, stderr empty)

```json
{
  "login_server": "https://hs.dacs.test",
  "client": {"state": "logged_in", "control_url": "https://controlplane.tailscale.com", "backend_state": "Stopped", "...": "same as status"},
  "already_dacs": false,
  "requires_switch_consent": true,
  "previous_control_url": "https://controlplane.tailscale.com",
  "disclosure": "This host is already connected to another Tailscale network. The Tailscale client runs one network at a time: … DACS never switches networks without this approval."
}
```

`disclosure` is exactly `TAILNET_SWITCH_DISCLOSURE`.

### `dacs-net preflight --config net/deploy/netadmin/preflight.example.json` (exit 1)

```
error: replace example endpoint placeholders with owner-supplied deployment addresses
```

The template's `https://HS_DOMAIN` is refused before any DNS lookup or connection. A live preflight
needs owner-supplied endpoints, and none exist.

### No-change check

Before and after the three `dacs-net` commands (20:25:16–20:25:17 UTC), SHA-256 digests were taken of
`{BackendState, HaveNodeKey, Version, Self.TailscaleIPs, CurrentTailnet.Name, Peer count}` and of
`{ControlURL, LoggedOut, WantRunning}`. Both were identical: `c8169e359730aa21` / `cf9e06b65a30b733`
before and after.

## Findings

1. **"Stopped" is not logged out on macOS.** With the app stopped, the network extension still answers
   LocalAPI. It reports `BackendState: Stopped`, `WantRunning: false`, `LoggedOut: false`, a retained node
   key, overlay addresses and the tailnet. `dacs-net` correctly reports `logged_in` / `Stopped`, and
   `assess` requires switch consent naming `https://controlplane.tailscale.com`. Joining DACS would
   displace this login. DACS would not do that silently, because `check_join` requires a matching
   `TailnetSwitchConsent`. This exact shape is now a fixture in
   `crates/dacs-netclient/tests/fake_tailscale_cli.rs` and `tests/join_preconditions.rs`.
2. **`daemon_unavailable` was not observed** on this Mac, because the extension was running. The
   prerequisite path is covered only by a fixture (a fake CLI that fails like an unreachable service).
3. **Stale netmap while stopped.** The stopped client still lists its peer as `Online: true` with a
   public DERP `Relay`. Passive `dacs-net peers` would therefore report a `derp` path although no
   tunnel is up. Path/transfer evidence must come from active probes (`ping`, `compiler-check`)
   taken while the client is `Running` on the DACS control URL.
4. **Version skew.** 1.102.4 parses identically for `status --json` and `debug prefs`. `join`
   (`tailscale login --auth-key=file:…`) and the stale-home-DERP `debug restun` nudge were verified
   only on 1.104.7 in WSL. Whether `login` brings the standalone app's tunnel up while
   `WantRunning: false` was not tested here.

## Gate table (this macOS host)

| Gate | Result | Evidence / reason |
|---|---|---|
| Network-client prerequisite present and found by `dacs-net` | Passed | Wrapper on `PATH`, standalone app 1.102.4 |
| `status` / `assess` / `preflight` make no client or network change | Passed | Identical state digests. Process-level tests confirm `status`/`assess` issue only `status --json` and `debug prefs` |
| Existing control-server login detected, including a stopped one | Passed | `logged_in` / `Stopped`, `previous_control_url` = Tailscale SaaS |
| Displacement disclosure and explicit approval required | Passed | `requires_switch_consent: true`, exact disclosure text |
| `join` without, or with mismatched, switch consent runs no network command | Passed against a fake CLI only | `fake_tailscale_cli.rs`: no `login/up/switch/logout/set/down` issued. Real macOS client: not run. Real Linux client: passed in `wsl-netns-e2e.md` checks 8–9 |
| Auth key passed only via a private file, removed after `login`, never on argv/stdout | Passed against a fake CLI only | `consented_join_passes_key_by_private_file_and_removes_it`. Real macOS: not run |
| `leave` touches only the DACS profile | Passed against a fake CLI only | Refuses the SaaS profile; one plain `logout` for DACS. Real macOS: not run |
| `daemon_unavailable` / `not_installed` blockers | Fixture only; not observed on macOS | Extension was running |
| Preflight against live bootstrap/Headscale/control/netadmin | Not run | No endpoints exist. Template refused before DNS/TLS |
| Public installation and invitation opening join no network (macOS) | Not run | Person 1's installer/invitation flow was not exercised here |
| Named-host acceptance, individual approval, unique single-use device credentials | Not run on macOS | Unit tests; real Headscale in `local-headscale-integration.md` and `wsl-netns-e2e.md` |
| Consented tailnet switch and DACS join from macOS | Not run | Would displace the owner's login; no DACS server |
| Authenticated worker connection over the overlay | Not run | No coordinator/overlay |
| Revocation and reconnect from macOS | Not run | No DACS server |
| Direct vs relayed path and compiler reachability from macOS | Not run | No DACS overlay. `wsl-netns-e2e.md` is single-machine only |
| Public DNS/TLS/DERP | Not run | No server or domain |
| Linux compile guest as its own node | Not applicable here | Single-machine WSL pass in `wsl-netns-e2e.md`. Real VM on a second PC not run |
