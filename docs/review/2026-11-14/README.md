# Race-condition review, 2026-11-14

Eleven read-only reviewers covered the whole workspace at `56422f5` (plus the commits that landed
during the review, through `d6e13b0`). Each report lists race conditions with file/line, the
interleaving, impact and a suggested fix, then cleanup candidates and what was checked and found
sound. Nothing was executed by the reviewers; findings are from reading the code unless a report
says otherwise.

Line numbers refer to the revision reviewed. Code has moved since; locate by content.

| Report | Scope |
|---|---|
| [RevWorkerRuntime](RevWorkerRuntime.md) | `dacs-worker` runtime, journal, local control |
| [RevWorkerProcesses](RevWorkerProcesses.md) | supervisor, OMP adapter, workspaces, crash tests |
| [RevWorkerBuilds](RevWorkerBuilds.md) | build runner, build execution, task-board backend |
| [RevCoreEngine](RevCoreEngine.md) | `dacs-core` engine, scheduling, publication, builds |
| [RevCoreNetwork](RevCoreNetwork.md) | coordinator network state, bridge, owner onboarding |
| [RevAppServer](RevAppServer.md) | HTTP server, entry point, local state, task-board server |
| [RevSource](RevSource.md) | Git/source storage |
| [RevSetup](RevSetup.md) | installer engine, catalogue, scan, updater |
| [RevNet](RevNet.md) | netadmin, device client, contract |
| [RevUiReleaseDeploy](RevUiReleaseDeploy.md) | UI crates, task-board frontend, release tooling, deploy script |
| [RevIncoming](RevIncoming.md) | commits `3028ec9`, `86a1d1b` and `docs/remaining-work.md` |

## Fixed on `main` (commit `e646463`)

All workspace tests passed afterwards except `windows_workflow` (see below).

- **App server** (RevAppServer): browser command path now runs the workflow-artifact validation
  (the security finding); listeners bind before state is opened (2); cooperative maintenance stop
  (1); store and network maintenance split (8); SIGTERM (7); flash notices as a list (6); atomic
  `admin.token` (9); cached task-board index (11); test port reservation serialised (12); unused
  `tower-http` dependency removed from the crate.
- **Network bridge and onboarding** (RevCoreNetwork): revocation proceeds when the contract check
  fails (5); no error spam without a transport; per-device operations lock (9); latches released
  when nothing was sent (3, partly); reviewed contract kept on a failed re-inspect (7); cloned
  consent for the join attempt (8); two cleanup items.
- **Setup** (RevSetup): completed journal re-probed before reuse (1); update candidate verified
  before the target is moved (2, ordering only); failed rollback reported (3, error only);
  `replan_required` saved on refused plans (6); dead `download-journal.json` writes removed.
- **Source** (RevSource): refs and pack metadata fsynced (1); no group kill after a normal exit
  (7); vanished directories tolerated in the budget scan (9); repository initialised through
  temporary staging (5); dead code removed.
- Separately: nine unsafe-free crates inherit the workspace `unsafe_code = "forbid"` lint;
  `dacs-setup` and `dacs-source` declare an explicit opt-out (they contain audited OS calls).

## Not fixed

Everything else in the reports is open. The fix work for these slices was stopped before any
edit landed, so every finding in the report stands:

- RevWorkerRuntime: includes the `pause_desktop` / `desktop_pause` kind mismatch (3), the fixed
  reconcile command id that can wedge the outbox (1), and `Failed` sent after an accepted
  submission (2).
- RevWorkerProcesses and RevIncoming: process-group signalling after reap, the Linux start-time
  bound, receipt handling.
- RevWorkerBuilds, RevCoreEngine (including lease expiry while the coordinator is down, 2),
  RevUiReleaseDeploy.
- RevNet: `netadmin-unfinished-fixes.patch` is an unapplied, uncompiled-against-current-main
  draft of items 1-8 for `dacs-netadmin`, written before the netadmin commits `964b806`..`c43ca7c`
  landed. It overlaps their revocation changes; treat it as notes, not as a patch to apply.
- Design-level items in the fixed slices (atomic update swap, enrollment state-machine exits,
  stale Git lock recovery, quota reserves, SQLite calls on async threads, enrollment idempotency).

## Found by running, not by review

- `crates/dacs-worker/tests/windows_workflow.rs` fails with MSVC `LNK1181: cannot open input file
  '.obj'` when the temp directory path is long (it failed under
  `C:\Users\GeorgeVogas\AppData\Local\Temp` and passed with `TMP=TEMP=C:\t`). The admitted Windows
  build's object paths exceed 260 characters once the worker state path is long enough.
