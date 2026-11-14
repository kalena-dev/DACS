## Race conditions and defects

Nothing was built or run: the review is read-only and both test files are `#![cfg(unix)]` on a Windows host. Kernel behaviour I rely on but did not read here is marked [INFERENCE].

**1. high — confirmed by reading — `crates/dacs-worker/src/supervisor.rs:428-478` (called at `:188`, kill at `:190`). An unrelated process group in the worker's own session is still SIGKILLed after PID reuse.**
- **What the check proves:** only that every member has the recorded session number and `start_time() >= launched_at`. The time bound cannot reject a PID-reuse group, because anything that recycled the PID started after the launch. The session is the launching worker's (`:230`), which a manually launched worker shares with every other job in that terminal or login.
- **Interleaving:**
  1. Worker in session S launches leader L; the receipt holds `pid=L, session_id=S, launched_at=T0`.
  2. The worker is SIGKILLed. The orphaned tree then exits on its own; the receipt stays, because only `terminate` (`:348`) and spawn failure (`:251`) clear it.
  3. Before the next manual start, PID L is recycled as leader of an unrelated group in session S (for example an owner pipeline from the same terminal), and that leader exits while members keep running.
  4. On restart `system.process(L)` is `None`, all members pass both checks, and `terminate_group(L)` kills the pipeline.
- **Reboot variant:** receipts carry no boot identity and S is itself a recyclable PID. After a reboot or container restart, a different session can carry the same number S and a new group the number L.
- **Impact:** SIGKILL of processes DACS never launched, contradicting `:422-423` ("DACS never signals processes it cannot attribute to its own launch").
- **The test shows it:** `crates/dacs-worker/tests/crash_recovery.rs:319-321` kills a group no `Supervisor` launched, using a hand-written receipt.
- **Smallest fix:** put a per-launch random token in the child environment and the receipt, and require it in every live member's environment (refuse if unreadable). Also record a boot id and never signal on a receipt from another boot.

**2. medium — confirmed by reading (sysinfo); kernel side [INFERENCE] — `supervisor.rs:468` with `:223-226`. A genuine descendant is refused as "started before the recorded leader" on Linux.**
- **Cause:** `launched_at` is the floor of the real wall clock. sysinfo's Linux `start_time()` is `btime + starttime_ticks / CLK_TCK` (`sysinfo-0.37.2/src/unix/linux/process.rs:418-422, 554-556`; `system.rs:117-158`), which is two separate floors and can be one second below the floor of the true start.
- **Interleaving:** a descendant forks in the same wall second as the launch, which is the normal case. If the fractional parts of boot time and uptime-at-fork sum to 1 or more, `start_time() == launched_at - 1` and `:468` rejects it.
- **Impact:** the receipt is kept and `runtime.rs:389` aborts every worker start until someone kills the group and deletes the JSON by hand. This brings back the "restart refused forever" case that cb4ec11 removed, for a per-boot fraction of launches.
- **Test effect:** `worker_crash_with_live_process_trees_is_reconciled_on_restart` (`crash_recovery.rs:121`) forks `sleep 600` milliseconds after launch, so it becomes intermittently failing on Linux hosts.
- **Related:** a backward wall-clock step of 1 s or more between launch and restart has the same effect, because `btime` moves and `launched_at` does not.
- **Smallest fix:** compare with slack (`process.start_time() + 2 < started`). Finding 1 shows the bound has no discriminating power to lose.

**3. low — confirmed by reading — `supervisor.rs:165-190`. Nothing pins the group between attribution and the kill.**
- **Interleaving A:** the verified members exit after the snapshot at `:451`, group L disappears, PID L is recycled as a new group leader, and `killpg` at `:190` kills the unverified group.
- **Interleaving B:** L is recycled between the leader snapshot (`:165`) and the member snapshot (`:451`). The new group, including a live leader, is then judged only by session and time; the start-time equality check at `:168-171` is never applied.
- **Likelihood:** both need PID allocation to wrap onto L within milliseconds.
- **Smallest fix:** on Linux, open a pidfd per verified member and signal through those. Otherwise re-check leader absence and membership immediately before `killpg`.

**4. low — confirmed by reading — `supervisor.rs:188` (`?`). One unattributable receipt stops the whole pass, in directory order.**
- **Interleaving:** receipt A has a live, matching leader; receipt B is a pre-3028ec9 receipt (no `session_id`) with a leaderless live group. If `read_dir` yields B first, the pass returns an error and A's tree keeps running with dead pipes on every start attempt.
- **Impact:** after upgrading, any old-format leaderless receipt blocks startup and leaves other orphaned trees alive. The message says "owner must inspect it", but no command resolves it.
- **Smallest fix:** process all receipts, collect the refusals, and return the error after the loop.

**5. low — confirmed by reading — `crash_recovery.rs:253-268`, `supervisor.rs:445`. The tests exercise a branch production never takes.**
- `write_receipt` never writes `launched_at`, so all four staged cases run on the `start_time` fallback.
- Every receipt written by `launch` has `launched_at`, so the production branch (and its precedence over `start_time`) has no direct test.
- On Linux the crash test still passes if `launched_at` recording is removed, because the zombie leader's start time is observed and the fallback applies [INFERENCE].
- **Smallest fix:** write `launched_at` in `write_receipt` and add a case with `start_time` absent.

**6. low — confirmed by reading — `crash_recovery.rs:246-249` versus `:286`/`:301`. A fixture can leak before its guard exists.**
- The asserts inside `leaderless_group` run before the caller builds `GroupGuard`. If the shell fails after starting `sleep` (for example `leader.pid` cannot be written), `sleep 600` survives for ten minutes holding the inherited stderr.
- A harness that reads the test's stderr to EOF then waits that long.
- **Smallest fix:** set `.stderr(Stdio::null())` and return the guard from `leaderless_group`.

## Documentation mismatches

- **Windows/macOS join is dropped from the handoff.** `docs/remaining-work.md:15`, under "Already done: do not repeat", says: "Real WSL namespace clients passed TLS netadmin access, bootstrap/device join, switch-consent checks, revocation/rejoin…". `net/README.md` (Verification status) says: "Passed in the single-machine WSL namespace run, **Linux client only**, after two client fixes found by this run … Windows/macOS `join` and switching on a real host with an existing login: not run". Section 3 has no item for that gap, and the bullet omits that rows 3 and 10 were "**First run FAILED**".
- **The "personal tailnet" was a stand-in.** `remaining-work.md:45` speaks of "the previous personal tailnet". `net/evidence/wsl-netns-e2e.md` says: "Second Headscale (A) … Stands in for a personal tailnet."
- **"Return to the previous personal tailnet" is missing code, not a missing run.** `remaining-work.md:45` says: "Test DACS `leave` and return to the previous personal tailnet". `leave` only runs `logout` (`net/crates/dacs-netclient/src/lib.rs`), there is no `"switch"` call anywhere in `net/crates/dacs-netclient/src`, and `net/README.md:173` says it "logs out the DACS profile only". The new test pins exactly one plain `logout`. This conflicts with the handoff's premise "do not … treat every unchecked item as missing code".
- **Task-board acceptance is listed as done while an owner check is still open.** `remaining-work.md:14` says: "Browser task-board CRUD, status/text filtering, keyboard interaction, narrow layout and persistence through backend restart passed. Later frontend-owner checks also covered focus restoration and inline title errors."
  - `examples/task-board-web/ACCEPTANCE.md` says: "Person 4 still owns independent browser acceptance." No remaining item carries that.
  - Rows 7d and 7e read "**Failed before the fix**" and row 9 "**Defect before the fix**", all "Pass (after fix)". This was one frontend-owner run with reruns, not an earlier pass plus later checks.
  - The handoff also omits: "Tested only in Chromium with synthetic keyboard input."
- **Headscale key co-location rule is weaker than the README and already breached by the evidence.** `remaining-work.md:50` says: "Headscale administration keys stay off agent PCs; if co-location is unavoidable, obtain and record acceptance of that risk." `net/README.md:404-407` says: "They never go to agent PCs, the coordinator, invitation links or worker configuration." The evidence harness expects "`/root/hs` (Headscale binary, config, sqlite DB, API key)" on the same PC and says "Only `/root/hs` and `/root/ts` … remain in the distro", with no acceptance recorded.
- **`preflight` is not carried over.** The README row "Read-only `preflight` | Implemented; no live endpoints supplied, not run" has no counterpart in `remaining-work.md`.
- **The focused tests cannot run on the handoff's computer.** `remaining-work.md:7` says "Check their focused tests" for the incoming supervisor and network changes. Both files are `#![cfg(unix)]`, so on Windows `cargo test` reports success with zero of these tests; they need WSL or Linux.
- **Evidence-internal contradiction (outside the target, but the comparison depends on it).** `wsl-netns-e2e.md` row 21 says: "`compiler-check` on the new endpoints passes (`direct`)". `net/evidence/wsl-e2e/logs/revoke.log:38` shows both endpoints at `100.64.0.6` with `"reachable":false`, `"error":"Connection refused (os error 111)"`, `"ok":false`; only the path is `direct`. Compiler reachability is evidenced before revocation only (`paths.log:16,34`).
- **Consistent with the evidence:**
  - `remaining-work.md:43` — netadmin was called directly; the `/api/network/*` bridge was not run.
  - `:46` — the automatic `restun` nudge is unverified; only the manual one was observed.
  - `:47` — overlay IPs changed on re-enrollment (`.5` → `.6`).
  - `:48` — public infrastructure, NAT and real-network paths were not run.
  - `:15` — "stand-in TCP listeners, not sccache".

## Cleanup candidates

- `supervisor.rs:420-421` and the message at `:470` say "recorded leader", but the bound is `launched_at` first (`:445`). The test asserts on that string (`crash_recovery.rs:314`).
- `supervisor.rs:445`: the `.or(receipt.start_time)` fallback is reachable only by receipts no writer produces, since `session_id` and `launched_at` arrived in the same commit.
- `crash_recovery.rs:308`: `assert!(reconcile(&receipts).is_err())` accepts any error; the other two refusal cases check the message.
- `fake_tailscale_cli.rs:377-387`: the second-leave assertion is vacuously true on zero calls, and the fake exits 0 for any `debug …` subcommand.
- `crash_recovery.rs:226`: the new-session fixture requires `perl` on PATH and fails rather than skips without it.

## Checked and found sound

- **Live recorded PID:** it is signalled only when PID and start second both match (`supervisor.rs:167-171`). A recycled PID with a live process is refused and nothing is signalled.
- **Crash between the pre-spawn record (`:232`) and the PID record:** the receipt has `pid: None` and is refused at `:162-164` with no signal. No spawned tree exists without a receipt.
- **Leaderless group in another session, with a pre-launch member, or with an old receipt:** all refused (`:463-467`, `:468`, `:446-450`). Genuine members always carry the recorded session, because the wrapper only changes the group and a descendant that calls `setsid` leaves it.
- **Two workers or two passes:** `runtime.rs:371-378` takes an exclusive `worker.lock` and the host owner lock before `reconcile_previous` (`:389`).
- **Windows:** reconciliation never signals. A live matching leader bails (`supervisor.rs:172-177`). Descendants are handled by the kill-on-close job (`process-wrap-10.0.1` `tokio/job_object.rs:86`, `windows.rs:154-155`). `session_id` stays `None` and `launched_at` is unused.
- **The new crash_recovery tests do test the change:**
  - Without `verify_leaderless_group`, `:289` panics on `unwrap_err` and `:308`/`:312` fail.
  - The session comparison, time bound and missing-session refusal each have one case that fails when removed.
  - There are no fixed sleeps, each test has its own tempdir, and the fixture leader is reaped by `.status()`.
- **fake_tailscale_cli isolation:** `DACS_TAILSCALE_BIN`, `FAKE_TS_DIR` and `FAKE_EXPECT_KEY` are set on the child only, each `Fake` has its own tempdir, the key file has a random temp name (`lib.rs:256`), and the current directory is not used.
- **The new fake_tailscale_cli tests fail on regression:**
  - `leave_logs_out_only_the_active_dacs_profile_once` (`:357`) fails if URL normalization regresses, if any other mutating call or a `logout` with arguments appears, or if the second leave logs out again.
  - `join_when_already_on_dacs_does_not_rejoin` (`:390`) fails if the `AlreadyJoined` check is removed, because the fake `login` would succeed, giving exit 0 and a recorded mutation.

## Not covered

- macOS behaviour of sysinfo (zombie visibility, start-time source) was not read.
- Kernel facts used above (truncated `btime`, PID not reallocated while it names a group or session) come from knowledge, not from sources in this repository.
- The fake's post-logout state is the test's own invention; the real Tailscale shape after `logout` from Headscale was not observed.
- Suspected, outside the diff: `revoke.log:10` shows a revoked client as `"state":"logged_in"` on the DACS URL with `NeedsLogin`. `check_join` would then return "already joined" until tailscaled restarts. The new test covers only `Running`.
- On Windows, `secure_directory` (`runtime.rs:388`) launches through the persistent supervisor before reconciliation; receipt id collisions there were not traced.
- Non-network sections of `remaining-work.md` and its other linked documents were not compared. The evidence logs were read only where a claim depended on them.