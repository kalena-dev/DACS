# Two-machine demo over a LAN

Machine A runs the coordinator. Machine B enrolls as a worker device. This uses a routable LAN
address with application TLS and device authentication (allowed for development by the general plan);
it does **not** use the private overlay, which still needs public infrastructure.

Verified on 2026-11-14 at `main` after merging through `894c14c`, on one Windows PC against its LAN
address `172.30.80.30` (coordinator and "machine B" client as separate processes): steps 1-6 below.
Step 7 (running the worker with OMP) was **not** run here; it needs the pinned OMP and a provider
login on machine B. Replace `A_IP` with machine A's LAN address everywhere.

## Both machines

```sh
git pull origin main
cargo build -p dacs
```

The binary is `target/debug/dacs` (`dacs.exe` on Windows). Copying the Windows binary to machine B
instead of building also works if B has the MSVC runtime.

## Machine A: coordinator

1. Create a development certificate for the address B will use, then start the coordinator:

   ```sh
   dacs tls --hostname A_IP --output-dir tls
   dacs serve --listen 0.0.0.0:7443 --public-url https://A_IP:7443 --tls-cert tls/server.pem --tls-key tls/server.key --data-dir coord --host-name "Demo coordinator"
   ```

2. Allow inbound TCP 7443 through machine A's firewall (Windows: an inbound rule for port 7443 on the
   private profile; needs administrator rights). No rule exists by default.

3. Copy `tls/server.pem` (the certificate only, never `server.key`) to machine B.

4. Create a project and an invitation (a second terminal on A):

   ```sh
   echo '{"type":"create_project","name":"demo","target_branch":"main","max_tasks":16,"max_depth":2}' > p.json
   dacs request --url https://A_IP:7443 --token-file coord/admin.token --ca tls/server.pem --file p.json
   # copy "id" from the result into PROJECT_ID below
   echo '{"type":"create_invitation","project_id":"PROJECT_ID","host_name":"Demo coordinator","expires_in_seconds":3600}' > i.json
   dacs request --url https://A_IP:7443 --token-file coord/admin.token --ca tls/server.pem --file i.json
   # the result's "token" is the invitation token; give it to machine B privately
   ```

   The dashboard does the same in a browser: open `https://A_IP:7443`, sign in with the contents of
   `coord/admin.token`. The browser warns about the development certificate unless it is trusted.

## Machine B: device

5. Check reachability and view the invitation. Viewing has no side effects:

   ```sh
   curl --cacert server.pem https://A_IP:7443/health
   curl --cacert server.pem https://A_IP:7443/api/invitations/INVITATION_TOKEN
   ```

6. Enroll with the enrollment API. The onboarding page (`/onboarding`) only offers registration once
   a network contract is available, which needs the network infrastructure, so it cannot be used
   for this LAN demo.

   ```sh
   cat > enroll.json <<'EOF'
   {"invitation_token":"INVITATION_TOKEN","registration":{"name":"machine-b","physical_host_id":"host-b",
    "os":"windows","architecture":"x86_64","capabilities":["git"],
    "capacity":{"agent_slots":1,"cpu_millis":4000,"memory_mb":8192,"disk_mb":20000},
    "omp_version":"OMP_VERSION","policy_revision":1,"owner_consented":true,"consent_host":"Demo coordinator"}}
   EOF
   curl --cacert server.pem -H 'Content-Type: application/json' --data-binary @enroll.json https://A_IP:7443/api/enroll > enrollment.json
   ```

   `enrollment.json` holds `worker.id` and the secret `device_token`. Save the token alone (no
   newline) into a private file `device.token` and delete `enrollment.json`. The device is
   **pending** and is refused project state (`403 device is revoked or awaiting approval`) until
   machine A approves it:

   ```sh
   # on machine A; WORKER_ID is worker.id from the enrollment (also on A's dashboard)
   echo '{"type":"approve_worker","worker_id":"WORKER_ID"}' > a.json
   dacs request --url https://A_IP:7443 --token-file coord/admin.token --ca tls/server.pem --file a.json
   # on machine B: now succeeds and shows the project
   dacs snapshot --url https://A_IP:7443 --token-file device.token --ca server.pem
   ```

7. Run the worker. **Not run in this verification.** A teammate's single-host run
   (`docs/evidence/single-host-e2e.md`, macOS, fake OMP stand-in) used this configuration shape;
   on machine B it needs the real installed OMP and a completed provider login. Write
   `worker.json` (absolute paths; `registration` is the same object sent in step 6):

   ```json
   {"coordinator_url":"https://A_IP:7443","credential_file":"/abs/device.token","worker_id":"WORKER_ID",
    "registration":{ "...": "the registration object from step 6" },
    "omp_executable":"/abs/path/to/omp","omp_version":"OMP_VERSION","omp_profile":null,
    "data_dir":"/abs/b-state/worker","workspaces_dir":"/abs/b-state/workspaces",
    "ca_certificate":"/abs/server.pem","poll_interval_ms":500}
   ```

   ```sh
   dacs worker run --config worker.json
   dacs worker status --data-dir /abs/b-state/worker
   ```

   Machine A's dashboard then shows the worker's heartbeat and runtime status; create a task on the
   project to see admission and execution.


## What to show

- A's dashboard at `https://A_IP:7443`: project, pending device, approval, worker status.
- TLS is enforced: `dacs snapshot --url https://A_IP:7443 --token-file coord/admin.token` without
  `--ca` fails with `invalid peer certificate: UnknownIssuer`.
- Opening the invitation changes nothing; registration alone does not grant access.
- Task-board preview (loopback on either machine): see the README section "Private task-board preview".

## Known limits for this demo

- No private overlay, Headscale or relay: that needs a public Linux host and DNS.
- `crates/dacs-worker/tests/windows_workflow.rs` fails on a PC whose temp path is long (MSVC
  `LNK1181`, object paths past 260 characters) and passes with a short `TMP`/`TEMP` such as `C:\t`.
  If machine B's admitted Windows build fails at link time, set a short worker state directory.
