# Phase 05 — Web app (`web-app/`, React + Vite + Tailwind + antd)

Owner: one agent. Repo: `/Users/benji/Projects/SenClaw/web-app` (only). Contract: `senclaw/docs/runtime-protocol.md` §5.
The daemon side is built in parallel against the same contract — code to the contract. Node 24 / npm are installed;
run `npm install` first (network is available).

## Context

`web-app/` is the old `web/`: the React UI the daemon serves (dev fallback now `../web-app/dist`, release asset
`senclaw-web-dist.tar.gz`). Engines left the daemon (see phase 04 context) and the REST paths for OCR/TTS/Whisper/Decision
stay the same through the daemon's proxy.

## Build

Same product scope as the desktop (keep the two clients at parity — `docs/client-parity.md` in the old repo):

1. **Settings → Runtime** (LM Studio-style; see phase 04 "Build 1" and its reference description): Runtime Selections with
   auto-update toggle, updates channel + Check for updates, Engines & Frameworks list with search / compatible / type
   filters, per-row Install / Update / progress / Latest / Not published / Incompatible and a menu (uninstall version,
   install from a local path — text input, since a browser cannot pick a daemon-side folder — logs, stop), Running list.
2. **Settings → Local models**: list, load/unload/delete, HF download dialog with file picking and progress, settings.
3. **Runtime-missing state** in `OcrSettings`, `TtsSettings`, `WhisperSettings`, `DecisionSettings` (+ gate/skills cards keep
   working — those routes stay daemon-native), `utils/ttsPipeline.ts` and voice in `ChatView.tsx`: 503 with
   `code` `runtime_not_installed` / `runtime_not_selected` / `runtime_start_failed` → banner with the message and a link to
   Settings → Runtime. Drop UI that only made sense in-process.
4. **Embedding settings**: `local` = a GGUF embedding model from Local models.
5. **i18n**: all new strings in every locale file the app has (Vietnamese included), following `docs/web-i18n.md` (old repo).
6. **Build & release**: keep `npm run build` (tsc -b && vite build) green; a GitHub workflow that builds and attaches
   `senclaw-web-dist.tar.gz` to a `v*` release; `vite.config.ts` dev proxy target configurable by env (default the
   daemon's 18788, documented how to point at a test daemon on 28788).
7. Docs: `README.md` + `CLAUDE.md` for this repo (dev, build, the old UI rules that concern web: `LLMSettings.tsx`
   vision patterns must match `senclaw/src/zen_core/vision.rs`, `MAX_IMAGE_EDGE` 1568, `MAX_DOC_BYTES` 32 MB, watch strip,
   dispatch retry, i18n).

## Acceptance

`npm run build` green with no new TypeScript errors; any existing lint/test scripts green; components isolated enough that
a later reviewer can read them (split the runtime screen into small components/hooks, one API module
`src/lib/runtimeApi.ts` or matching existing conventions). Report screens added/changed and any contract gap found.
