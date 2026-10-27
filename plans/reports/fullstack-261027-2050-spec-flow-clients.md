# Spec flow clients (web-app + desktop)

Both repos on branch `feat/spec-flow`, changes uncommitted, nothing pushed. No files outside the two repos touched.

## web-app (`/Users/benji/Projects/SenClaw/web-app`)

Files:
- `src/components/chat-common/specCommands.ts` (new) - five `AgentCommandItem`s (`kind: 'command'`, key = name without slash, so inserting gives `/review `), `specCommands(t)` translates the descriptions.
- `src/components/chat-common/index.ts` - exports `specCommands`.
- `src/components/ChatView.tsx` - `commands={[]}` -> `commands={specCommandItems}` (`useMemo(() => specCommands(t), [t])`).
- `src/components/chat-common/useCommandSuggestions.tsx` - two small behaviour fixes needed by the new entries (see Deviations).
- `src/components/settings/SpecFlowControl.tsx` (new) - antd Segmented card modelled on `CodeAutonomyControl` (Design first / When a spec exists / Off, blurb list, footer hint listing the commands).
- `src/components/settings/AgentBehaviorSettings.tsx` - `specFlow?: SpecFlow` in the state type, `ToggleKey` excludes it, control rendered in "Code sessions" only when GET returned `specFlow`; reuses the existing `save()` (partial POST `{specFlow}`, optimistic, revert on failure). Section shows if either `codeAutonomy` or `specFlow` is present.
- `src/i18n/vi.web.json` - 12 new keys (settings strings + five command descriptions). `Off` already existed; checked none of the new keys are in `vi.json`.

Commands run:
- `npm run build` (tsc -b && vite build): green, only the known CSS-minify warning. No test runner configured (CLAUDE.md), so no unit tests.

## desktop (`/Users/benji/Projects/SenClaw/desktop`)

Files:
- `lib/features/chat/widgets/slash_mention_input.dart` - `kSpecCommands`, five fixed `command` rows ahead of skills for the `/` trigger, `command` kind icon (terminal). Descriptions go through `context.tr`.
- `lib/features/settings/spec_flow_field.dart` (new) - SegmentedButton modelled on `code_autonomy_field.dart` (key `spec-flow`, pending/failure handling, snackbar on error).
- `lib/features/settings/settings_screen.dart` - import + `if (b['specFlow'] is String) SpecFlowField(...)` right after the code autonomy field; `onChanged` does `api.post` then `await ref.read(agentBehaviorProvider.future)` (no flash of old value).
- `lib/core/i18n/vi/settings_screen.dart` (settings strings), `lib/core/i18n/vi/chat_widgets.dart` (command descriptions).
- `test/spec_flow_field_test.dart` (new, 8 tests, modelled on `code_autonomy_field_test.dart`: field alone, onChanged value, failed save, Vietnamese at 1280/420 px, in Settings with the delayed-GET no-flash check, hidden when `specFlow` absent).
- `test/slash_mention_input_test.dart` - 4 new tests (commands before skills, pick inserts `/review `, `/spe` ranks `spec` first, `#` shows no commands).

Commands run:
- `flutter analyze` (whole project): No issues found.
- `flutter test test/spec_flow_field_test.dart test/slash_mention_input_test.dart test/code_autonomy_field_test.dart`: all passed.
- `flutter test` (whole suite): 638 passed, 1 skipped (pre-existing), 0 failed.
- Mutation check: removing the `await ref.read(agentBehaviorProvider.future)` makes the new no-flash test fail; restored afterwards (verified no leftover).
- No `flutter build` run.

## Deviations / decisions to know

1. Web hook `useCommandSuggestions.tsx` changed (2 small edits, not in the brief):
   - `commands` are now offered for the `/` trigger only; `#` lists skills alone (previously `[...commands, ...skills]` for both, so `#review` would have been inserted, which the daemon does not treat as a command). Matches the prop's doc ("Extra `/` entries"). `ChatView` was the only caller passing `commands`.
   - For `/` and `#`, names that start with the typed text are listed first. Without it, `/spec` + Enter picked `draft` (its description contains "spec"). Same rule added on desktop. `@` ordering unchanged.
2. Desktop POSTs only `{ "specFlow": v }` (daemon merges partials), not the whole behavior object like the neighbouring switches and the autonomy field do. Avoids overwriting a field changed meanwhile.
3. Command descriptions differ per client on purpose: web `"/review [path] — ..."` (shown in a wide detail pane, includes the reviewer names Cảnh/Kiểm/Hiểu/Phản biện); desktop short argument-hint form `"[path] — ..."` (one truncated row next to the `/review` name), reviewer names omitted.
4. Desktop `SlashMentionField` is shared, so the commands also appear in the New Session dialog composer. Web only wired `ChatView` as asked; `NewChatScreen` passes no commands.
5. Segment order is required, on, off (contract order, default first), unlike autonomy's strictest-first order.

## Not done / not verified

- No live check against a daemon (the daemon side is in another session); behaviour of both controls is verified only against fakes (desktop tests) and the type checker (web). Nothing visually checked in a browser or window.
- vi.json (generated from desktop maps) not regenerated; web strings live in `vi.web.json` with identical English/Vietnamese for the settings text, so the later export cannot conflict.

## Unresolved questions

- Should the five commands also be offered in web `NewChatScreen` (first message of a new chat)? Desktop's new-session composer already shows them.
