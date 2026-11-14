# dacs-netadmin local integration run: real Headscale and Tailscale in WSL

Date: 2026-11-14. Host: Windows 11 (10.0.26300, x64). Contract `dacs-net/2026-11-14.1`.
Secrets (Headscale API key, operation token, pre-auth keys) are redacted. All throwaway
configs and secrets were kept in `%TEMP%\dacs-hs-test\` (Windows) and `/root/hs`, `/root/ts`
(WSL). None are in the repo. The Windows host's own Tailscale stayed logged out the whole time
(`tailscale.exe status` → `Logged out.` after the run).

## Versions

| Component | Version / source |
|---|---|
| WSL distro | Ubuntu 24.04.5 LTS, WSL2, kernel 6.6.87.2-microsoft-standard-WSL2 |
| Headscale | `headscale_0.29.7_linux_amd64`, `headscale version` → `v0.29.7`, commit `c9cf210b8bc84d13177e18b91f555788a44b47b6` |
| Tailscale (WSL client) | static tarball `tailscale_1.104.7_amd64.tgz` from pkgs.tailscale.com, `1.104.7-tfb3b9b296-g0651cc45e` |
| dacs-netadmin | workspace build `cargo build -p dacs-netadmin` (cargo 1.100.0, MSVC), debug profile |
| Policy | `net/deploy/headscale/policy.hujson` copied unchanged (sha256 `8a7384b2…cf6f9a90` identical on both sides) |

## Results summary

| # | Step | Result |
|---|---|---|
| 1 | Install Ubuntu WSL distro without prompts | PASS (no reboot, no admin prompt) |
| 2 | Headscale v0.29.7 download, checksum, config, user, API key | PASS (one config change was needed: a DERP map is required, see 2c) |
| 3 | Build netadmin, `check`, `serve` | PASS (`headscale_url` had to be `http://localhost:8080`, see 3b) |
| 4a | POST /v1/enrollments → single-use, non-ephemeral, tags-only key | PASS |
| 4b | Real `tailscale up` with the grant key | PASS |
| 4c | Membership query → Joined, renamed, overlay IPs | PASS |
| 4d | GET /v1/nodes, GET /v1/compiler-endpoints (server :10501) | PASS |
| 4e | Same key from a second tailscaled state | PASS (refused: `authkey already used`) |
| 4f | Unauthenticated / wrong token → 401 | PASS |
| 4g | Consent host mismatch → 400, no key created | PASS |
| 5a | Revoke joined node → deleted, verified_absent | PASS |
| 5b | Revoked client loses the network, old state cannot reconnect | PASS |
| 5c | New grant, revoked before use → key_expired, KeyExpired, key refused by client | PASS |
| 6 | Response shapes vs `headscale.rs` decoding | PASS: no differences, no code changes |
| 7 | Stop headscale/tailscaled/netadmin | PASS |

The full flow (enroll → join → query → nodes → endpoints → revoke) was run twice: first with a
Python `urllib` driver (device `dev-wsl-01`, node 1), then again with plain `curl`
(device `dev-wsl-03`, node 2). The curl commands are shown below. Both runs gave the same results.

## 1. WSL distro

```
wsl -l -v            -> "Windows Subsystem for Linux has no installed distributions." (Default Version: 2)
wsl --install -d Ubuntu-24.04 --no-launch
                     -> "Distribution successfully installed." (46 s, no reboot/elevation prompt)
wsl -d Ubuntu-24.04 -u root -- id      -> uid=0(root)
printf '[user]\ndefault=root\n' > /etc/wsl.conf      (inside WSL)
```

## 2. Headscale

### 2a. Download and verify

```
cd /root/hs
curl -fsSLO https://github.com/juanfont/headscale/releases/download/v0.29.7/headscale_0.29.7_linux_amd64
curl -fsSLO https://github.com/juanfont/headscale/releases/download/v0.29.7/checksums.txt
sha256sum -c --ignore-missing checksums.txt
  -> e827fc9b12d958bc95b081c4401dd63f8e0471d025b45b508535a920532150a8  headscale_0.29.7_linux_amd64
  -> headscale_0.29.7_linux_amd64: OK
```

### 2b. Test config (`%TEMP%\dacs-hs-test\headscale-test.yaml` → `/root/hs/config.yaml`)

Based on `net/deploy/headscale/config.yaml`, with these changes:
`server_url: http://127.0.0.1:8080`, `listen_addr: 0.0.0.0:8080`, sqlite and keys under
`/root/hs/data`, `policy.path: /root/hs/policy.hujson`, `unix_socket: /root/hs/run/headscale.sock`,
no `trusted_proxies`. Same prefixes (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`), `node.expiry: 0`,
MagicDNS `dacs.internal`.

`headscale -c config.yaml configtest` → rc=0.

### 2c. Failure 1: DERP map required

With `derp.server.enabled: false` and `derp.urls: []`, `serve` exits:

```
Error: headscale ran into an error and had to shut down: initial DERPMap is empty, Headscale requires at least one entry
```

Fix (test config only): turned on the embedded DERP server, as in the deploy config
(`region_id 999`, `verify_clients: true`, `ipv4: 127.0.0.1`, `stun_listen_addr: 127.0.0.1:3478`).
The deploy config already enables embedded DERP, so this does not affect production. Headscale then
starts with `listening and serving HTTP on: 0.0.0.0:8080`. The client reports that it could not reach
the local DERP relay (DERP over plain HTTP on 127.0.0.1). This does not matter for control-plane
registration, which is what this run tests.

### 2d. User and API key

```
headscale users create dacs-admin      -> User created
headscale users list                   -> 1 | | dacs-admin | | 2026-11-14 18:32:31
headscale apikeys create --expiration 1d > /root/hs/apikey   (chmod 600; 88 bytes incl. newline)
headscale apikeys list                 -> 1 | hskey-api-5Pp2lOKggHUz-*** | 2026-11-15 18:32:32 | ...
headscale policy get                   -> the repo policy text (loaded OK; dacs-admin@ resolves)
```

Raw REST checks (from Windows, `Authorization: Bearer <api key>`):

```
GET /api/v1/health      -> 200 {"databaseConnectivity":true}
GET /api/v1/preauthkey  -> 200 {"preAuthKeys":[]}
GET /api/v1/node        -> 200 {"nodes":[]}
GET /api/v1/user        -> 200 {"users":[{"id":"1","name":"dacs-admin","createdAt":"2026-11-14T18:32:31.991261839Z",...}]}
bad API key             -> 401 "Unauthorized"
```

## 3. dacs-netadmin on Windows

### 3a. Build, token, check

```
cd net && cargo build -p dacs-netadmin        -> Finished `dev` profile (21 s)
dacs-netadmin gen-token > op-token 2> op-token.sha256
dacs-netadmin hash-token < op-token | diff - op-token.sha256   -> identical
dacs-netadmin check --config %TEMP%\dacs-hs-test\netadmin-test.json
  -> {"config":"ok","headscale":"ok","headscale_nodes":0,"operation_tokens":1,"tls":false}   rc=0
dacs-netadmin serve --config %TEMP%\dacs-hs-test\netadmin-test.json
  -> INFO dacs-netadmin serving listen=127.0.0.1:9443 tls=false
```

Test config: `listen 127.0.0.1:9443`, `allow_plaintext_loopback: true`,
`headscale_url: http://localhost:8080`, `login_server: https://hs.test.invalid` (placeholder),
coordinator `https://bootstrap.test.invalid` / `https://coordinator.dacs.internal:7443`,
`key_ttl_minutes: 15`, ports 10600/10501, audit log in `%TEMP%`.

### 3b. Failure 2 (environment): WSL localhost forwarding is IPv6-only on this host

`curl http://127.0.0.1:8080/health` from Windows → `connect ... failed: Connection refused`.
`curl http://localhost:8080/health` and `http://[::1]:8080/health` → `200 {"status":"pass"}`.
`netstat -ano` shows the WSL relay listening only on `[::1]:8080`. The fix was the config value
`headscale_url: http://localhost:8080`, which `is_loopback_url` already accepts. Inside WSL,
`127.0.0.1:8080` works, so the Tailscale clients used `--login-server http://127.0.0.1:8080`.
No netadmin change was needed.

`GET /v1/health` (no token) → `200 {"ok":true,"contract_revision":"dacs-net/2026-11-14.1","headscale_pinned":"v0.29.7","headscale_error":null}`.

## 4. Enrollment and join

Request body (`enroll3.json`; `dev-wsl-01`/`dacs-wsl-node1` in the first run):
`device_id dev-wsl-03`, `host_name dacs-wsl-node3`, `host_kind {"kind":"physical"}`,
`roles ["worker","compile_server"]`, consent `accepted_host = host_name`,
`disclosure_revision "host-admin-access/1"`, approval `approved_by coord-admin`,
`authorization_ref test-ticket-1`, current timestamps.

### 4a. Enroll

```
curl -X POST -H "Authorization: Bearer <op token>" -H 'Content-Type: application/json' \
     --data @enroll3.json http://127.0.0.1:9443/v1/enrollments
-> HTTP 200 {"contract_revision":"dacs-net/2026-11-14.1","device_id":"dev-wsl-03","host_name":"dacs-wsl-node3",
   "login_server":"https://hs.test.invalid","auth_key":"hskey-auth-<redacted>","auth_key_id":3,
   "tags":["tag:dacs-worker","tag:dacs-sccache-server"],"expires_at":"2026-11-14T18:51:42.863427Z",
   "coordinator":{...}}
```

`headscale preauthkeys list` (first run, key 1, before use):

```
ID | Key/Prefix                  | Reusable | Ephemeral | Used  | Expiration          | Created             | Owner
1  | hskey-auth-J_-wHRvLbnMc-*** | false    | false     | false | 2026-11-14 18:48:24 | 2026-11-14 18:33:24 | tag:dacs-sccache-server
   |                             |          |           |       |                     |                     | tag:dacs-worker
```

Raw REST shape for the same key: `{"user": null, "id": "1", "key": "hskey-auth-J_-wHRvLbnMc-***",
"reusable": false, "ephemeral": false, "used": false, "expiration": "2026-11-14T18:48:24.768094300Z",
"createdAt": "...", "aclTags": [...]}`. The key is tags-only (`user: null`), single-use, non-ephemeral,
and expires after the 15-minute TTL.

### 4b. Join with the real client (WSL)

```
curl -fsSLO https://pkgs.tailscale.com/stable/tailscale_1.104.7_amd64.tgz
sha256sum check against tailscale_1.104.7_amd64.tgz.sha256 -> OK
tailscaled --tun=userspace-networking --statedir=/root/ts/st3 --socket=/tmp/ts3.sock --port=41643
tailscale --socket=/tmp/ts3.sock up --login-server http://127.0.0.1:8080 \
          --auth-key file:/root/ts/key3 --hostname ubuntu-wsl-box          -> rc=0
tailscale status -> 100.64.0.2  ubuntu-wsl-box  tagged-devices  linux  -
```

A persistent `--statedir` was used instead of `--state=mem:` so that step 5b could restart a daemon
with its old state. The first run used `st1`, `--hostname wsl-os-name`, and got node 1, `100.64.0.1`.

`headscale nodes list` right after the join: `2 | ubuntu-wsl-box | ubuntu-wsl-box | ... | tagged-devices |
tag:dacs-sccache-server tag:dacs-worker | 100.64.0.2 fd7a:115c:a1e0::2 | false | ... | online`.

### 4c. Membership query

```
curl -X POST ... --data @query3.json http://127.0.0.1:9443/v1/memberships/query
   (query3.json: {"device_id":"dev-wsl-03","host_name":"dacs-wsl-node3","auth_key_id":3,"node_id":null})
-> HTTP 200 {"device_id":"dev-wsl-03","host_name":"dacs-wsl-node3","state":{"state":"joined","node_id":2,"online":true},
   "overlay_ips":["100.64.0.2","fd7a:115c:a1e0::2"],"tags":["tag:dacs-sccache-server","tag:dacs-worker"],
   "last_seen":"2026-11-14T18:37:02.228777135Z"}
```

The rename route (`POST /api/v1/node/{id}/rename/{name}`) worked. `headscale nodes list` then showed
`Hostname ubuntu-wsl-box | Name dacs-wsl-node3`, and the client's own `status --json` showed
`"DNSName": "dacs-wsl-node3.dacs.internal."`. netadmin logged no rename warning. Repeating the query
returns the same Joined state. A query by `node_id` only (`{"auth_key_id":null,"node_id":1}`, first run)
also returned `joined`.

### 4d. Nodes and compiler endpoints

```
curl -H "Authorization: Bearer <op token>" http://127.0.0.1:9443/v1/nodes
-> HTTP 200 [{"node_id":2,"name":"dacs-wsl-node3","overlay_ips":["100.64.0.2","fd7a:115c:a1e0::2"],
   "tags":["tag:dacs-sccache-server","tag:dacs-worker"],"online":true,"last_seen":"2026-11-14T18:37:02.228777135Z"}]
curl -H "Authorization: Bearer <op token>" http://127.0.0.1:9443/v1/compiler-endpoints
-> HTTP 200 [{"host_name":"dacs-wsl-node3","role":"server","overlay_ip":"100.64.0.2","port":10501}]
```

### 4e. Key reuse from a second state

```
tailscaled --tun=userspace-networking --statedir=/root/ts/st2 --socket=/tmp/ts2.sock --port=41642
tailscale --socket=/tmp/ts2.sock up --login-server http://127.0.0.1:8080 --auth-key file:/root/ts/key1 --hostname wsl-reuse
-> rc=1  "backend error: authkey already used"; headscale nodes list still only node 1; key 1 Used=true
```

### 4f. Authentication

```
curl -X POST --data @enroll3.json http://127.0.0.1:9443/v1/enrollments     (no token)
-> HTTP 401 {"code":"unauthorized","message":"operation token required"}
GET /v1/nodes with "Bearer dacs-op-wrong"
-> HTTP 401 {"code":"unauthorized","message":"invalid operation token"}
```

### 4g. Consent host mismatch

```
curl -X POST ... --data @enroll-mismatch.json http://127.0.0.1:9443/v1/enrollments
   (consent.accepted_host "someone-else" != host_name "dacs-wsl-node4")
-> HTTP 400 {"code":"bad_request","message":"inconsistent request: owner accepted a different host name"}
GET /api/v1/preauthkey before/after -> same key ids (first run: [] -> []; curl run: 1,2,3 -> 1,2,3)
```

Duplicate enrollment for a host name that is already registered (first run) →
`409 {"code":"conflict","message":"host name dacs-wsl-node1 is already registered as node 1; revoke it before re-enrolling"}`.

## 5. Revocation

### 5a. Revoke a joined node

```
curl -X POST ... --data @revoke3.json http://127.0.0.1:9443/v1/revocations
   (revoke3.json: {"device_id":"dev-wsl-03","auth_key_id":3,"node_id":null,"requested_by":"coord-admin","reason":"curl integration test"})
-> HTTP 200 {"device_id":"dev-wsl-03","key_expired":false,"node_deleted":2,"verified_absent":true,"at":"2026-11-14T18:37:20.6850261Z"}
memberships/query -> {"state":{"state":"revoked","node_id":null},"overlay_ips":[],"tags":[],"last_seen":null}
/v1/nodes -> []      /v1/compiler-endpoints -> []
headscale nodes list -> (empty)
```

In the first run, revoking with both ids (`auth_key_id 1, node_id 1`) returned
`{"key_expired":false,"node_deleted":1,"verified_absent":true}`. Using only `auth_key_id` (above) found
the node through `preAuthKey.id`.

### 5b. Revoked client loses the network

A few seconds after the revoke, on the client:

```
tailscale --socket=/tmp/ts3.sock status   -> "Logged out."  rc=1
status --json                             -> "BackendState": "NeedsLogin", "Online": false
```

The first-run daemon (`st1`) was stopped and restarted with its old state file:

```
control: RegisterReq: onode= node=[2b22v] ...
control: RegisterReq: got response; nodeKeyExpired=false, machineAuthorized=false; authURL=true
Switching ipn state NoState -> NeedsLogin
headscale nodes list -> (empty)
tailscale up ... --auth-key file:/root/ts/key1 --force-reauth -> rc=1 "backend error: authkey already used"
```

The old node key is no longer accepted. Headscale only offers interactive registration, and nobody
approves that, so the node never comes back.

### 5c. Grant revoked before use

```
POST /v1/enrollments (dev-wsl-01 / dacs-wsl-node1)          -> 200 auth_key_id 2
POST /v1/memberships/query {auth_key_id:2}                  -> {"state":"key_issued","auth_key_id":2,"expires_at":"2026-11-14T18:50:57.0757276Z"}
POST /v1/revocations {auth_key_id:2,node_id:null,...}       -> 200 {"key_expired":true,"node_deleted":null,"verified_absent":true}
POST /v1/memberships/query {auth_key_id:2}                  -> {"state":"key_expired","auth_key_id":2}
headscale preauthkeys list -> 2 | hskey-auth-1MtUVrFTyscD-*** | false | false | false | 2026-11-14 18:35:57 (expired now)
tailscale --socket=/tmp/ts2.sock up ... --auth-key file:/root/ts/key2  -> rc=1 "backend error: authkey expired"; no node created
```

## 6. Response shapes compared with `headscale.rs`

All real v0.29.7 shapes decoded without changes:

- Field names are lowerCamelCase (`preAuthKeys`, `aclTags`, `ipAddresses`, `givenName`, `lastSeen`,
  `preAuthKey`, `databaseConnectivity`).
- uint64 values are JSON strings (`"id":"1"`; the user id is `"2147455555"`, which is `tagged-devices`).
- Timestamps are RFC 3339 with up to 9 fractional digits (`2026-11-14T18:48:24.768094300Z`). Unset values
  come as `null` (`"expiry": null`) or as the zero time (`tagged-devices.createdAt "0001-01-01T00:00:00Z"`).
  The undecoded fields are ignored.
- A tags-only key has `"user": null`, which is ignored. The create body without `user` was accepted.
  The key in list responses is masked (`hskey-auth-…-***`); the full key appears only in the create response.
- Nodes carry a single `tags` array (no forced/valid split).
- 404 mapping: `DELETE /api/v1/node/999` and `GET /api/v1/node/999` →
  `404 {"code":5,"message":"node not found"}`, so `is_not_found()` is correct.
- The rename route works as implemented.

Observed but harmless for the current code:

- `POST /api/v1/node/999/rename/foo` (missing node) → `500 {"code":2,"message":"node no longer exists in NodeStore: 999"}`,
  not 404. netadmin only renames a node it just listed. A rename failure is logged as a warning and does
  not fail the query.
- `POST /api/v1/preauthkey/expire {"id":"999"}` (missing key) → `200 {}`. netadmin expires only keys it
  just found by id.

`cargo test -p dacs-netadmin` → `14 passed; 0 failed`. Since no code changed, no new unit tests were added.

## 7. Shutdown

headscale, all three tailscaled instances and dacs-netadmin were stopped.
`pgrep -af 'tailscale|headscale'` in WSL → nothing. `tasklist` shows no `dacs-netadmin.exe`.
The Ubuntu-24.04 distro and the throwaway files in `%TEMP%\dacs-hs-test` and `/root/{hs,ts}` are
still there. They contain only test secrets; the Headscale API key expires on 2026-11-15.

## Not run, and why

- TLS on netadmin (`tls` block) and the nginx SNI router: not part of this local test. Plaintext on
  loopback was used as the task allowed.
- Traffic between two joined DACS nodes (policy grants on 7443/10501/10600) and the `dacs-net`
  client CLI: outside this task's targets. The embedded DERP was not reachable by the client in this
  setup, so peer data-plane checks would not have meant anything.
- Tailnet-switch consent (`tailnet_switch`), guest `host_kind`, and the scheduler/coordinator roles:
  only `worker + compile_server` with a physical host was enrolled, as the task specified.
