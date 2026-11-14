## Race conditions

Review was read-only: nothing was built or run, so every item is "confirmed by reading" unless marked suspected.

### 1. A completed journal permanently short-circuits any later plan with the same digest
- **Severity / confidence:** high / confirmed by reading.
- **Location:** `crates/dacs-setup/src/engine.rs:154`, `engine.rs:209-211`; digest inputs at `catalogue.rs:779-785` and `catalogue.rs:928`.
- **Interleaving:** the plan digest covers only the request, platform and detected state/version, with no nonce or timestamp. Plan A (say `repair git 2.47.1`, or `install` while git is missing) runs and `setup-<sha>.json` becomes `completed`. Git is later removed or breaks again; a fresh plan is byte-identical, so it gets the same sha. `apply` loads the old journal and returns it at line 210 before any probe or command.
- **Impact:** the UI reports `completed` with old timestamps while nothing was installed. That request can never run again in this state directory until the journal file is deleted by hand. The same collision makes a new plan inherit an old `Failed`/`Running` journal and demand `resume_interrupted`.
- **Smallest fix:** add a random `plan_id` and `created_at` to `Plan` at `catalogue.rs:928` so the digest is unique per review. Alternatively, on `completed`, re-probe every step and fall through if any is not `Installed`.

### 2. Self-update swap leaves no executable at the target path if interrupted
- **Severity / confidence:** high / confirmed by reading.
- **Location:** `crates/dacs-setup/src/update.rs:637-651` (apply), `update.rs:531-539` (rollback).
- **Interleaving:** line 637 renames target to backup. Line 643 then fsyncs a journal write and line 644 hashes the whole candidate. Only at line 651 is the candidate renamed to target. A crash, kill or power loss anywhere in between leaves `target` absent.
- **Impact:** the recovery branch at line 638 exists, but its only automatic caller is `reconcile_update` from the app's own startup (`dacs-app/src/main.rs:441`). That cannot run because the app binary is gone from its path. Nothing relaunches the leftover `update-helper-<uuid>`, so recovery is manual.
- **Ordering defect inside the window:** the candidate digest is checked only after the original has been moved away, so a missing or quarantined candidate is discovered too late and forces a rollback.
- **Smallest fix:** move the candidate check (644-650) above line 637. Then make the swap never leave the target absent: on Unix `hard_link(target, backup)` followed by a replacing `rename(candidate, target)`; on Windows `ReplaceFileW(target, candidate, backup)`.

### 3. A transient swap failure is recorded as an explicit rollback and blacklists the release
- **Severity / confidence:** medium / confirmed by reading.
- **Location:** `update.rs:644-655`, `update.rs:544`, `update.rs:589-593`, `update.rs:565-567`.
- **Interleaving:** `rename_wait(candidate, target)` fails once (any I/O error), or the candidate digest check fails. The code saves `rollback_pending` and calls `recover_rollback`, which restores the backup and saves `rolled_back`. On retry, `apply_staged_update` reuses that journal because `next_sha256` matches, and `apply_journal` returns "explicitly rolled back; stage a newer release". `stage_update` meanwhile answers `reused: true`.
- **Impact:** one transient error makes this signed release unappliable on the host for good. With `--update-manifest-url` set, `startup_update` propagates that error (`main.rs:446-456`, outside my slice), so `serve` fails on every launch while the manifest advertises that version. A genuine user rollback hits the same startup failure.
- **Second defect:** `let _ = recover_rollback(..)` at lines 646 and 653 discards its error. If the restore rename also fails, the journal stays `rollback_pending` with the target missing, which is the state in finding 2.
- **Suspected contributor (std mapping not checked):** the Windows retry at `update.rs:474` matches only `PermissionDenied`/`WouldBlock`. I believe `ERROR_SHARING_VIOLATION` maps to neither, so an antivirus handle on the new file would fail immediately instead of retrying for 60 seconds.
- **Smallest fix:** use a distinct phase such as `apply_failed` that permits re-apply, reserve `rolled_back` for `rollback_update`, and propagate the `recover_rollback` error.

### 4. Terminal journal phases are re-enforced against the live binary on every startup
- **Severity / confidence:** medium / confirmed by reading. This is state staleness rather than an interleaving.
- **Location:** `update.rs:594-599`, `update.rs:382-385`, `update.rs:411`, `update.rs:734`, `update.rs:343`.
- **Sequence:** an update completes, so the journal is `completed` with `next_sha256 = X`. The binary is later replaced outside this client; the Person 4 MSI or a dev rebuild would do it. On the next start, `reconcile_update` reaches line 595 and returns "Applied target no longer matches signed release". `main.rs:441-444` propagates it and `serve` refuses to start until `apply-journal.json` is deleted. The `rolled_back` phase at line 383 behaves the same way.
- **Worse variant:** `rollback_update` saves `rollback_pending` at line 734 before checking that the target is still the release. If the target has drifted, every startup returns `helper_required`, the helper fails in `recover_rollback` ("Target changed externally"), and the app exits each time.
- **Related staleness:** `load_staged` verifies downgrade and minimum-version rules against `staged.current_version` recorded at stage time (line 343). A release staged long ago can therefore be applied over a newer binary that was installed outside this client.
- **Smallest fix:** for `completed`/`rolled_back`, report drift as a non-fatal status such as `superseded_externally`. Check the target digest before persisting `rollback_pending`. Pass the running version into `apply_staged_update`.

### 5. `util::run` can hang forever after the child exits, and its timeout kill and output cap have side effects
- **Severity / confidence:** medium. The code path is confirmed; a real installer that triggers it is suspected.
- **Location:** `crates/dacs-setup/src/util.rs:275-302`, `util.rs:286-287`; same pattern at `scan.rs:535` and `scan.rs:649-652`.
- **Hang:** `try_wait` returns a status, then lines 295-302 join the reader threads with no deadline. They only finish at pipe EOF. If the installer left any descendant holding the inherited stdout or stderr (a daemon started from a postinst script, a background service), the join never returns. The 1800-second timeout no longer applies. `apply` then holds `setup.lock`, the journal step stays `Running`, and the serial Jobs queue in `dacs-app` is blocked.
- **Scan variant:** `reader.join()` and `errors.join()` after killing OMP hang the same way, and the 190-second deadline is not checked while blocked in `stdin.write_all(&frame)`.
- **Timeout kill:** `child.kill()` hits only the direct child, so descendants such as `msiexec`, `dpkg` or `cargo` (under `rustup run` on Windows) keep running. The journal says `Failed` while the install proceeds, and a `resume_interrupted` retry can overlap it.
- **Output cap:** after 1 MiB + 1 bytes `collect` returns and drops the pipe's read end (line 275). The installer's next write gets EPIPE or SIGPIPE mid-install, and the step is reported as an I/O error.
- **Smallest fix:** after exit, wait for the readers with a short grace period and then abandon them. Keep draining and discarding past the cap instead of closing. Run the child in its own process group or Job Object and kill the tree.

### 6. `engine::apply` early returns leave the journal saying `running`
- **Severity / confidence:** low / confirmed by reading.
- **Location:** `engine.rs:225-226`, then returns at `engine.rs:246`, `253`, `282`, `307`.
- **Sequence:** the status is persisted as `running`, then a Homebrew or Trunk "changed since review" check returns `Err` without rewriting it. The lock is released and no step is `Running`.
- **Impact:** a journal reader cannot tell a refused plan from a live or crashed run. The same plan fails the same way every time while the file claims `running`.
- **Smallest fix:** set `replan_required` and save before those returns, as line 234 already does.

### 7. No-replace rename on Unix is check-then-act
- **Severity / confidence:** low / confirmed by reading.
- **Location:** `util.rs:141-144`, used by `update.rs:460-461` for every swap rename.
- **Sequence:** `target.exists()` is false, another process creates the destination, and `fs::rename` silently overwrites it. `update.lock` excludes other DACS processes but not a package manager writing into the install directory.
- **Impact:** an externally written binary could be clobbered without the "refusing overwrite" error the Windows path gives.
- **Smallest fix:** `renameat2(RENAME_NOREPLACE)` on Linux, or `hard_link` plus `remove_file` elsewhere.

### 8. Resume replays a non-idempotent command
- **Severity / confidence:** low / suspected (rustup behaviour not verified).
- **Location:** `catalogue.rs:498-506`, replayed by `engine.rs:310-314`.
- **Sequence:** a repair of `rustfmt` or `clippy` is `[component remove, component add]`. If the remove succeeds but the process dies before line 317's state is saved, the step stays `Running`. With `resume_interrupted` the remove is re-run on a component that is already gone, which I believe exits non-zero.
- **Impact:** the plan can never complete; only a fresh plan (now a different digest) gets past it.
- **Smallest fix:** on resume, probe before replaying a `Running` removal step, or have the engine skip a removal whose component is already absent.

### 9. Crash and handoff leftovers are never reclaimed
- **Severity / confidence:** low / confirmed by reading and a repo-wide grep for removal sites.
- **Location:** `update.rs:491-498`, `update.rs:681-699`, `update.rs:289`.
- **Detail:**
  - A crash between creating the candidate (684-697) and saving the journal (712) orphans a `.dacs-next-<uuid>` of up to 512 MiB that no journal references.
  - Each helper handoff leaves a full `update-helper-<uuid>` copy of the executable.
  - Each new release orphans the previous `.dacs-backup-*` when `new_journal` overwrites the journal.
  - A crash mid-download leaves the `.tmp*` artifact.
- **Smallest fix:** write the journal with the candidate path before creating the candidate, and sweep unreferenced `.dacs-*`, `update-helper-*` and temp files under the lock in `reconcile_update`.

### 10. AI scan config write is rejected on macOS (not a race)
- **Severity / confidence:** medium / confirmed by reading, not run on macOS.
- **Location:** `scan.rs:438-443` with `util.rs:113`.
- **Detail:** `tempfile::tempdir()` lives under `$TMPDIR`, which on macOS is `/var/folders/...`, and `/var` is a symlink. `atomic_json` calls `reject_links` and fails with "Symlink/reparse path is not allowed: /var", so `scan_project(.., true)` always fails there. The `util.rs` test canonicalises its temp dir for the same reason.
- **Smallest fix:** canonicalise `temporary.path()` before joining, or write this throwaway file with plain `fs::write`.

## Cleanup candidates

- `update.rs:260-263` and `update.rs:325-328`: `download-journal.json` is written twice and never read. A repo-wide grep finds no reader. Safe to delete both writes.
- `update.rs:570-574`: the "Existing update requires recovery before another release" branch is unreachable. Lines 556-561 already return for every phase other than `completed`/`rolled_back`. Safe to fold into the preceding branch.
- `scan.rs:343-348`: the conflict neutralisation in `deterministic` is redundant. `scan_project` repeats it at `scan.rs:692-696` under the same condition, and `rust-stable` is already present whenever a pin exists. The channel parsing in `toolchain_pins` (251-265) is also duplicated in `deterministic` (roughly 291-314).
- `update.rs:316` and `update.rs:462-467`: `sync_directory` straight after `rename_durable` is redundant. On Unix `rename_durable` already fsyncs the parent (`util.rs:145`), and elsewhere `sync_directory` is a no-op (`util.rs:150-158`). Only the call at `update.rs:697` does work.
- `update.rs:582`: `check_journal` runs two or three times per call path (inside `load_recorded_release`, again at 381 or in `rollback_update`, again here). The call in `apply_journal` alone is sufficient.
- `engine.rs:361-364`: `enrollment-<sha>.json`, which holds the invitation token, is written but no code in the repo reads or deletes it. Onboarding states that enrollment does not go through this adapter (`onboarding.rs:805-806`). Remove it only if no external adapter is planned to consume it.
- `lib.rs:15-16`: `SignedUpdate`, `UpdateMetadata` and `update_signing_bytes` are re-exported but have no users outside the crate (grep over `crates`, `person-4`, `net`, `examples`). Either have Person 4's signing tool use them or make them `pub(crate)`.

## Checked and found sound

- **State and manifest writes** (`util.rs:112-148`): random temp file in the same directory, file fsync, then a write-through or parent-fsynced rename. Every non-test write in `engine.rs` and `update.rs` goes through it, so no half-written file is observable.
- **Mutual exclusion** (`engine.rs:153`, `update.rs:224`, `376`, `552`, `719`): an exclusive try-lock is held for the whole operation and is per handle, so same-process threads conflict too. Caveat: the lock is per state directory. A CLI run with another `--state-dir` can drive the same package manager concurrently, and `plan`/`detect` take no lock.
- **Journal ordering** (`engine.rs:310-314`, `212-224`): `Running` is durable before spawn, and a `Running`/`Failed` step is never replayed without explicit `resume_interrupted`.
- **Plan approved against changed state:**
  - The digest and catalogue recipes are recomputed and the persisted plan compared (`engine.rs:49-146`, `156-163`).
  - Re-probes cover no-command steps (229-241), the Homebrew version (242-257), unversioned installs (259-276) and Trunk (277-309), with a post-install probe at 345-356.
  - Pinned install and repair are not re-probed, but the pinned version in the command makes them fail rather than drift.
- **Artifact verification order:**
  - The signature is checked before any fetch (`update.rs:213`), the trust anchor is pinned (225-236) and redirects are off.
  - The download is stream-hashed into a private random temp file before it is renamed to `artifact.bin` (289-315).
  - Signature and digest are re-checked on apply (334-346).
  - The candidate is hashed after the copy (696) and again before the swap (644), and the target is hashed after it (656).
- **Stage ordering:** a crash between replacing `artifact.bin` (315) and writing `staged.json` (324) is caught by `load_staged` and fails closed; restaging heals it. Staging is refused while a journal is mid-flight (237-244), and per-digest receipts keep rollback metadata safe from later stages.
- **Temp names:** all are random (`NamedTempFile`, `tempdir`, UUID candidate with `create_new`, UUID helper). No predictable names found.
- **Scan:**
  - Root is canonicalised and reads are confined through `cap_std` (`scan.rs:32-54`); symlink entries are skipped (85-92).
  - Evidence digests and HEAD are re-read after analysis (700-719).
  - The walk uses ambient `read_dir` (63), so a directory swapped for a symlink mid-walk can leak evidence file names into `unresolved`, but not contents.
  - A concurrent `git pack-refs` between `exists()` and the read (181-187) only produces an error.
  - Caveat: `revision` is HEAD, while evidence comes from the working tree, with no dirty check.
- **`catalogue.rs`** has no persistent state. Its only timing-sensitive role is the digest determinism in finding 1 and the probes that `engine.rs` re-runs.
- **`lib.rs`** is re-exports only.

## Not covered

- Platform behaviour not exercised:
  - Windows `MoveFileExW` and std error-kind mapping.
  - macOS temp paths.
  - Whether real installers (winget, apt postinst, brew) actually leave pipe-holding descendants or exceed 1 MiB of output.
  - The rustup exit code when removing an absent component.
- `dacs-app` consumers (`main.rs`, `server.rs`, `local.rs` including `launch_update_helper` and `Jobs`) were read only far enough to trace impact. I did not establish whether the helper can restart the new binary before the old process has released its ports.
- Per-platform recipe correctness in `catalogue.rs` and the OMP RPC protocol details in `scan.rs` were not audited beyond their process and timeout handling.