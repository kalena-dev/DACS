# Task board browser acceptance (Person 3)

- Date: 2026-11-14
- Tested build: fix commit `c1cb0ba` (merged into `main` as `89aded0`). Steps 1–9 first ran on the pre-fix build of `c637793`. The affected steps were rerun after the fix.
- Host: Windows 11, managed Chromium 157 driven via CDP, loopback only.
- Viewports: 1280×900 desktop; 360×780 narrow.

## Commands

```
# from examples/task-board-web
C:/Users/GeorgeVogas/.cargo/bin/trunk.exe build --release
# from the repository root
C:/Users/GeorgeVogas/.cargo/bin/cargo.exe build -p dacs
C:/Users/GeorgeVogas/.cargo/bin/cargo.exe test -p dacs-task-board-web   # 3 passed
target/debug/dacs.exe task-board --listen 127.0.0.1:7312 \
  --data %TEMP%/dacs-tb-accept/board.redb \
  --assets <worktree>/examples/task-board-web/dist
```

The data file was outside `dist`. Each "restart" killed the process and started it again with the same `--data` path.

## Results

| # | Step | Observed | Result |
|---|------|----------|--------|
| 1 | Create two tasks with title and description | "Write report" and "Plan sprint" listed with their descriptions, both Todo. Form cleared. Message: "Task created and saved." | Pass |
| 2 | Edit title and description | Edit put the task into the form and moved focus to Title. Saving changed it to "Plan sprint 42 / Collect sprint goals and estimates". `GET /api/tasks` returns the same values. | Pass |
| 3 | Status Todo → In progress → Done | The select showed `in_progress`, then `done`. The server reports `done`. | Pass |
| 4 | Filter by text, by status, then clear | `q=estimates` showed 1 task (Plan sprint 42). `status=done` showed 1 task (Write report). `q=report&status=todo` showed 0 tasks plus the empty-state message. Clear filters emptied both inputs and showed 2 tasks again. | Pass |
| 5 | Backend down, then restart and reload | While the backend was down, Refresh showed "Cannot reach the task-board server" and kept the last snapshot. After restart and reload: both tasks with edited text, Done/Todo status. | Pass |
| 6 | Delete with confirmation, restart | "Permanently delete “Write report”?" appeared with `aria-expanded=true`. Keep task cancelled it. Confirm delete removed the task. It was still gone after another restart and reload. | Pass |
| 7a | Skip link | The first Tab focuses "Skip to tasks", which is visible. Enter moves focus to the Tasks heading (`#task-list`), and the next Tab goes to the search field. | Pass |
| 7b | Tab order | Skip link → Refresh → Title → Description → Create → Search → Status filter → Apply → Clear → then for each row: Status → Edit → Delete. | Pass |
| 7c | Form submit with Enter | Typed title and description, then pressed Enter in Title. The task was created. | Pass |
| 7d | Keyboard status change | **Failed before the fix:** after ArrowDown saved the status, focus jumped to the Tasks heading. **After the fix:** focus stays on the row's status select. ArrowDown twice gives Todo → In progress → Done (server: `done`). If a filter hides the row, focus goes to the Tasks heading. | Pass (after fix) |
| 7e | Keyboard delete confirm | Enter on Delete, then Tab, Tab reaches Confirm delete and Keep task. **Failed before the fix:** Keep task dropped focus to `<body>`. **After the fix:** Keep task and Escape both return focus to Delete. Enter on Confirm delete deletes the task, and focus moves to the Tasks heading. | Pass (after fix) |
| 7f | Visible focus | Every Tab stop has a 2px solid `#aab7ca` outline and matches `:focus-visible`. | Pass |
| 8 | Narrow viewport (360×780) | `scrollWidth` 360 equals `innerWidth` 360. No element extends past the viewport, including the open delete confirmation and the inline title error. Every button, input and select is at least 44px. A status change and a delete confirmation both worked at this width. | Pass |
| 9 | Empty title rejected | Empty title: native `required` blocks submit ("Please fill out this field."), and no POST is sent. Whitespace-only title: rejected with no POST. **Defect before the fix:** the message showed in the global "Request error" alert, with "Last request failed" and "Retry loading tasks". **After the fix:** an inline field error (`role=alert`, `aria-invalid`, `aria-describedby`), no global alert, cleared when typing. | Pass (after fix) |

Screenshots (01–30, local only, not committed) are in `%TEMP%/dacs-tb-accept/shots/`.

## Fixes made (`c1cb0ba`, frontend only)

- Status changes return focus to the row's status control.
- Keep task and Escape return focus to Delete.
- Whitespace-only titles get an inline field error instead of the global request-error alert.

No backend defect was observed.

## Remaining limits

- This is a single-host loopback check by the frontend owner. It is not the four-agent / three-PC proof.
- Person 4 still owns independent browser acceptance.
- Tested only in Chromium with synthetic keyboard input. Not tested: screen readers, other browsers, touch devices, forced-colors mode.
- The native unit tests cover only `api.rs` (wire format, URLs, decoding). UI behaviour is covered by the browser run above only.
