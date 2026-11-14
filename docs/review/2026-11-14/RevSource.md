## Race conditions

Everything below is from reading the code; nothing was run or reproduced. Git's `core.fsync` semantics were checked against the git-config documentation.

### 1. Target ref is not durable when SQLite records the publication as published
- **Severity:** high (silently lost publication; needs power loss or OS crash within the flush window)
- **Confidence:** confirmed by reading and by [git-config `core.fsync`](https://git-scm.com/docs/git-config#Documentation/git-config.txt-corefsync)
- **Locations:** `crates/dacs-source/src/git.rs:241`, `crates/dacs-source/src/lib.rs:393-401`, `crates/dacs-source/src/quarantine.rs:62-63`
- **Evidence:** every Git call passes `core.fsync=committed`. Git documents `committed` as equivalent to `objects`, which is `loose-object,pack`. It excludes `reference` and `pack-metadata`. The store uses `PRAGMA synchronous=FULL` (`crates/dacs-core/src/lib.rs:205`).
- **Interleaving:**
  1. `publish` runs `update-ref` at `lib.rs:393`; Git renames the ref without fsync.
  2. `finish_publication` at `lib.rs:401` durably commits `published`, `Accepted`, `integrated_sha`, and releases dependants.
  3. Power is lost before the ref reaches disk; after restart the target is back at the old head.
  4. `reconcile` (`lib.rs:413`) only visits `pending`/`reconciling` publications, so nothing notices.
  5. The next `prepare` merges against the old head and its CAS succeeds, so the accepted work is missing from the target.
- **Second effect:** pack `.idx`/`.rev` files are also unsynced, contrary to the comment at `quarantine.rs:62-63`. A torn `.idx` already linked into live makes re-import of the same bundle fail permanently with "Immutable Git object storage collision" (`quarantine.rs:55-58`).
- **Smallest fix:** change `git.rs:241` to `core.fsync=committed,reference,pack-metadata` (or `all`) and correct the comment.

### 2. A killed Git leaves a stale ref `.lock` that is never cleared
- **Severity:** medium (permanent stuck state, narrow window)
- **Confidence:** confirmed by reading; no `.lock` recovery exists in the crate (grep for `.lock"` finds only `project.lock`)
- **Locations:** `git.rs:297-315` (deadline/budget kill), `git.rs:571-600` (job kill on close), `lib.rs:50-78` (sweep covers `temporary/` only), `lib.rs:325`, `lib.rs:393`
- **Interleaving:**
  1. `update-ref` creates `refs/heads/<target>.lock`.
  2. Git is terminated before the rename: coordinator killed on Windows (`KILL_ON_JOB_CLOSE`), or `tree.stop` after the 120 s deadline or a failed budget sample. Forced termination skips Git's lockfile cleanup.
  3. After restart, `head()` (read-only `for-each-ref`) sees the expected head, so the publication becomes `not_published`.
  4. Every later `update-ref` on that target fails with "File exists", for every candidate, until someone deletes the file by hand.
- **Also affects:** `refs/dacs/staged/<id>/<sha>.lock` at `lib.rs:325`, which blocks re-preparing that candidate.
- **Smallest fix:** in `lock()`, after acquiring the project lock, also remove `*.lock` under `repository.git/refs` and `packed-refs.lock`. This is only safe once finding 4 is fixed on Unix.

### 3. Repository quota is checked after the write in `prepare`/`publish`; an overrun bricks the project
- **Severity:** medium (stuck state; only reachable near the 512 MiB or 100,000-entry limit)
- **Confidence:** confirmed by reading
- **Locations:** `git.rs:202`, `git.rs:303-306`, `git.rs:333`, `git.rs:452-455`, `git.rs:501-502`, `lib.rs:316-325`, `quarantine.rs:42-47`
- **Evidence:** only the import path reserves space before writing (`quarantine.rs:42-47`). `merge-tree --write-tree`, `commit-tree` and the staged-ref write have no reserve.
- **Interleaving:**
  1. The pre-check at `git.rs:202` passes with a few KB free.
  2. The merge writes loose objects past the limit, between 100 ms samples or before the post-check at `git.rs:333`.
  3. The objects stay. Every later `run` fails its pre-check: `verify`, `publish`, `diff`, `export`, `import`, `prepare`.
  4. If a publication intent is pending for that project, `reconcile` fails and `SourceService::new` (`lib.rs:41`, called at `dacs-app/src/server.rs:1082`) refuses to start.
- **No recovery path:** there is no gc, prune or repack anywhere in the crate.
- **Smallest fix:** before `git.merge` in `prepare`, require a write reserve below the byte and entry limits (as import does), and let read-only commands (`for-each-ref`, `rev-parse`, `cat-file`) skip the hard pre-check so reconciliation still works over quota.

### 4. On Unix a Git child outlives a crashed coordinator and the project lock
- **Severity:** medium (wrong durable state; rare)
- **Confidence:** mechanism confirmed by reading; practical reachability suspected (needs `update-ref` to stall across a coordinator restart)
- **Locations:** `git.rs:262-267`, `storage.rs:140-149`, `lib.rs:413-423`
- **Evidence:** children get their own process group with no parent-death signal. The lock file descriptor is close-on-exec, so the lock is released when the coordinator dies while the child keeps running. The 120 s deadline lives in the dead parent.
- **Interleaving:**
  1. The coordinator crashes after spawning `update-ref` for a pending publication.
  2. The new coordinator's `reconcile` takes the free lock and observes the expected head.
  3. `finish_publication` records `not_published`; with `cancel_requested` set it cancels the task (`dacs-core/src/publication.rs:197-212`).
  4. The orphan then completes its CAS. The target now holds a candidate the store calls unpublished or cancelled.
- **Related:** an orphaned `fetch` writing into `temporary/import-*` can make the new process's `remove_temporary_tree` (`lib.rs:73`) fail with "directory not empty" until it exits.
- **Windows:** not affected, because the job object kills the tree.
- **Smallest fix:** let the child inherit a duplicate of the project-lock descriptor so the flock stays held while any Git descendant lives. Alternatively set `PR_SET_PDEATHSIG` in `pre_exec` (Linux only).

### 5. Interrupted `git init` leaves a half-made `repository.git` that `ensure()` never repairs
- **Severity:** low (stuck state; window is the first init of a project)
- **Confidence:** confirmed by reading
- **Locations:** `git.rs:99-121`
- **Interleaving:**
  1. `ensure()` sees no repository and runs `init --bare` straight into the final path.
  2. A crash, kill or power loss interrupts it.
  3. On the next call `inspect` returns a directory, init is skipped, and `verify()` fails on `rev-parse` for every operation.
- **Smallest fix:** init into a directory under `temporary/` and `rename` it to `repository.git`, then sync the parent.

### 6. Directory sync is a no-op on Windows
- **Severity:** low
- **Confidence:** no-op confirmed by reading; power-loss effect on NTFS suspected, not exercised
- **Locations:** `storage.rs:197-203`; callers at `quarantine.rs:75-90`, `lib.rs:485`, `git.rs:112`
- **Impact:** on the Windows workstation, the object hard links, the `refs/dacs/imports/<id>` rename and the artifact no-clobber rename return success without their directory entries being flushed. A power loss shortly after can drop an import or artifact the store already references.
- **Smallest fix:** on Windows open the directory with `FILE_FLAG_BACKUP_SEMANTICS` and write access, then `sync_all()`.

### 7. SIGKILL is sent to the process group after the leader has been reaped
- **Severity:** low
- **Confidence:** confirmed by reading; needs PID wrap-around inside a microsecond window
- **Locations:** `git.rs:309-315`, `git.rs:551-556`
- **Interleaving:** `try_wait` returns the status at line 310, which reaps the child. `tree.stop` then calls `kill(-pgid, SIGKILL)` at line 554 on every successful command. If the group is empty and the PID has been reused as another group leader, that group is killed, under the coordinator's elevated identity.
- **Smallest fix:** call `tree.stop` only when `result` is `Err`, or signal the group while the child is still a zombie (`waitid` with `WNOWAIT`).

### 8. Windows job assignment happens after spawn
- **Severity:** low
- **Confidence:** suspected (window is between process creation and `AssignProcessToJobObject`)
- **Locations:** `git.rs:267-268`, `git.rs:585`
- **Interleaving:** a helper process started by Git before the assignment is outside the job. On timeout it survives `TerminateJobObject` and keeps the stdout/stderr pipes open, so the `join()` calls at `git.rs:317-324` block while the project lock is held.
- **Smallest fix:** spawn with `CREATE_SUSPENDED`, assign, then resume.

### 9. Budget monitor can abort Git on a transient filesystem error
- **Severity:** low
- **Confidence:** suspected; I could not construct a concrete Git operation that removes a directory mid-scan
- **Locations:** `storage.rs:217-229`, `git.rs:303-306`
- **Evidence:** `tree_usage` tolerates a vanished file (`inspect` returns `None`), but `read_dir` at line 228 and any non-NotFound stat error are returned as failures. The 100 ms monitor would then kill a running Git.
- **Smallest fix:** treat NotFound from `read_dir` and from its entries as "skip", as is already done for files.

## Cleanup candidates

- `crates/dacs-source/src/lib.rs:86` — `ensure_project` has no callers. Grep across `crates/`, `net/`, `examples/` and `person-4/` finds only the definition.
- `crates/dacs-source/src/lib.rs:22-23` — `MAX_BUNDLE_BYTES` and `MAX_ARTIFACT_BYTES` are never referenced (same grep).
- `crates/dacs-source/src/lib.rs:411` — `reconcile` is `pub` but its only caller is `lib.rs:41`; it can be private.
- `crates/dacs-source/src/lib.rs:484` — `file.sync_all()` after `persist_noclobber` is redundant. `write_temporary` already synced the same content at `storage.rs:193`, and the rename does not change data.
- `crates/dacs-source/src/quarantine.rs:62-63` — the comment says the fetch fsynced pack metadata through `core.fsync=committed`; that contradicts Git's documented behaviour (finding 1).
- `crates/dacs-source/src/storage.rs:197` — `sync_directory` is a misleading name on non-Unix, where it does nothing (finding 6).
- `crates/dacs-source/src/lib.rs:532-571` — `verify_artifact` repeats the no-follow open, size cap and SHA-256 check that `read_limited` (`storage.rs:153-188`) plus `get_artifact` (`lib.rs:511-512`) already perform. Merging is safe if the 64 MiB buffer is acceptable; otherwise leave it.

## Checked and found sound

- **Project lock** (`storage.rs:130-151`, `lib.rs:50-78`): an OS lock on a file that is never deleted, released by the OS on process death, with one handle per call so threads exclude each other. Every public operation, including read-only ones, takes it before touching `temporary/`, the repository or artifacts.
- **Blocking callers**: every `dacs-app` call into the service runs under `spawn_blocking` (`server.rs:92-95`, `383-386`), so the blocking lock never sits on an async worker.
- **Temp names**: the bundle, export file, artifact temp and quarantine directory all use random `tempfile` names inside the locked project (`storage.rs:190`, `lib.rs:221`, `quarantine.rs:17`).
- **Artifacts** (`lib.rs:449-521`): the digest is computed on the in-memory bytes before the lock. The temp file is fully written and fsynced before an atomic no-replace rename, so readers never see a partial file. An existing object, or an `AlreadyExists` result, is re-hashed, and `get_artifact` re-hashes on read. Tests cover this in `artifacts_are_verified_immutable_and_project_authorized` and `artifact_symlink_cannot_escape_private_storage`.
- **Quarantine promotion** (`quarantine.rs:17-93`): the fetch goes into a private repository with a relative alternate. Objects are hard-linked no-clobber in pack, then rev/bitmap, then idx order (`quarantine.rs:125`). Refs appear through one directory rename after all objects. A crash before the rename leaves only unreachable objects, and the next `lock()` sweeps the quarantine. Tests cover the oversized, incremental and abandoned-quarantine cases.
- **Seed** (`lib.rs:117-201`): the head-absent check, pending-publication check and zero-expected `update-ref` all happen under one lock. A crash between promotion and the ref update leaves the target empty and the seed retryable.
- **Prepare** (`lib.rs:307-336`): the staged ref is written before the store record. The merge commit is deterministic (fixed identity and date), so a retry is idempotent and a crash leaves only an extra pin.
- **Publish and reconcile** (`lib.rs:338-447`):
  - The candidate is re-read under the lock, and the intent is committed before `update-ref`.
  - The CAS uses the intent's expected head, or zero for an empty target.
  - The outcome is decided by the observed head, not the exit status, and an existing intent is never replayed.
  - Core allows one unresolved publication per project (`publication.rs:112-116`).
  - `finish_publication` returns early for settled publications (`publication.rs:174-176`), so the list read outside the lock at `lib.rs:413` is harmless.
  - The candidate, expected and ambiguous outcomes are tested in `restart_observes_candidate_expected_or_ambiguous_ref_without_replay`.
- **Subprocess control** (`git.rs:190-340`): inherited `GIT_*` variables are removed, system and global config are disabled, hooks point at a dedicated directory, stdin is null, and cwd is the project. Both pipes are drained by threads while the parent polls `try_wait`, so a full pipe cannot deadlock. Every exit path kills the tree, waits, then joins the readers, so an in-process timeout never leaves Git running before the head is observed. `gc.auto`, `maintenance.auto` and `core.fsmonitor` are off, so Git starts no detached daemons.
- **Caveat on subprocess errors**: the deadline (`git.rs:297`) is tested before `try_wait` (`309`), and the post-exit budget check (`333`) can report failure for a command that already completed. `publish` tolerates this by observing the head. A seed retry would get "cannot replace an existing target".
- **Tests** (`tests.rs`): each fixture owns a temp directory. There are no fixed ports, global environment mutations, sleeps or threads, so parallel runs do not interfere.

## Not covered

- Nothing was executed. Power-loss outcomes on NTFS and ext4 (findings 1 and 6) are inferred from documented semantics.
- I did not determine how `update-ref` or `for-each-ref` treat a zero-length loose ref left by an unsynced create.
- `crates/dacs-source/src/tests.rs` was read for covered orderings only; fixture helpers at lines 60-287 and the diff tests at 764-938 were skimmed by name.
- Outside the slice, only `dacs-core/src/publication.rs` and the `dacs-app/src/server.rs` call sites were read. `sched::cancel`, `refresh_dependencies` and `apply_deferred` were not reviewed.