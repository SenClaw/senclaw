# Phase 05 — Web app: implementation report

Plan: `senclaw/plans/260927-0112-runtime-split-jev-migration/`
Repo touched: `/Users/benji/Projects/SenClaw/web-app` only.
Status: **completed**. `npm install` and `npm run build` (tsc -b && vite build) both green, no new TypeScript errors, no existing lint/test scripts (none configured in `package.json` beyond dev/build/preview).

## Screens added

- **Settings → Runtime** (LM Studio-style engine manager), new section in `SettingsPage.tsx`:
  - Runtime Selections card (per-slot dropdown + monospace version chip, auto-update toggle)
  - Runtime updates channel card (tooltip, Check for updates, Stable/Beta)
  - Engines & Frameworks card (search, Compatible-only/All, type filter; per-row icon/name/version-chip-with-arrow/description/release-notes link; Install/Update button with live ~1s-polled progress, "Latest version"/"Not published yet"/"Incompatible" states; kebab menu — uninstall a version (submenu when multiple installed), install from a local path, view logs, stop running processes)
  - Running list (process table: runtime/version/slot/model key, state, port, uptime, launches, Stop)
  - Install-from-local-path modal (text input — no file picker, per spec: the package lives on the daemon's filesystem)
  - Logs modal (`GET /api/runtimes/:id/logs`)
- **Settings → Local models**: model table (name, GGUF/MLX badge, size, vision/embedding chips, per-row "no runtime selected for this format" hint linking to Runtime, Load/Unload/Delete), HF download dialog (repo → `hf-files` → pick a GGUF file + optional mmproj, or the whole MLX snapshot → progress with cancel), engine settings card (default context length + temperature/top_k/top_p/max_new_tokens/max_kv_tokens/enable_thinking, round-tripping `<local-models>/settings.json` untouched for any field it doesn't render).

## Screens changed

- **`OcrSettings.tsx` / `TtsSettings.tsx` / `WhisperSettings.tsx` / `DecisionSettings.tsx`**: a 503 with `code` `runtime_not_installed`/`runtime_not_selected`/`runtime_start_failed` on the model-list fetch now replaces the panel with `RuntimeMissingBanner` (message + "Open Runtime settings" link) instead of a generic error toast. Decision's `DecisionGateCard`/`DecisionSkillsCard` keep rendering and working in that state (they hit the daemon-native `/api/decision/gate`/`/skills`, not the proxied `sen-sysone` routes).
- **`EmbeddingSettings.tsx`**: the `local` provider is now a GGUF embedding model picked from Local models (`embedding: true` capability) instead of the old candle HF-download UI (candle left the daemon entirely — dropped `/api/embedding/{features,models,download-model}` usage). Shows an inline warning (not a full banner, rest of the form still works) when the selected model's GGUF slot has no runtime chosen.
- **`ChatView.tsx`** (mic → `/api/whisper/transcribe`) and **`MessageBubble.tsx`** (read-aloud → `/api/tts/synthesize` via `ttsPipeline.speakPipelined`): the same 503 shape now surfaces as `message.error` with a click-through link to `/settings?section=runtime`, instead of a bare "recognition/synthesis failed" toast.
- **`SettingsPage.tsx`**: added `runtime`/`local-models` sections; active section is now deep-linkable via `?section=` (`useSearchParams`) specifically so the banners above and the chat-side links can navigate straight there.
- **`decisionApi.ts`**: `api()` now throws the shared `ApiError` (re-exported from `lib/runtimeApi`) carrying `status`/`code`/`slot`, so Decision's 503 is recognized by the same `runtimeMissingFromError` helper as everything else.

## Files

New (17): `src/lib/runtimeApi.ts`; `src/hooks/useRuntimes.ts`, `useLocalModels.ts`; `src/components/settings/RuntimeSettings.tsx`, `RuntimeSelectionsCard.tsx`, `RuntimeUpdatesChannelCard.tsx`, `RuntimeCatalogCard.tsx`, `RuntimeProcessesCard.tsx`, `RuntimeInstallLocalModal.tsx`, `RuntimeLogsModal.tsx`, `RuntimeMissingBanner.tsx`, `LocalModelsSettings.tsx`, `LocalModelDownloadDialog.tsx`, `LocalModelEngineSettingsCard.tsx`; `README.md`; `CLAUDE.md`; `.github/workflows/release.yml`.

Modified (12): `src/pages/SettingsPage.tsx`; `src/components/settings/OcrSettings.tsx`, `TtsSettings.tsx`, `WhisperSettings.tsx`, `DecisionSettings.tsx`, `decisionApi.ts`, `EmbeddingSettings.tsx`; `src/components/ChatView.tsx`, `MessageBubble.tsx`; `src/utils/ttsPipeline.ts`; `src/i18n/vi.web.json` (+103 new keys, all verified present, zero duplicate JSON keys, zero missing vs a full source scan); `vite.config.ts`.

## Release workflow

`.github/workflows/release.yml`: builds on `v*` tags (+ manual dispatch), packs `dist/` into `senclaw-web-dist.tar.gz` (flat tree, matching the old repo's asset layout the daemon's `distrib.rs`/`senclaw web` installer expects), attaches it to the GitHub release via `softprops/action-gh-release`. `vite.config.ts` proxy target is now `VITE_DAEMON_URL`-configurable (default `http://127.0.0.1:18788`; documented in README with the `28788` test-daemon example from the plan's rules).

## Build result

- `npm install`: clean (460 packages; pre-existing `npm audit` findings unrelated to this change, untouched).
- `npx tsc -b --force`: exit 0, zero errors, run twice more after later edits (i18n, embedding rewrite) — stayed clean throughout.
- `npm run build`: exit 0 both before my changes (baseline) and after; same pre-existing "chunk larger than 500 kB" warning as baseline (mermaid/cytoscape/elk vendor chunks) — not introduced by this phase, not addressed (out of scope).
- No lint/test script exists in `package.json` beyond `dev`/`build`/`preview` — nothing else to run.
- Never pointed any dev server at the real daemon (18788/18789); no `vite`/`npm run dev` was started during this work, only `npm install` and `npm run build`.

## Contract gaps found (flagged for the daemon side / phase 01 & 06)

1. `POST /api/runtimes/check-updates` response shape isn't pinned in `docs/runtime-protocol.md` §5.1. Typed as `unknown` client-side; the UI treats it as a trigger and always re-fetches `GET /api/runtimes` + `/catalog` afterward rather than reading fields off it.
2. `POST /api/runtimes/install-local` return shape is assumed `InstalledRuntime`-like (not explicit in the doc). The UI doesn't read any field from it — it just calls `refresh()` after — so a mismatch is low-risk.
3. `LocalDownload.state` (§5.3) isn't exhaustively enumerated the way `InstallJobState` is. Typed with a `string & {}` escape hatch and rendered through `t(state)` (falls back to the raw English word) rather than an exhaustive switch, so an unanticipated value degrades instead of breaking.
4. **Biggest gap**: `/api/embedding-config`'s wire shape for `provider: "local"` isn't covered by `docs/runtime-protocol.md` at all (that route predates the runtime split and sits outside §5). I mapped `modelName` to the bare `LocalModel.key`, grounded in §5.5's own route (`/api/runtimes/models/<key>/v1/embeddings`) — this needs explicit confirmation once the daemon's embedding-config handler is rewritten in phase 01, since "local" previously meant an in-process candle model name, not a Local-models key.
5. `PUT /api/runtimes/settings` — unclear if it accepts a partial patch or requires the full `{autoUpdate, channel, idleTimeoutSecs}` object. The client always sends the full merged object defensively (spread last-known settings + patch), so it's correct either way.
6. Local models "shared engine settings" fields end with "…" in the phase spec (implying more may exist beyond `temperature/top_k/top_p/max_new_tokens/max_kv_tokens/enable_thinking`). The settings card exposes exactly those six and round-trips any other pre-existing key untouched (spread-then-patch), so nothing is silently dropped, but nothing beyond the six is editable yet.

## Environment note

Machine-wide disk space dropped to ~929 MB free partway through final verification (after a coordinator-reported cleanup had brought it to ~2.6 GB) — almost certainly other parallel phases' Rust/Flutter builds. I paused further `npm`/build commands and notified the main session per its instruction once free space fell below 1.5 GB; my own `tsc`/`build` runs had already completed green immediately before the drop, so no results here are affected, and this repo's `node_modules`/`dist` add negligible footprint by comparison.

## Update: runtime-contract conformance fixes (follow-up task)

Source: `code-reviewer-260927-0957-runtime-contract-conformance.md` (web rows + fix list) plus the lead's pinned open decisions in the updated `docs/runtime-protocol.md` §5. Daemon is being fixed to the same doc in parallel — coded to the doc, not to today's daemon behavior. All items below closed the gaps my own earlier report flagged in "Contract gaps found" (now resolved by the pinned doc).

1. **Download state vocabulary** (§5.3 pinned: `queued|listing|downloading|done|failed|cancelled`, first three active). Fixed `LocalDownload.state` type (dropped `verifying` — that's only an install-job state — added `listing`) in `src/lib/runtimeApi.ts`; `ACTIVE_DOWNLOAD_STATES`/`ACTIVE_STATES` in `useLocalModels.ts` and `LocalModelDownloadDialog.tsx` now `['queued','listing','downloading']`. Failure styling already keyed on `'failed'` (was already correct). Added `"listing"` to `vi.web.json`.
2. **Wrong types on 4 unused API helpers + dead `GateView.backend`**: `runtimeApi.jobs()` → `{jobs: InstallJob[]}`, `cancelJob()` → `{ok, cancelled}`, `localModelsApi.downloads()` → `{downloads: LocalDownload[]}`, `cancelDownload()` → `{ok, cancelled}`. Also widened `LocalModelsSettings.defaultContextLength`, `CatalogEntry.latestVersion` to `| null`, and `RuntimeProcess.slot` to `SlotId | null` (per the report's item 18). Removed `GateView.backend` from `decisionApi.ts` and the now-permanently-dead warning block that read it in `DecisionGateCard.tsx` (its guard `view.backend === 'local'` could never be true once the field is gone) — also dropped the now-unused `compiled` prop `DecisionGateCard` took only for that warning, updating both call sites in `DecisionSettings.tsx`.
3. **Embedding local provider**: was already sending the bare `m.key` (correct); fixed the stale default — `PROVIDER_DEFAULTS.local.modelName` was `'all-MiniLM-L6-v2'` (passed the `required` validator, then 404'd since no such Local-models key exists) → now `''`, forcing an explicit pick. Relabeled `PROVIDER_LABELS.local` from "Local (candle / on-device)" to "Local (GGUF / on-device)".
4. **Selections default to `version: null`**: `RuntimeSelectionsCard.tsx` now sends `null` (track newest) when the picked candidate's version matches that id's current newest install (the normal case), and only pins an explicit version when the user picks a candidate that is *not* the newest (an older build still installed alongside it) — resolves the report's open question #1 per the doc's new pinned text.
5. **Load body + response + banner**: `localModelsApi.load()` now always sends a JSON body (`{}` when no `contextLength}`, never `undefined`) — omitting it entirely got today's daemon's `Json<T>` extractor a 415, per the report's blocker #3. Response was already typed as the process object (no change needed there). `LocalModelsSettings.tsx` now shows `RuntimeMissingBanner` (not just a toast) above the table when any row action 503s with a runtime-missing code, cleared on the next successful action.
6. **Poll while `starting`**: `useLocalModels.ts` already did (unchanged). Added the same to `useRuntimes.ts` (was missing) — a new interval polls `GET /api/runtimes` at 1s while any `view.processes[]` is `starting`, so the Running list and slot-selection status catch up once the health gate passes, mirroring the doc's "a process is visible as starting from the moment it is launched" note.
7. **PUT settings as partial merge**: `useRuntimes.ts`'s `saveSettings` no longer reconstructs `{...base, ...patch}` before sending — it sends only `patch` and applies the daemon's returned full object as new state (avoids a race where a locally-stale copy of an untouched field, e.g. `idleTimeoutSecs`, would silently overwrite a concurrent change). Local-models settings was already effectively a full, correct object every save (its `engine` blob is documented as passed through *whole*, not deep-merged, so reconstructing the complete engine object before sending is required, not a bug) — left unchanged after re-confirming against the newly-pinned §5.3 text.
8. **Also from the report's fix list**: typed `checkUpdates()`'s response as the now-pinned `{checkedAt, channel, updates, started, error}` shape (`CheckUpdatesResult`) instead of `unknown` — closes contract gap #1 from my first report. Added `revision` threading: `hfFiles(repo, revision?)` now appends `&revision=`, wired from `LocalModelDownloadDialog.tsx`'s revision field (previously ignored).

Not changed, with reasoning: uninstall-on-409 auto-retry, logs `?key=` param, and the 200-vs-202 install status codes are all explicitly marked "OK" or "no client change needed" for web in the report — left alone to avoid unrequested scope. `RuntimeProcess.slot` staying non-null in the doc's own inline JSON example vs. the `| null` widening requested: applied the widening anyway (backward-compatible, defensive, and explicitly requested) and confirmed every render site (`RuntimeProcessesCard.tsx`, `RuntimeCatalogCard.tsx`) degrades gracefully (React renders `null` as nothing) rather than crashing.

Files touched this round (all within `web-app/`, no new files): `src/lib/runtimeApi.ts`, `src/hooks/useRuntimes.ts`, `src/hooks/useLocalModels.ts`, `src/components/settings/RuntimeSelectionsCard.tsx`, `RuntimeCatalogCard.tsx`, `LocalModelDownloadDialog.tsx`, `LocalModelsSettings.tsx`, `LocalModelEngineSettingsCard.tsx`, `EmbeddingSettings.tsx`, `decisionApi.ts`, `DecisionGateCard.tsx`, `DecisionSettings.tsx`, `src/i18n/vi.web.json` (+1 key: `listing`).

Verification: `npx tsc -b --force` exit 0 (caught 2 real fallout errors from the `latestVersion | null` widening in `RuntimeCatalogCard.tsx` — fixed by guarding the install/job-lookup call sites and the two display spots). `npm run build` exit 0, same pre-existing chunk-size warning as before, nothing new.

## Unresolved questions

- All five contract gaps from the original report are now resolved by the pinned `docs/runtime-protocol.md` update, except: confirm once the daemon lands that `PUT /api/local-models/settings` really does treat `engine` as whole-object-replace-when-present (my reading of "passed through snake_case untouched" + "partial body... keeps what it does not name" at the top level only) rather than a deep merge — if it's deep-merged instead, `LocalModelEngineSettingsCard.tsx`'s spread-then-patch approach is still correct either way, so this is a "confirm, not urgent" item.

Status: DONE
Summary: Implemented Settings → Runtime and Settings → Local models (LM Studio-style engine manager + shared model library), runtime-missing banners across OCR/TTS/Whisper/Decision settings and chat voice features, rewired Embedding settings' `local` provider onto Local models, added the release workflow + configurable dev proxy + README/CLAUDE.md; then closed all 8 web-side runtime-contract conformance items from the code-reviewer's report (download-state vocabulary, 4 mistyped API helpers + dead `GateView.backend`, embedding default/label, selections `version: null` default, Load body/response/banner, starting-state polling on the Runtime screen, settings-as-partial-merge, checkUpdates typing + hf-files revision). `npm run build` green throughout every round, no new TS errors.
Concerns: None blocking. One low-priority confirm-not-urgent item remains (local-models `engine` deep-merge-vs-replace semantics, noted above) — current client code is correct under either daemon interpretation.
