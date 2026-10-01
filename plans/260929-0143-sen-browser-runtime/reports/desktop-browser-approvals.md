# Desktop: "Waiting for your approval" card (Settings → Browser)

Worktree: `/Users/benji/Projects/SenClaw/.worktrees/desktop-browser` (branch `feat/browser-engine-settings`)
Commit: `f347d66dd2fac081d9cd1eac3bb2d6860a456a0c`

## What changed

### `lib/features/settings/browser_models.dart`
- `PendingBrowserApproval` — parses `GET /api/browser-agent/approvals` rows
  (`approval_id, task_id, chat, goal, action, operation, text, driver, url,
  waiting_secs`), with `text`/`url`/`waitingSecs` nullable per spec.
- `browserApprovalsProvider` — `FutureProvider<List<PendingBrowserApproval>>`,
  polled every 5s by the card (never starts the runtime, matching the GET
  endpoint's own contract).
- `formatBrowserWaitingTime(int seconds)` — pure formatter: `45s` / `3m` /
  `1h 5m` (drops the smaller unit once whole minutes are reached; hours show
  a minute remainder only when nonzero).
- `BrowserApprovalOutcome` — parses the `POST .../approvals/:id` response
  (`task_id, status, message`).
- `answerBrowserApproval(ApiClient, String approvalId, {required bool
  approve})` — `POST /api/browser-agent/approvals/:id` with body `{"approve":
  ...}`, given a 30-minute timeout (via `ApiClient.post`'s existing `timeout`
  param) instead of the default 30s `kApiTimeout`, since the daemon's own
  contract says the call can take minutes (the task keeps running inside it).
  A 404 (`code: "no_approval"`, already answered elsewhere) surfaces as an
  ordinary `ApiException`, same as any other daemon error — no special-casing
  needed since the spec treats it the same as any other error (show the
  message, reload).

### `lib/features/settings/browser_section.dart`
- `_ApprovalsCard` (+ `_ApprovalsCardState`), inserted right after
  `_EngineStatusCard` and before `_SettingsFormCard` in `BrowserSection`'s
  build — above the settings form, per spec. Hidden (`SizedBox.shrink()`)
  when the list is empty, still loading, or errored with no prior good data
  (mirrors `_ExtensionCard`'s `skipError: true` so a transient 5s-poll
  failure keeps showing the last good state instead of flashing an error —
  this card is a courtesy notice; the chat's own approval prompt is never
  blocked by it).
- Each row: action label bold, operation tag via a small `_operationLabel`
  mapper (`CLICK`→"Click", `KEY_ENTER`→"Press Enter", `DIALOG_ACCEPT`→
  "Confirm a dialog", `TYPE_TEXT`→"Type text", anything else shown as is),
  `Goal: <goal>` (reusing the already-shared `'Goal'` key from
  `kanban.dart`), the URL on one ellipsized line (only when present),
  the chat id, and `Waiting {time}` (only when `waitingSecs` is present).
  Approve and Decline buttons; while a POST is in flight for that row both
  buttons disable and a spinner appears alongside them (matches the spec's
  "that row shows a spinner and both buttons are disabled").
- Approve shows a confirm dialog first ("SenClaw will do this in the browser
  now."); Decline acts immediately, no dialog, per spec.
- Outcome handling: `needs_approval` is the one status meaning the task
  paused again rather than ending — confirmed against the daemon's own e2e
  fixture (`plans/260929-0143-sen-browser-runtime/e2e/approval.sh`, which
  asserts `task["status"] == "needs_approval"` for a paused task). So a
  decline whose outcome status is anything else toasts "Declined"; every
  other outcome (including every approve) toasts "The task went on:
  {status} — {message}". Both success and error paths reload the list
  (`ref.invalidate(browserApprovalsProvider)` in `finally`), matching "On
  error (including 404) show the daemon's error message and reload."

### `lib/core/i18n/vi/browser.dart`
Added the 11 new keys from the spec's Vietnamese table verbatim (`Waiting
for your approval`, the hint sentence, the confirm sentence, `Decline`,
`Declined`, `Waiting {time}`, `The task went on: {status} — {message}`,
`Click`, `Press Enter`, `Confirm a dialog`, `Type text`). Did **not** re-add
`Approve` (already in `settings_screen.dart`, same Vietnamese value) or
`Goal` (already in `kanban.dart`, same Vietnamese value) — reused per this
file's own documented "shared keys are not repeated" rule. All interpolated
strings use the app's existing `context.trArgs('... {x} ...', {'x': ...})`
helper, no hand-written replace chains.

### Tests
- `test/browser_models_test.dart`: `PendingBrowserApproval.fromJson` (full
  fields, all-nullable-fields-missing, a present `TYPE_TEXT` `text`),
  `formatBrowserWaitingTime` (seconds/minutes/hours boundaries incl. the
  spec's own `45s`/`3m`/`1h 5m` examples, and a defensive negative-clamp
  case), `BrowserApprovalOutcome.fromJson` (full + all-missing).
- `test/browser_section_test.dart`: extended the existing `_FakeApi` with
  `approvals`/`approvalOutcome`/`approvalError` (same fake-API pattern the
  file already uses for settings/extension/tabs) and `callCount` for
  reload assertions. New widget tests: card hidden when the list is empty;
  a pending approval renders action/operation-label/goal/url/chat/waiting-
  time and the hint text; an unrecognized operation is shown as is; Approve
  requires the confirm dialog and only then POSTs `{"approve": true}` and
  toasts the composed message (dialog's own Approve button is disambiguated
  from the row's via `find.descendant(of: find.byType(AlertDialog), ...)`
  since both are `FilledButton`s with the same text); cancelling the confirm
  dialog sends nothing; Decline needs no dialog and toasts "Declined" when
  the outcome ended the task; a decline that leaves the task at
  `needs_approval` toasts the generic status/message instead; a POST error
  (404-shaped) shows the daemon's message and reloads the list.

## Test results

- `flutter analyze`: **No issues found!**
- `flutter test test/browser_section_test.dart test/browser_models_test.dart`:
  **36/36 passed** (22 model tests incl. 12 new, 14 widget tests incl. 8 new).
- Other files matched by `grep -rl browser test/` (`tool_step_card_test.dart`,
  `slash_mention_input_test.dart`, `alias_tab_test.dart`) were incidental
  matches, unrelated to this feature, unmodified — all still pass.
- Full `flutter test`: **344 passed, 1 skipped** (pre-existing skip, present
  before this change, unrelated to browser code — nothing failed). Exit
  code 0, confirmed twice for stability.

## Commit

```
f347d66dd2fac081d9cd1eac3bb2d6860a456a0c
feat(settings): approve or decline paused browser actions
```

Working tree clean afterward; only the 5 permitted files touched (`git diff
--stat` against the prior commit matches exactly: `browser_section.dart`,
`browser_models.dart`, `vi/browser.dart`, `browser_section_test.dart`,
`browser_models_test.dart`). No new dependencies. Nothing pushed, no daemon
or app started/stopped.

## Notes / judgment calls (spec was silent on these)

- **"the task ended" for the decline→"Declined" rule**: the spec doesn't
  enumerate the outcome `status` vocabulary. I resolved this from the
  daemon's own e2e fixture (`e2e/approval.sh`, `e2e/managed.sh`,
  `e2e/extension.sh`), which only ever assert `status == "needs_approval"`
  (paused) or `status == "done"` (ended) — so I treat any status other than
  `"needs_approval"` as "ended." Flagging this because it's inferred from
  daemon test fixtures rather than stated directly in `ui-management.md`;
  worth a quick daemon-side confirmation if the status vocabulary ever grows
  a second "still going" value.
- **Card visibility on a poll error with no prior data**: spec only says
  "Empty list: hide the card." I hide on error/loading too (no error banner),
  reasoning it as the same "nothing confirmed to show" case, and because this
  card is explicitly not the only approval path (the chat's own prompt
  remains). Reasonable default; call it out in case a visible error state is
  preferred here.
- **30-minute POST timeout**: the daemon's contract says "can take minutes"
  with no fixed ceiling; the transport (`ApiClient`) requires a finite
  `Duration`, so I picked 30 minutes as a generous but bounded ceiling
  (`local_models_section.dart` uses 11 minutes for an analogous "daemon says
  this can be slow" call, bounded there by a documented 600s daemon health
  budget — no such documented bound exists here, hence the larger margin).
