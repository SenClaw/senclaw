# Phase 07 — Integration, end-to-end, review (lead)

Isolated home (`HOME=<scratch>/home`), daemon on 28788/28789, runtimes on ephemeral ports. Never the real `~/.senclaw`.

1. Build daemon; `make package` each runtime; `POST /api/runtimes/install-local` all five; select slots.
2. Legacy paths through the proxy: `/api/ocr/recognize`, `/api/tts/synthesize`, `/api/whisper/transcribe`,
   `/api/decision/ask` + `/api/decision/gate/check` + `/api/decision/skills/check`; runtime-missing 503 body before install.
3. MLX: copy a small MLX model into the isolated home, load via `/api/local-models/:key/load`, stream a chat through the
   `local:` provider; idle stop; crash → restart.
4. GGUF: install upstream `llama.cpp-metal` (stable), download a small GGUF chat model + a GGUF embedding model, chat +
   `/v1/embeddings`, memory `local` embeddings. Test models (checked on HF 2026-09-27):
   `Qwen/Qwen2.5-0.5B-Instruct-GGUF` `qwen2.5-0.5b-instruct-q4_0.gguf` (429 MB, chat),
   `nomic-ai/nomic-embed-text-v1.5-GGUF` `nomic-embed-text-v1.5.Q4_K_M.gguf` (84 MB, embedding),
   `ggml-org/SmolVLM-256M-Instruct-GGUF` `SmolVLM-256M-Instruct-Q8_0.gguf` + `mmproj-SmolVLM-256M-Instruct-Q8_0.gguf` (vision + mmproj pairing).
5. Space Apps: `register-local` (SDK example / `senclaw create app`), `register` (manifest URL served locally),
   `install-zip`, MCP + LLM autoRegister, start/stop/restart, uninstall.
6. Clients: `flutter analyze` + `flutter test` (desktop), `npm run build` (web-app); screens against the test daemon.
7. Orphans: kill -9 the daemon → runtimes exit (watchdog); restart → `running.json` cleanup.
8. Code review (reviewer agent) of all repos; fix findings; final report + docs pass.
