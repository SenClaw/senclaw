# E2E integration — runtime split (lead)

Isolated daemon: `HOME=<scratchpad>/e2e/home`, ports 28788/28789, runtimes on ephemeral ports. Real models reached via
APFS clones (`cp -c`) of `~/.senclaw` checkpoints — zero extra disk, originals never written.

## Results

| Area | Check | Result |
|---|---|---|
| Empty state | `GET /api/runtimes` 6 slots empty; `/api/{ocr,tts,whisper,decision}/models` | ✓ 503 `runtime_not_installed` + `slot` |
| Install | `install-local` of all 5 `dist/*.tar.gz` (flat and wrapped layouts) | ✓ `source: local`, slots auto-selected |
| OCR | `POST /api/ocr/recognize` (PNG, 2 lines) | ✓ exact text, conf 0.99/0.99, 2.7 s cold |
| TTS | `/api/tts/synthesize` default + VieNeu v3 | ✓ macOS voice 22 kHz; VieNeu 48 kHz, 1.3 s |
| ASR | `/api/whisper/transcribe` of the TTS clip (vi) | ✓ correct except TTS-mispronounced words |
| Decision | settings merge (gate/skills from daemon), load multilingual, `/api/decision/ask` (vi) | ✓ refund p=1.0, 58 ms, option order preserved |
| Control plane | `/api/decision/gate/check`, `/api/decision/skills/check` | ✓ (response keys sorted = pre-existing `json!` display, request order intact) |
| Idle | service runtimes idled out at ~300 s | ✓ |
| MLX | JIT load `local:` model, streamed chat via `/api/runtimes/models/:key/v1` | ✓ after fix; output buffered by design (tool/think marker parsing) |
| Space Apps | `register-local` (Python SDK demo, venv), MCP autoRegister, stop/start/restart/ready, UI proxy | ✓ MCP connected, launches 1→2→3 |
| Space Apps | `register` from manifest URL, `install-zip` (index.html at root), uninstall | ✓ |
| Crash | `kill -9` daemon with 3 runtimes up | ✓ all runtimes exit ≤ 2.0 s (watchdog); restart resets `running.json` |

## Bugs found and fixed by the lead during E2E

1. **Health gate killed every model-mode runtime** — `supervisor::wait_healthy` treated 503 ("loading") as failure;
   now only 500 is terminal. Tests: waits through 503, fails fast on 500.
2. **Whisper snapshot listed as a chat LLM** — `local_models::scan` now skips speech checkpoints (`model_type: whisper`,
   `*ForCTC`, `*ForSpeechSeq2Seq`).
3. **MLX keys/names cut at the second dot** (`Qwen2.5-0.5B` → `qwen2-5-0`) — `model_key` uses the directory name for
   snapshots, `file_stem` only for `.gguf`.
4. **`SENCLAW_LOCAL_MODELS_DIR` ignored by the model picker** — `local_models::register_root/root_for`, keyed by config
   file, registered at boot.
5. **llama.cpp uninstallable** — extractor refused every symlink; now allows links that resolve inside the package,
   refuses escaping/absolute links and hard links.
6. Checksum files of sen-mlx/sen-whisper were bare hashes — now `shasum -c` format (protocol §2.3 pinned).

## Final pass (after conformance fixes + control plane)

| Area | Check | Result |
|---|---|---|
| Catalog | `llama.cpp-metal` `available`, release notes URL, remote-index fetch error reported | ✓ |
| llama.cpp | install from upstream b11201 (202 → done), generated manifest, dylib symlinks kept | ✓ |
| GGUF | HF download (Qwen2.5-0.5B q4_0, nomic-embed); retry after a network drop | ✓ (restarts from 0 — no resume) |
| GGUF chat | load with no body, streamed chat via `local:` route | ✓ 11 chunks, first token 0.15 s, correct vi answer |
| GGUF embed | `/v1/embeddings` | ✓ 768-d, sim 0.80 vs 0.42 as expected; launched with `--embedding`, `-c 2048` |
| Agent turn | ACP → WS gateway → AgentPool → `local:` GGUF (`--gateway ws://127.0.0.1:28789`) | ✓ "PONG" in 9.7 s |
| Control plane | specs listed (shadow/off modes); trace written | ✓ neutral-v1, metadata only, 0600 |
| agent_status | present at the tail of the outgoing request | ✓ |
| Crash | `kill -9` with llama-server loaded → next boot stops the orphan | ✓ |

More bugs found and fixed by the lead: launch-context default 8192 could not hold an agent turn (opening prompt
measured at 15.7K tokens) → default 32768, capped by the model maximum; generated llama.cpp manifest now takes its
name/accelerator from the index ("Metal llama.cpp").

## After the security review (`code-reviewer-260927-1127-*`, fixes by two agents)

Live on the isolated daemon, rebuilt with every fix:

| Area | Check | Result |
|---|---|---|
| Trace read | `GET /api/traces/..:..%2fconfig`, `..%3A..%2Fconfig`, `a%2Fb:c` / a listed id | ✓ 404 ×3 / 200 |
| Runtime logs | `?key=model:../../../../etc/x`, id `..%2F..%2Fx` / plain `sen-ocr` | ✓ 400 ×2 / 200 |
| Crash, model | `kill -9` loaded llama-server → next chat request | ✓ respawned, `launches` 1→2, 200 |
| Crash, service | `kill -9` sen-ocr → next `/api/ocr/models` | ✓ 200 in 0.29 s, new pid, `launches` 2 |
| Stream vs idle sweep | model idle timeout 3 s, 17.2 s stream (5002 chunks) | ✓ same pid throughout; stopped 4 s after the stream |
| Vision | SmolVLM-256M Q8_0 + `mmproj` download, pairing, chat with a PNG | ✓ `capabilities: [chat, vision]`, `--mmproj` + `-c 8192` (model max), text read correctly, 1.1 s |

Bug found and fixed: with a `local:` model as the active LLM, the boot SOUL.md ingest ran ~13 ms before the UI
server bound, so every section's extraction failed ("send chat request") on every boot — a chunk without edges is
retried only on the next boot, which failed the same way. `run_daemon` now holds the ingest until the HTTP listener
is bound (`watch` channel). Live: before 0 entities/0 edges, after 20/32; two sections still hit the 60 s cognify
timeout because the 0.5B test model runs away without `max_tokens` (llama-server cancels on disconnect — model
quality, not wiring).

Also: review-finding labels the fixing agents wrote into comments were removed (34 places), and three comments that
described the pre-split engines or "this round" were rewritten to the current behavior. `cargo tree`: no
mlx/candle/ort/ocr-rs/MNN/whisper/llama crate in the daemon.

## Incident — one test turn reached the user's real daemon

`senclaw acp` takes its target from `--gateway` (default `ws://127.0.0.1:18789`) and ignores
`SENCLAW_WS_PORT`. The first ACP run (~11:13 local) therefore connected to the user's real daemon: it registered
one chat group `acp:acp-work:729a2246` (group type `code`, allowed dir = the session scratch folder) and sent one
message ("Reply with exactly one word: PONG"), answered by the user's configured model; no tools ran, nothing else
was modified. Later runs pinned `--gateway`. At ~11:30 the real daemon and the desktop app were no longer running
(not stopped by this session). At 11:34:51 a debug build of the new daemon (`target/debug/senclaw`, pid 81302) was
started from an interactive shell in the user's Antigravity IDE on 18788/18789 with the real `HOME` — it has since
launched `sen-sysone` from `~/.senclaw/runtimes/`. Not started or touched by this session; it predates the security
fixes. Offered to the user: remove that chat group from the real data.

## Open

- Downloads are not resumable (a retry restarts from 0).
- The existing llama.cpp package keeps its old display name until reinstalled.
- Cognify sends no `max_tokens`; a very small local model can run to the 60 s timeout. Adding one needs care:
  OpenAI reasoning models reject `max_tokens` (they take `max_completion_tokens`).
- Carried over from the old repo: `query_llm.rs` still names `local-candle-native` / `local-mlx` adapters and
  says "the in-process path always wins", but no in-process engine exists; such a provider falls through to the
  OpenAI adapter. Harmless; clean up when that file is next touched.

## Final state

Daemon `cargo test --workspace` 2667 passed / 0 failed / 13 ignored; SDKs 18 + 24 passed. Runtimes, desktop
and web-app as in their reports. All acceptance criteria in `plan.md` met.

Status: DONE
Summary: Every acceptance criterion verified on an isolated daemon, including the review fixes live; one boot-order
bug (local LLM before the HTTP server) found and fixed.
Concerns: one ACP test turn reached the user's real daemon (above); the user's debug daemon on 18788 predates the
security fixes and should be restarted from the current build.
Unresolved: whether to delete the `acp:acp-work:729a2246` chat group from the real data (user's call).
