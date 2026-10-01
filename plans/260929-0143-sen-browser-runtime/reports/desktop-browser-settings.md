# Desktop: Settings → Browser + `browser` runtime slot

Date: 2026-09-29
Worktree: `/Users/benji/Projects/SenClaw/.worktrees/desktop-browser` (branch `feat/browser-engine-settings`, commit `ad7bcba`)
Spec: `senclaw/plans/260929-0143-sen-browser-runtime/ui-management.md`

## What changed

New Settings → Browser section (placed in the sidebar right after Decision (Laya)), plus recognition of the
`browser` runtime type in the Runtime screen's catalog filter.

### Files

- `lib/features/settings/browser_models.dart` (new, 394 lines) — data classes + providers for
  `/api/browser-agent/{settings,extension,tabs}`: `BrowserSettings`/`BrowserBands`/`BrowserSettingsView`
  (camelCase, mirrors `src/browser_agent/settings.rs`), `BrowserExtensionState` and its
  connected/pending/paired sub-shapes (snake_case, mirrors `src/browser_agent/extension.rs`'s `Hub::status`),
  `BrowserSession`/`BrowserTab`/`BrowserTabsState`, and `browserSettingsDiff(before, after)` — the pure
  function that turns an edited `BrowserSettings` into the partial body `PUT` actually sends (only changed
  top-level keys; a touched list/map/band goes in whole, since the daemon's merge is shallow — verified by
  reading `settings_put` in `src/browser_agent/rest.rs` on the daemon's `feat/sen-browser-v2` branch,
  read-only via `git show`, no checkout).
- `lib/features/settings/browser_section.dart` (new, 969 lines) — `BrowserSection` + private cards:
  `_EngineStatusCard` (engine in use, "runtime not installed" warning + Open Runtime settings button),
  `_SettingsFormCard` (all `BrowserSettings` fields grouped General/Decisions/Sites/Advanced, one Save
  button that PUTs only `browserSettingsDiff`, 422 `error` shown verbatim via SnackBar), `_TagsField` and
  `_DomainDriversField` (self-contained editors for the two domain lists and the host→driver map),
  `_ExtensionCard` (connected/not-connected, pending codes with Approve, paired browsers with
  Remove+confirm; polls `GET .../extension` every 3s only while mounted, via a local `Timer.periodic` +
  `ref.invalidate`, same pattern `DecisionSection` uses), `_ActivityCard` ("Show open tabs" button only —
  no timer — since `GET .../tabs` starts the runtime; runtime-missing errors there reuse the existing
  `RuntimeMissingBanner`/`runtimeMissingFrom`).
- `lib/core/i18n/vi/browser.dart` (new) — Vietnamese strings for every new key in the spec's table.
- `lib/core/i18n/l10n.dart` — wired `viBrowser` in (import + spread).
- `lib/features/settings/settings_screen.dart` — added `'browser'` to `_sections` (right after
  `'decision'`) and its `switch` case.
- `lib/features/settings/runtime_catalog.dart` — added `('browser', 'Browser')` to the catalog's
  `_typeFilters` and a `'browser' => Icons.web_outlined` case to `_typeIcon()`. This is the only
  client-side "known slot/type" list in the app (`RuntimeSlot`/`CatalogEntry` render whatever
  `GET /api/runtimes` sends with no hardcoded slot list — confirmed against `runtime_section.dart` /
  `runtime_models.dart` — so the Runtime Selections row for a `browser` slot needs no code change, only
  the catalog's type filter, which is client-owned).
- `test/browser_models_test.dart` (new) — JSON parsing of the settings and extension (+tabs) payloads,
  defaults-when-missing, and `browserSettingsDiff` (nothing changed → empty; a scalar change → one key; a
  list/map/band change → sent whole; null transitions are sent, not omitted; map key-order doesn't count
  as a change; several edits land as independent keys).
- `test/browser_section_test.dart` (new) — widget tests against a stubbed `ApiClient`: engine status +
  disabled Save on load, a pending code's **Approve** button connecting it (the specifically requested
  case), a paired browser's Remove+confirm flow, Save sending only the one changed field
  (`{'headless': false}`, nothing else), Activity never calling `GET tabs` until the button is pressed (and
  rendering the session/tab it gets back), and a `v2`+installed state showing the connected extension with
  no warning.

## Verification

- `flutter analyze` — clean, 0 issues, both scoped to the 6 touched/new lib files and for the whole project.
- `flutter test test/browser_models_test.dart test/browser_section_test.dart` — 19/19 pass.
- `flutter test` (full suite) — 327 passed, 1 skipped (pre-existing, `i18n_screenshot_test.dart`, marked
  "manual only" — unrelated to this change), 0 failed.
- Manually re-read the diff (`git diff --stat` / `git status`) before committing to confirm only the
  intended 8 files changed, and confirmed the sibling `senclaw` and `desktop` (main) checkouts were left
  exactly as found (pre-existing uncommitted work from other sessions in both, untouched by this task —
  the daemon reads used `git show <branch>:<path>` only, never a checkout).
- Committed on `feat/browser-engine-settings` (commit `ad7bcba`), conventional commit, no AI
  attribution lines. Not pushed.

## Deviations from the spec doc (and why)

- **"Add site" reuses the app's existing Vietnamese value ("Thêm trang") instead of the spec's ("Thêm
  site").** The English key `Add site` already exists in `lib/core/i18n/vi/plugins_misc.dart` (the Space
  App sandbox's allowed-sites list) with `'Thêm trang'`. The desktop app's own i18n rule (`CLAUDE.md`,
  also restated in the orchestrating instructions) is that a shared key must never be redefined with a
  different value in a new file — the last spread in `l10n.dart` would silently win and change the
  *other* screen's text too. I reused the existing key/value rather than fabricate a second, conflicting
  one; documented in a comment at the top of `vi/browser.dart`.
- **No client-side "known type" list needed for the Runtime Selections rows**, only the catalog type
  filter — see the `runtime_catalog.dart` bullet above. This isn't a deviation so much as the spec's one
  sentence ("Add slot/type `browser` to the slot/type unions and the catalog type filter") mapping to a
  single concrete change once I confirmed the slot rows are fully data-driven.
- **Card titles not given verbatim by the spec** (the settings-form card, the engine-status card) were
  chosen to reuse already-translated, unambiguous existing keys: `context.tr('Settings')` (generic, already
  in `common.dart`) for the form card, and the spec's own `Browser engine` for the status card.
- **`current` in the `GET /api/browser-agent/tabs` response is parsed but not surfaced separately** —
  confirmed via the daemon's `tabs()` handler that it's `{managed, extension}` tab ids for the "default"
  chat (Settings has no chat context to scope it to), and the spec's own Activity bullet only asks for
  "sessions with driver/profile and their tabs" — which already carries the per-tab `owner` chat. Kept out
  to avoid rendering a top-level "current" value that means little outside a specific chat.

## Open issues

- None blocking. The daemon side (`feat/sen-browser-v2`) is still its own branch, not on `senclaw` main —
  this UI cannot be exercised against a live daemon until that lands; verification here is by contract
  (read the daemon's actual route/serde code on that branch) plus the stubbed-client tests.
