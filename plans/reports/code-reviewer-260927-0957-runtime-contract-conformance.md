# Runtime contract conformance: daemon vs web-app vs desktop

Date: 2026-09-27. Read-only review against `docs/runtime-protocol.md` §5, plus `/api/embedding-config` and `/api/embedding/*`.

Path prefixes: `rt/` = `senclaw/src/runtime/`, `lm/` = `senclaw/src/local_models/`, `ui/` = `senclaw/src/gateway/ui_server/`, `W/` = `web-app/src/`, `K/` = `desktop/lib/features/settings/` (other desktop dirs are spelled out in full).

## How the review was done

I started from the route registration in `ui/core.rs:645-697,713-716,1161-1172`. For each route I read the handler's serde attributes and `json!` keys, then every client call site and parser. I also checked the relevant library behaviour and neighbouring repos:
- axum 0.7.9 `Json<T>` returns **415** when a request has no `Content-Type: application/json` (`axum-0.7.9/src/json.rs:108-112`, `extract/rejection.rs:32-40`).
- The bundled `runtimes/index.json` lists all four llama.cpp entries with `releases: []` plus an `upstream` block.
- sen-mlx reads `max_kv_tokens` (`sen-mlx/src/settings.rs:73`), so the clients' engine keys are correct.
- sen-sysone's settings view has a `settings` key (`sen-sysone/src/http.rs:326-340`), so the daemon's gate/skills merge lands where both clients expect it.

## Blockers
1. **Neither UI can install any runtime.** The catalog returns `available:false` for every llama.cpp entry, and both clients hide Install when that is false.
2. **The auto-update toggle does nothing**, and `idleTimeoutSecs` is never applied. `PUT /api/runtimes/settings` reads snake_case keys, so the camelCase keys the clients send are ignored.
3. **Load fails with 415.** The desktop always gets it. The web-app gets it for models whose `contextLength` is null.
4. **Desktop local embeddings 404**, because the desktop saves `modelName: "local:<key>"`.
5. **`defaultContextLength` can't be saved** (same casing problem), and even if it were, the daemon never uses it.

## Conformance table

| # | Route | Daemon actual | web-app expectation | desktop expectation | Verdict |
|---|---|---|---|---|---|
| 1 | `GET /api/runtimes` | `rt/rest.rs:42-117`, camelCase `json!`. `processes[].state` is never `starting`: the process is inserted only after the health gate (`rt/supervisor.rs:216-219`). `installed[].version`/`versions` are sorted by **string** compare (`rt/rest.rs:55`), so `b9999` sorts above `b11201` and `0.9.0` above `0.10.0` | `W/lib/runtimeApi.ts:309`, types `:24-78` | `K/runtime_models.dart:193-220`. Polls only while `starting\|stopping` (`K/runtime_section.dart:40-50`) | MISMATCH (`starting` never visible; "newest" is wrong) |
| 2 | `GET /api/runtimes/catalog[?refresh=1]` | `rt/rest.rs:125-167`. `available` ignores `upstream` (`:145-146`), so it is **false for all llama.cpp entries**. `releaseNotesUrl`/`downloadSize` are hard-coded `null` (`:159`). `error` is always `null` because the fetch error is dropped (`:127-135`, `rt/manager.rs:316-319`). On beta, `latestVersion:"latest"` makes `updateAvailable` always true (`:149-153`) | `!available` shows "Not published yet" with no Install button (`W/components/settings/RuntimeCatalogCard.tsx:244-247`). Renders `error` (`:187-191`) and the notes link (`:229-233`) | Same gate (`K/runtime_catalog.dart:478-480`). Error at `:258-259`, notes at `:436` | MISMATCH (blocker) |
| 3 | `POST /api/runtimes/check-updates` | `rt/rest.rs:317-322` returns 200 `{fetchedAt, source, startedJobs:[{id,version,jobId}]}`. On beta, `rt/updates.rs:29-31` compares the literal `"latest"` with installed tags, so every check re-installs llama.cpp | `W/lib/runtimeApi.ts:312`, typed `unknown`, re-fetches | Body ignored (`K/runtime_section.dart:99`) | OK on shape (not pinned in the doc). MISMATCH on beta behaviour |
| 4 | `POST /api/runtimes/install {id, version?}` | `rt/rest.rs:176-180` returns **200** `{jobId,id,version}`; the doc says 202 | `W/lib/runtimeApi.ts:313-314`, `W/hooks/useRuntimes.ts:98-116`, accepts any 2xx | `K/runtime_catalog.dart:95-113`, accepts any 2xx | MISMATCH (status code only, harmless) |
| 5 | `POST /api/runtimes/install-local {path}` | `rt/rest.rs:187-195` returns an `installed[]` entry, with `compatible` hard-coded `true` (`:194`) | `W/lib/runtimeApi.ts:315`, response not read | `K/runtime_catalog.dart:202-205`, response not read | OK |
| 6 | `GET /api/runtimes/jobs` | `rt/rest.rs:197-200` returns `{jobs:[Job]}` | Typed `InstallJob[]` (`W/lib/runtimeApi.ts:316`), no caller | Not called | MISMATCH (latent) |
| 7 | `GET /api/runtimes/jobs/:jobId` | `rt/rest.rs:202-206`, `rt/jobs.rs:21-46`. camelCase; state values match the doc | `W/lib/runtimeApi.ts:108-128,317` | `K/runtime_models.dart:332-373`, `K/runtime_catalog.dart:79-82` | OK |
| 8 | `POST /api/runtimes/jobs/:jobId/cancel` | `rt/rest.rs:208-211` returns `{ok, cancelled}`. The llama.cpp path never receives the cancel token (`rt/jobs.rs:208-210`). A cancelled job keeps `finishedAt:null` (`rt/jobs.rs:168-170`) | Typed `InstallJob` (`W/lib/runtimeApi.ts:318`), no caller | Not called | MISMATCH (latent) |
| 9 | `DELETE /api/runtimes/:id/versions/:version?force=1` | `rt/rest.rs:219-231`: 409 while running, otherwise `{ok:true}` | `W/lib/runtimeApi.ts:319-322`; no force retry | 409 → confirm → `force=1` (`K/runtime_catalog.dart:124-149`) | OK |
| 10 | `PUT /api/runtimes/selections {slot,id\|null,version\|null}` | `rt/rest.rs:233-247` returns the `GET /api/runtimes` body. Nothing ever moves a selection to a new version after an update (§7.3) | Always sends an explicit version (`W/components/settings/RuntimeSelectionsCard.tsx:31`) | Always sends an explicit version (`K/runtime_section.dart:62-68`) | OK on shape. MISMATCH on §7.3 behaviour |
| 11 | `GET /api/runtimes/settings` | `rt/rest.rs:249-256`, camelCase | `W/lib/runtimeApi.ts:325`, no caller | Not called | OK |
| 12 | `PUT /api/runtimes/settings` | `rt/rest.rs:258-282`: `SettingsBody` has **no `rename_all`**, so it reads `auto_update`/`idle_timeout_secs`. camelCase keys are silently dropped and the response is 200 with the old values. Only `channel` takes effect | Sends `{autoUpdate, channel, idleTimeoutSecs}` (`W/hooks/useRuntimes.ts:144-156`) | Same (`K/runtime_section.dart:77-87`) | MISMATCH (blocker) |
| 13 | `POST /api/runtimes/slots/:slot/start` | `rt/rest.rs:284-293` returns a `ProcessSnapshot` without `slot`/`modelKey` (`rt/supervisor.rs:79-92`). Failure is 503 `{error}` with no `code`/`slot` | `W/lib/runtimeApi.ts:327`, typed `RuntimeProcess`. Hook exists (`W/hooks/useRuntimes.ts:175-181`), no UI caller | Not called | MISMATCH (latent) |
| 14 | `POST /api/runtimes/processes/:key/stop` | `rt/rest.rs:295-299` returns `{ok, stopped}`. The key is decoded twice (`:297`) | `W/lib/runtimeApi.ts:328`, `encodeURIComponent` | `K/runtime_section.dart:111-116`, `K/runtime_catalog.dart:156-168` | OK |
| 15 | `GET /api/runtimes/:id/logs?lines=` | `rt/rest.rs:307-315` returns `{path, lines}`, but reads only `<id>.log` (`rt/manager.rs:157-163`). Model-mode processes write `<id>--<key>.log` (`rt/manager.rs:271`) | `W/components/settings/RuntimeLogsModal.tsx:24-26` | `K/runtime_catalog.dart:214-243` | MISMATCH (always empty for llama.cpp and sen-mlx) |
| 16 | Legacy `/api/{ocr,tts,whisper}/*`, `/api/decision/*`, and the 503 body | `ui/core.rs:645-650,663-664`; `rt/proxy.rs:27-52` returns `{error, code, slot}` with slot `ocr\|tts\|asr\|decision`. Message is `"no OCR runtime is installed. Install one in Settings -> Runtime."` (`rt/manager.rs:37-45`); the doc says `"No … → Runtime."`. 502/500 responses use `code:"runtime_error"`, which is not in the SDK codes (`rt/manager.rs:60`) | `W/lib/runtimeApi.ts:254-297`; `OcrSettings.tsx:105`, `TtsSettings.tsx:95`, `WhisperSettings.tsx:125`, `ChatView.tsx:323`, `utils/ttsPipeline.ts:117` | `desktop/lib/core/transport/runtime_missing.dart:12-65`; `K/settings_screen.dart:3736`; `desktop/lib/features/chat/audio_service.dart:49,166` | OK (message text differs cosmetically) |
| 17 | Control plane: `GET\|PUT /api/decision/gate`, `POST …/gate/check`, `GET\|PUT /api/decision/skills`, `POST …/skills/check` | `ui/core.rs:655-658`, `ui/decision.rs:48-219`. The gate view no longer includes `backend` (`:56-63`) | Paths match (`DecisionGateCard.tsx:108,130,145`, `DecisionSkillsCard.tsx:44,67,82`). Cards stay visible when the runtime is missing (`DecisionSettings.tsx:579-580`). `GateView.backend` is now a dead field (`decisionApi.ts:258`, `DecisionGateCard.tsx:266`) | Paths match (`K/decision_gate.dart:107,164,186`, `K/decision_skills.dart:38,82,101`). **On a 503 only the banner renders** (`K/decision_section.dart:172-179`), so the gate/skills cards (`:228-229`) can't be reached without sen-sysone installed. Uses `backend ?? 'local'` (`K/decision_gate.dart:77,395`) | MISMATCH (desktop) |
| 18 | `GET\|PUT /api/decision/settings` | `rt/proxy.rs:138-206`. The merge into `settings` is correct, and PUT correctly strips gate/skills. But any non-2xx from the runtime becomes **502 `runtime_error`**, with the runtime's raw body nested inside `error` (`:155-160`). §4.3 promises the original status codes and bodies | `DecisionSettings.tsx:229-234`, `DecisionRunSettings.tsx:99` | `K/decision_run_settings.dart:19,107` | MISMATCH (error path) |
| 19 | `GET /api/local-models` | `lm/rest.rs:53-82`. `LocalModel` keys are correct. `process` carries only `{pid,port,state,startedAt,lastUsedAt,launches,error}` (`:55-61`). `runtime.selected` is `{id,version}` with no `name` (`:62-66`). `starting` is never visible (see #1) | Typed `process: RuntimeProcess`, `selected: SlotCandidate` (`W/lib/runtimeApi.ts:139-154`). Polls on `starting` (`W/hooks/useLocalModels.ts:45`) | Parses defensively (`K/local_models_section.dart:36-97`). Sets `loading` on `starting` (`:78`) | MISMATCH (partial process object; `starting` never visible) |
| 20 | `GET /api/local-models/hf-files?repo=` | `lm/rest.rs:84-96`, `lm/hf_files.rs:11-26`. Accepts `revision` | `W/lib/runtimeApi.ts:337`. The dialog's revision field is never passed here (`LocalModelDownloadDialog.tsx:53`) | `K/local_models_section.dart:530-532` | OK (the web lookup ignores revision) |
| 21 | `POST /api/local-models/download {repo,file?,mmproj?,revision?}` | `lm/rest.rs:98-118` returns **200** `{downloadId}`; the doc says 202 | `W/lib/runtimeApi.ts:338-339`. Blocks `unknown` format | `K/local_models_section.dart:556-572`. Download is enabled for `unknown` and even before any lookup (`:507,678`) | MISMATCH (status code only) |
| 22 | `GET /api/local-models/downloads` | `lm/rest.rs:120-122` returns `{downloads:[…]}` | Typed `LocalDownload[]` (`W/lib/runtimeApi.ts:340`), no caller | Not called | MISMATCH (latent) |
| 23 | `GET /api/local-models/downloads/:id` and `downloads[].state` | `lm/download.rs:22-47`: states are `queued\|listing\|downloading\|done\|error\|cancelled`; `totalBytes` is 0 when unknown | Union has `verifying`/`failed` (`W/lib/runtimeApi.ts:162`). "Active" = queued/downloading/verifying (`W/hooks/useLocalModels.ts:4`, `LocalModelDownloadDialog.tsx:17`). Failure styling keys on `'failed'` (`:200`) | "Active" = queued/downloading (`K/local_models_section.dart:117,123`) | MISMATCH: `listing` is treated as finished, so polling and Cancel stop right after a download starts. `error` ≠ `failed` |
| 24 | `POST /api/local-models/downloads/:id/cancel` | `lm/rest.rs:129-131` returns `{ok, cancelled}` | Typed `LocalDownload` (`W/lib/runtimeApi.ts:342`); return value unused | `K/local_models_section.dart:242-251` | OK (type drift only) |
| 25 | `DELETE /api/local-models/:key?force=1` | `lm/rest.rs:133-170`: 409 while loaded | Always `force=1` (`LocalModelsSettings.tsx:140`) | Never forces (`K/local_models_section.dart:237-239`), so the raw "pass force=1…" text is shown | OK (desktop UX gap) |
| 26 | `POST /api/local-models/:key/load {contextLength?}` | `lm/rest.rs:172-196`. `Json<LoadBody>` returns **415** when there is no body or Content-Type. `LoadBody.context_length` has no `rename_all`, so `contextLength` is ignored. Failure is 503 `{error}` **without `code`/`slot`** (`:193`). The request blocks until healthy (llama.cpp allows 600 s) | Omits the body when `contextLength` is falsy, which gets a 415 (`W/lib/runtimeApi.ts:345-349`). Expects the runtime-missing code (`LocalModelsSettings.tsx:36`) | **Never sends a body, so it always gets 415** (`K/local_models_section.dart:207-211`). `post` is capped at 30 s (`desktop/lib/core/transport/api_client.dart:29,69-70`) | MISMATCH (blocker) |
| 27 | `POST /api/local-models/:key/unload` | `lm/rest.rs:198-205` returns `{ok, unloaded}` | `W/lib/runtimeApi.ts:350` | `K/local_models_section.dart:213-217` | OK |
| 28 | `GET /api/local-models/settings` | `lm/rest.rs:207-211` returns `{defaultContextLength: number\|null, engine}` | Types it as `number` (`W/lib/runtimeApi.ts:198-201`) | `K/local_models_section.dart:742-760` | OK (nullability only) |
| 29 | `PUT /api/local-models/settings` | `lm/rest.rs:213-233`: `default_context_length` has no `rename_all`, so `defaultContextLength` is dropped. The stored value is never read at launch anyway (`lm/rest.rs:189`, `rt/proxy.rs:231`, `lm/rest.rs:44`) | `W/components/settings/LocalModelEngineSettingsCard.tsx:52` | `K/local_models_section.dart:772-786`. Also writes `enable_thinking:false` when the key was absent (`:758,784`) | MISMATCH |
| 30 | `ANY /api/runtimes/models/:key/v1/*` | `ui/core.rs:697`, `rt/proxy.rs:213-263` | Not called (daemon-internal) | Not called | OK |
| 31 | `GET\|POST /api/embedding-config` with `provider:"local"` | `ui/embedding_config.rs:12-57`. `modelName` is used verbatim as the model key (`senclaw/src/config.rs:919` → `senclaw/src/memory/embedding_providers.rs:436-439`). `modelPath` is never read | Sends the bare `m.key`, which is correct (`EmbeddingSettings.tsx:250-253`). The preset default `'all-MiniLM-L6-v2'` (`:56-61`) passes the `required` check and then 404s | **Sends `local:${m.key}`** (`K/settings_screen.dart:4100,4109`), which becomes `/api/runtimes/models/local:<key>/…` → 404. Same stale preset (`:3862`). Dead `modelPath` field (`:3994-4000`) | MISMATCH (desktop) |
| 32 | `GET /api/embedding/features`, `GET /api/embedding/models`, `POST /api/embedding/download-model` | `ui/core.rs:1161-1172`, `ui/embedding_models.rs`: new shapes, and 410 for download | Not called | Not called | OK (no remaining consumer) |

## Fix list, in priority order

### P0: features that don't work today
1. **Daemon: fix catalog `available` and fill the missing fields** (`rt/rest.rs:139-160`). The daemon diverged from §5.1, which defines `available:false` as "no package published for this platform/channel".
   ```rust
   let latest = e.channel_version(settings.channel);
   let pkg = latest.and_then(|v| e.release(v)).and_then(|r| e.package_for(r, platform));
   let available = compatible && match &e.upstream {
       Some(u) => u.assets.contains_key(platform),
       None => pkg.is_some(),
   };
   // "releaseNotesUrl": latest.and_then(|v| e.release(v)).and_then(|r| r.notes_url.clone()),
   // "downloadSize":    pkg.and_then(|p| p.size),
   ```
   For upstream entries with a concrete tag, set `releaseNotesUrl` to `https://github.com/{upstream.repo}/releases/tag/{tag}`. No client change needed.
2. **Daemon: add `#[serde(rename_all = "camelCase")]` to `SettingsBody`** (`rt/rest.rs:258`). The protocol and both clients use camelCase.
3. **Daemon: fix `POST /api/local-models/:key/load`** (`lm/rest.rs:172-196`). The protocol makes the body optional, so the daemon diverged.
   ```rust
   #[derive(Deserialize, Default)]
   #[serde(rename_all = "camelCase")]
   pub(crate) struct LoadBody { #[serde(default)] context_length: Option<u32> }
   // handler: take `body: axum::body::Bytes` instead of Json<LoadBody>
   let body: LoadBody = if body.is_empty() { LoadBody::default() }
       else { serde_json::from_slice(&body).map_err(|e| bad(e.to_string()))? };
   ```
   - On an `ensure_model_started` error, return the §5.2 body. Make `rt/proxy.rs::error_response` `pub(crate)` and return `error_response(e, model.slot())`, changing the handler's return type to `Response`.
   - Apply the same error body to `post_slot_start` (`rt/rest.rs:287-290`).
   - No desktop change is needed once the empty body is accepted.
4. **Desktop: send the bare model key for local embeddings.** At `K/settings_screen.dart:4100` and `:4109`, use `m.key` instead of `'local:${m.key}'`, and fix the doc comment at `:4041`. The daemon and web already agree on the bare key.
   - **Daemon hardening:** in `LocalProvider::new` (`senclaw/src/memory/embedding_providers.rs:380-382`), strip a leading `local:`. Desktop builds may already have written that prefix into `config.json`.
   - **Both clients:** change the stale `local` preset `modelName` from `'all-MiniLM-L6-v2'` to `''` (`W/components/settings/EmbeddingSettings.tsx:59`, `K/settings_screen.dart:3862`). Rename the web label "Local (candle / on-device)" (`EmbeddingSettings.tsx:69`) to "Local (GGUF / on-device)".
5. **Daemon: make local-model settings save and actually apply.**
   - Add `#[serde(rename_all = "camelCase")]` to `SettingsBody` (`lm/rest.rs:213`).
   - Treat PUT as a full object and always write `default_context_length`, so a `null` clears it. Both clients send the full object.
   - Launch context should be: explicit `contextLength`, then `defaultContextLength` (clamped to the model's max), then the model's max, then 4096. Apply this in `post_load` (`lm/rest.rs:189`), `proxy_model` (`rt/proxy.rs:231`), and `llm_configs` (`lm/rest.rs:44`), so the agent's context budget matches the `-c` the runtime was launched with.

### P1: wrong or misleading state
6. **Download state vocabulary.** The daemon renamed the install-job state `failed` to `error` for downloads, with no reason.
   - Daemon (`lm/download.rs:22-31`): rename `DownloadStatus::Error` to `Failed` and update its uses at `:67`, `:165-167`, `:317`.
   - Protocol §5.3: pin `state: "queued"|"listing"|"downloading"|"done"|"failed"|"cancelled"`.
   - Clients: add `listing` to the "active" sets. Web: `W/hooks/useLocalModels.ts:4` and `W/components/settings/LocalModelDownloadDialog.tsx:17` become `['queued','listing','downloading']`; the union at `W/lib/runtimeApi.ts:162` becomes the pinned enum. Desktop: `K/local_models_section.dart:123` becomes `state == 'queued' || state == 'listing' || state == 'downloading'`.
7. **Daemon: make `starting` visible.** `rt/supervisor.rs:216-219` inserts the process only after `spawn()` passes the health gate.
   - Split `spawn` into launch (child running, `RunningProcess{state: Starting}`) and a health wait. Insert the process and write `running.json` right after launch (§3.2 step 3). Set `Ready` or `Failed` after the health wait.
   - The idle sweep must skip `Starting`, and `stop` must kill a starting child.
   - This makes the existing client polls work (web `useLocalModels.ts:45`; desktop `runtime_section.dart:41-42` and `local_models_section.dart:78`).
8. **Desktop: longer timeout for model loads.** Add `Duration? timeout` to `ApiClient.post` (`desktop/lib/core/transport/api_client.dart:69-70`, mirroring `get`). Pass `const Duration(minutes: 11)` in `_load` (`K/local_models_section.dart:207-211`) to cover llama.cpp's 600 s health budget.
9. **Desktop: keep the gate and skills cards when the decision runtime is missing.** In `K/decision_section.dart:176-179`, have the error branch return a `Column` with `RuntimeMissingBanner`, `DecisionGateCard`, and `DecisionSkillsCard`, matching web `DecisionSettings.tsx:579-580`. Also remove the obsolete "no Laya engine" warning keyed on `backend` (`K/decision_gate.dart:395-399`), since the daemon no longer sends `backend`.
10. **Daemon: pass decision-settings errors through unchanged** (`rt/proxy.rs:155-160`). On a non-2xx, return the runtime's own status and body instead of wrapping them:
    ```rust
    return Err((StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                [(axum::http::header::CONTENT_TYPE, "application/json")], text).into_response());
    ```
11. **Daemon: compare versions properly.** Add a `cmp_versions` helper: `b<N>` compared numerically; dotted numerics compared segment by segment, with a `-pre` suffix sorting lower; otherwise a string fallback. Use it at `rt/rest.rs:55`, `rt/rest.rs:148` (`max_by`), and `rt/manager.rs:183`. This fixes `installed[].version`, `versions` order, `installedVersion`, and slot resolution when `version` is null.
12. **Daemon: logs for model-mode runtimes** (`rt/manager.rs:157-163`). When `<id>.log` is missing or the runtime is model-mode, tail the most recently modified `<id>--*.log`. Optionally accept `?key=` and add it to §5.1. No client change needed.
13. **Daemon: stop treating `"latest"` as a real version on beta** (`rt/updates.rs:29-31`, `rt/rest.rs:149-153`). Resolve it to a concrete tag (cache what `llamacpp::resolve` returns in `index-cache.json` during `refresh_index`) before comparing against installed versions. Report the resolved tag as `latestVersion`; report `updateAvailable:false` until it is resolved.
14. **Daemon: move the slot after an update (§7.3).** On job `Done` (`rt/jobs.rs:171-176`), for every slot selected to `job.id`, set `version = job.version` once no process of the old version is running; otherwise defer to the idle sweep. See the first open question below.

### P2: status codes, latent types, polish
15. **Daemon: return 202** from `POST /api/runtimes/install` (`rt/rest.rs:176-180`) and `POST /api/local-models/download` (`lm/rest.rs:109-118`), as `(StatusCode::ACCEPTED, Json(..))`. Clients already accept any 2xx.
16. **Daemon: return the fetch error.** Make `refresh_index` return the error (`rt/manager.rs:316-319`) and put it in the catalog's `error` field (`rt/rest.rs:127-135`). Both clients already render it.
17. **Daemon: one process shape everywhere.** Add a single `process_view(snapshot, slot, model_key)` helper, shaped like `processes[]` including `slot` and `modelKey`, and use it at `rt/rest.rs:98-103`, `lm/rest.rs:55-61`, `lm/rest.rs:195`, and `rt/rest.rs:292`. Add `"name"` to `runtime.selected` (`lm/rest.rs:66`). In `install-local`, compute `compatible` instead of hard-coding `true` (`rt/rest.rs:194`).
18. **Web: correct the unused types** so they don't mislead future callers.
    - `W/lib/runtimeApi.ts:316`: `jobs()` returns `{jobs: InstallJob[]}`.
    - `:318`: `cancelJob()` returns `{ok: boolean; cancelled: boolean}`.
    - `:340`: `downloads()` returns `{downloads: LocalDownload[]}`.
    - `:342`: `cancelDownload()` returns `{ok; cancelled}`.
    - `:199`: `defaultContextLength: number | null`.
    - `:93`: `latestVersion: string | null`.
    - `:61`: `slot: SlotId | null`.
    - Remove `GateView.backend` (`W/components/settings/decisionApi.ts:258`) and the warning that reads it (`DecisionGateCard.tsx:266`).
19. **Desktop polish:**
    - Refresh the catalog on channel change: in `_saveSettings`, also invalidate `runtimeCatalogProvider` (`K/runtime_section.dart:88`).
    - Only send `enable_thinking` if it was present or the user changed it, using a tri-state like the web (`K/local_models_section.dart:758,784`).
    - Delete of a loaded model: on 409, confirm and retry with `?force=1` (`:237-239`), the same way uninstall works (`K/runtime_catalog.dart:130-147`).
    - Disable Download until a lookup succeeds and whenever `_format == 'unknown'` (`K/local_models_section.dart:678`).
    - Remove the dead `modelPath` field (`K/settings_screen.dart:3994-4000`).
20. **Web: pass the revision on file lookup.** `hfFiles(repo, revision?)` should add `&revision=` (`W/lib/runtimeApi.ts:337`), called from `LocalModelDownloadDialog.tsx:53`.
21. **Protocol doc (§5.1/§5.3): pin the shapes nobody wrote down.**
    - `check-updates` returns `{fetchedAt, source, startedJobs:[{id,version,jobId}]}`.
    - `install-local` returns an `installed[]` entry.
    - `GET …/jobs` returns `{jobs:[Job]}`.
    - Cancel returns `{ok, cancelled}`; stop `{ok, stopped}`; unload `{ok, unloaded}`; delete `{ok}`.
    - `PUT /api/runtimes/settings` is a partial patch; `PUT /api/local-models/settings` is a full object; the load body is optional.
    - Either align the daemon's 503 text to the doc (`"No {label} runtime is installed. Install one in Settings → Runtime."`, `rt/manager.rs:38-44`) or update the doc.
    - Either add `runtime_error` to `sen_runtime_sdk::api::codes` or drop `code` on 502/500 responses (`rt/manager.rs:60`).

## Outside the wire contract, found along the way
- **Repeated filesystem scans on every poll.** `model_view` calls `manifest_for_slot` once per model (`lm/rest.rs:62-66`), and each call rescans the runtimes directory twice (`rt/manager.rs:170,182`). `get_runtimes` runs `find_by_key`, a full GGUF-header scan, once per model process (`rt/rest.rs:85`). The clients poll every 1–1.5 s. Compute these once per request.
- **Blocking I/O on an async worker.** `install_local` copies or extracts synchronously (`rt/rest.rs:193`). Wrap it in `spawn_blocking`.
- **Deleting one GGUF quant can break its siblings.** `delete_model` removes the mmproj file (`lm/rest.rs:166-168`), but every quant in the same repo directory pairs with that same file (`lm/scan.rs:152`). The other quants silently lose vision.
- **GGUF display names are the architecture.** `name` is `general.architecture` (`lm/scan.rs:167`), so every Llama/Qwen quant is listed as "llama"/"qwen2". Prefer `general.name` or the file stem.
- **Job cancel is a no-op for llama.cpp installs**, and cancelled jobs never set `finishedAt` (`rt/jobs.rs:168-170,208-210`).
- **The stop key is decoded twice** (`rt/rest.rs:297`). axum's `Path` already percent-decodes.

## Checks
- **Concurrency:** the start is single-flight, but the `starting` state is invisible (fix 7); `running.json` is written only after the health gate.
- **Error boundaries:** fixes 3 and 10.
- **API contracts:** covered by the table.
- **Backward compatibility:** `/api/embedding/*` changed shape but has no remaining consumer.
- **Input validation:** `normalize_repo` and `Slot::parse` are fine.
- **Auth:** every route sits behind the daemon's auth middleware, and the proxy strips client credentials (`rt/proxy.rs:24`).
- **Performance:** the repeated scans noted above.
- **Data leaks:** none found.

**Metrics:** I ran no tests or builds; this was a static conformance review.

## Open questions
1. §7.3 slot move: should the daemon re-point an explicitly versioned selection after an update (fix 14), or should the clients send `version: null` to mean "track newest"? This is a product decision.
2. When `defaultContextLength` is unset, should the launch fall back to the model's full max (the current behaviour, which launches 128K-context models with a full KV cache) or to a fixed cap such as 8192?
3. Should `PUT /api/local-models/settings` stay a full-object replace (my assumption in fix 5) or become a patch like `/api/runtimes/settings`?

Status: DONE
Summary: 32 routes/surfaces checked. 19 have at least one mismatch (5 are blockers) and 13 conform. The blockers are: `available:false` for all llama.cpp entries so nothing can be installed; three request structs missing camelCase renames (runtime settings, load, local-model settings); a bodyless Load rejected with 415; and the desktop's `local:`-prefixed embedding key. Fix list is prioritized, with the side to change and exact edits.
