# Phase 04 — Desktop (`desktop/`, Flutter)

Owner: one agent. Repo: `/Users/benji/Projects/SenClaw/desktop` (only). Contract: `senclaw/docs/runtime-protocol.md` §5.
The daemon side (phase 01) is being built in parallel against the same contract — code to the contract, not to what
the daemon answers today. Flutter 3.41.6 / Dart SDK ^3.11 are installed.

## Context

`desktop/` is the old `desktop_app/` (Flutter console that supervises the bundled `senclaw` daemon) plus
`desktop/update_desktop/` (the self-update helper). Engines left the daemon: OCR, TTS, Whisper, Laya/Jev now run as
runtimes behind the same REST paths; local LLMs are daemon-managed model files run by llama.cpp (GGUF) or sen-mlx (MLX).

## Build

1. **Runtime screen** (Settings → Runtime), modelled on LM Studio's (reference screenshot described below), backed by §5.1:
   - *Runtime Selections* card: one row per slot from `GET /api/runtimes` `slots[]` (GGUF, MLX, Decision (System One),
     OCR, Speech to text, Text to speech) with a dropdown of `candidates` rendered as `name` + a monospace version chip;
     `PUT /api/runtimes/selections`. Toggle **"Auto-update selected runtime packages"** (`autoUpdate`).
   - *Runtime updates channel* card: help tooltip, **Check for updates** button (`POST /api/runtimes/check-updates`),
     channel dropdown Stable / Beta (`PUT /api/runtimes/settings`).
   - *Engines & Frameworks*: search field, "Compatible only / All" filter, type filter (All types, LLM engines, Decision,
     OCR, Speech to text, Text to speech). Rows from `GET /api/runtimes/catalog` merged with `installed`: icon, name,
     version chip (installed → arrow → latest when `updateAvailable`), description, "<version> – Release notes ›" link
     (system browser), right side: Install / Update button, live progress (`GET /api/runtimes/jobs/:id` polled ~1 s,
     spinner + percent), green "✓ Latest version", "Not published yet" (`available: false`), "Incompatible";
     kebab menu: uninstall a version, install from folder/archive (file picker → `POST /api/runtimes/install-local`),
     view logs (`GET /api/runtimes/:id/logs`), stop running processes.
   - *Running* list: `processes[]` with state, model, port, uptime, launches, Stop.
2. **Local models screen** (Settings → Local models), §5.3: list (name, GGUF/MLX badge, size, vision/embedding chips,
   loaded state, Load/Unload/Delete; "no runtime selected for this format" hint linking to Runtime); **Download** dialog
   (HF repo id → `hf-files` → pick a GGUF file + optional mmproj, or the MLX snapshot → progress with cancel);
   settings (default context length + the shared engine settings: temperature, top_k, top_p, max_new_tokens,
   max_kv_tokens, enable_thinking, …, sent back snake_case untouched).
3. **Runtime-missing state** on OCR, TTS, Whisper and Decision (Laya) settings and on voice features (audio_service,
   voice_chat_overlay): a 503 with `code` `runtime_not_installed` / `runtime_not_selected` / `runtime_start_failed` shows
   a banner with the message and an "Open Runtime settings" action instead of a generic error. The rest of those screens
   keeps working unchanged (same REST paths). Drop UI that only made sense in-process (e.g. "not compiled into this build").
4. **Embedding settings**: the `local` provider now means a GGUF embedding model from Local models.
5. **Daemon supervisor & packaging**: no `senclaw-media` sidecar, no `mlx.metallib`. `update_desktop` `swap_bundle`
   requires only the daemon binary (keep its sha/atomic-swap logic). Add a `Makefile` with the old desktop targets
   (`app-dev app-build app-install signing-cert app-build-windows app-build-linux app-build-web app-clean-cache`) building
   the daemon from `../senclaw` and the web UI from `../web-app` exactly as the old targets/CI bundled them (read the old
   `Makefile` and `.github/workflows/desktop.yml` in `/Users/benji/Projects/SemaClaw`), plus optional bundling of runtime
   packages found in `../sen-*/dist/` into the app's `runtimes/` resource dir (the daemon scans `<exe_dir>/runtimes`).
   Copy `scripts/macos_sign_app.sh` and `scripts/macos_make_signing_cert.sh` from the old repo. Port the desktop CI
   workflow (checkout senclaw and web-app as siblings).
6. **i18n**: every new string in English and Vietnamese (`lib/core/i18n`), following the existing pattern and
   `docs/desktop-i18n.md` of the old repo. Vietnamese reference from LM Studio: "Tự động cập nhật các Gói Mở Rộng Thời Gian
   Chạy đã chọn", channel "Ổn định".
7. Docs: `README.md` + `CLAUDE.md` for this repo (build, run against a dev daemon, the UI rules from the old CLAUDE.md that
   concern the desktop — image edge cap `kMaxImageEdge`, 32 MB docs cap, watch strip, dispatch retry cards, i18n).

## Reference: LM Studio "Runtime" screen

Title "Runtime" · section "Runtime Selections" (rows "GGUF → Metal llama.cpp v2.13.0", "MLX → LM Studio MLX v1.8.5", auto-update
toggle) · card "Runtime updates channel ⓘ  [⟳ Check for updates] [Ổn định ▾]" · section "Engines & Frameworks" with
[Search…] [Compatible only ▾] [All types ▾] · rows: "⌁ LM Studio MLX  v1.8.5 → v1.11.0 / Apple MLX engine… / 1.11.0 -
Release notes ›  [◌ 100%] ⋮" and "Harmony (Mac) v0.3.5 / Chat history renderer… [✓ Latest version] ⋮". Dark, rounded cards.

## Acceptance

`flutter analyze` — no errors (no new warnings in touched files); `flutter test` green (add widget tests for the runtime row
states and the runtime-missing banner, with a fake API client); `cargo test` in `update_desktop/` green; a manual run against
a daemon on port 28788 is optional (the lead does the end-to-end). Report screens added/changed and any contract gap found.
