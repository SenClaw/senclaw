# Research: Headroom → context compression for SenClaw

2026-10-23. Goal: fewer tokens per chat, setting to toggle, default ON.
Shipped: `src/context_compress/`, `contextCompression` in `/api/agent-behavior`, rows in web + desktop Settings → Agent behavior. Guide: `docs/context-compression.md`.

## 1. What Headroom is

"Context optimization layer" for LLM apps/agents. Apache-2.0. Repo moved `chopratejas/headroom` → `headroomlabs-ai/headroom`. Python primary (`headroom-ai`, PyPI, py ≥3.10), TS SDK (npm, library only), Rust workspace (`crates/headroom-core`, `-proxy`, `-py`, `-parity`; ONNX runtime via `ort-sys` for the ML parts). Compression runs locally; prompts are not sent elsewhere to be compressed.

Pipeline (docs + crate listing): `CacheAligner → ContentRouter → {SmartCrusher | CodeCompressor | Kompress | Log/Search/Diff compressors}`, "live zone" only.

| Part | What it does | Claimed |
|---|---|---|
| ContentRouter | detects content type, picks compressor | — |
| SmartCrusher | JSON arrays: keep first N (shape), last N (recency), error items, rare-field / rare-status / outlier rows; `max_items_after_crush` 15, `min_tokens_to_crush` 200, `first_fraction` .3, `last_fraction` .15, `variance_threshold` 2.0, dedup identical | JSON 60–95 % |
| LogCompressor | classify lines ERROR/WARN/…; keep first+last error, ≤10 errors with 3 lines context, ≤3 stack traces (≤20 lines), ≤5 deduped warnings, summary lines; `max_total_lines` 100, input < 50 lines untouched, store original only if ratio < 0.5 | 85–95 % |
| Search / Diff compressors | grep-style and unified-diff aware | 80–95 % / 40–80 % |
| CodeCompressor | tree-sitter AST, 9 languages | — |
| Kompress-v2-base | HF model trained on agent traces, for prose | — |
| CCR (reversible) | original cached locally (TTL 1800 s default); marker `[1000 items compressed to 20. Retrieve more: hash=abc123]`; model calls `headroom_retrieve(hash)` | — |
| CacheAligner / live zone | flags volatile prefix content, never rewrites; only new content compressed, frozen prefix byte-identical; history never dropped | keeps provider cache hits |
| RollingWindow / IntelligentContext | history dropping | **retired** upstream |
| Modes | `compress()` library; `headroom proxy --port 8787` (Anthropic `/v1/messages`, OpenAI chat + responses); `headroom wrap <agent>`; MCP server (`headroom_compress/retrieve/stats`); LiteLLM / LangChain / ASGI adapters | — |
| Output shaping | verbosity steering, effort routing (off by default) | est. −31.7 % output tokens |

Project's own numbers (seeded, N=100, **vendor-reported; I found no independent benchmark**): code search 17,199→13,597 tokens (−21 %), SRE incident 55,957→24,340 (−57 %), codebase exploration 58,801→33,895 (−42 %), issue triage 46,067→32,429 (−30 %). GSM8K 0.870 = baseline; TruthfulQA 0.53→0.56 (inside CI); SQuAD v2 97 % @ −19 %; BFCL 97 % @ −32 %. Headline "20 % for coding agents, 60–95 % for JSON". Honest reading: big wins need repetitive structured output; prose and dense output compress little.

Sources: [PyPI headroom-ai](https://pypi.org/project/headroom-ai/), [GitHub](https://github.com/headroomlabs-ai/headroom) (README, `crates/headroom-core/src/transforms/{log_compressor,smart_crusher/*,safety}.rs`), [docs llms-full.txt](https://docs.headroomlabs.ai/llms-full.txt).

## 2. Where SenClaw's tokens go

Read from the code, not measured on a live chat (no live chat was driven for this).

1. System prompt + tool schemas (the browser notes in CLAUDE.md measure ~100 k chars + 49 tools on a real install). **Not touched by this work.**
2. History, re-sent on every call. Within it the part that grows fastest is tool results.
3. Already present: `auto_compact` (LLM summary at 80 % of the window), L1 offload (`substituteToolOutput`: head+tail preview, **off by default**, cuts the middle blind), Bash 30 k-char head cut with no way back.
4. **Waste found on the MCP path:** `McpBridgeTool::call` and `McpRegistryBridgeTool::call` do `serde_json::to_string_pretty(<whole JSON-RPC result>)` → `{"content":[{"type":"text","text":"<pretty JSON, quotes escaped>"}],"isError":false}`. Envelope + indentation + `\"` escaping on every Space App / built-in MCP result. No information, paid per call.

Constraint that shapes everything: a local engine (sen-mlx) reuses its KV cache only on an **exact prefix**; hosted providers cache prefixes too (CLAUDE.md "`<agent_status>`: measured 30 s vs 1.3 s per step"). Anything that rewrites history per call is out.

## 3. Options

| Option | Verdict |
|---|---|
| A. Run Headroom proxy (Python) in front of providers | **No.** Daemon is one Rust binary + optional runtimes; Python ≥3.10 + ML extras on every install; proxy sits between daemon and `local:<key>` loopback models, breaking usage accounting and the per-call clone discipline (`outgoing_messages`); proxy telemetry beacon must be disabled; cache-mode proxy can't see SenClaw's turn context layout. |
| B. Link `headroom-core` (Rust) | **No.** Pulls ONNX runtime (`ort-sys`) + embedding model for a default-on feature; crate not published as a stable library; ~100 KB per compressor of code to vendor and keep in sync. |
| C. MCP server `headroom_*` | **No.** Costs the model a tool call to compress what we can compress for free at the seam. |
| **D. Port the ideas natively** | **Chosen.** ~900 lines Rust, no model, no process, sub-ms, deterministic. |

## 4. What was ported, what was not

Ported (with the reason each is safe for an agent):

- **Content routing by shape** → `compress()`: MCP envelope / JSON / log. Allow-list of tools (`eligible`).
- **SmartCrusher selection** → `array::select`: head 5, tail 3, ≤7 unusual rows, spread to 15. Unusual = trouble words (Headroom's list + `lỗi`, `thất bại`), rare field (<20 % of rows), rare categorical value (top set ≤5 values covering ≥80 %), numeric outlier (3σ; Headroom uses 2.0 for change points). Deviation: a trouble-named *key* counts only with a real value (`"error": null` on every healthy row would flag every row — Headroom substring-matches the lowercased row).
- **LogCompressor** → `log::shrink`: first/last lines, failures kept first-2-per-kind + last (kind = numbers blanked), 2 lines context + indented stack, deduped warnings, last summaries, zero counts ("0 failed") are not failures. Output must be ≤50 % of input (same threshold as Headroom's CCR gate).
- **CCR** → artifact file + footer. Headroom: hash + `headroom_retrieve` tool. SenClaw: path + the `Read`/`Grep` the model already has; the file holds **one row per line** so `Grep` returns a row, not the document. Reuses `Workspace` (0700/0600 files, 50 MB/chat quota, same store as L1).
- **Live-zone / byte-identical prefix** → free by construction: compress where the result is created, history keeps the shrunk text.
- **"Never drop history"** → same.

Not ported, deliberately:

- **CodeCompressor, Kompress (ML), Diff/Search compressors.** `Read` output is what an `Edit` is matched against; a lossy file view breaks edits. ML adds a model download to a default-on path. `Grep`/`Glob` are already capped at 100 rows.
- **Relevance scoring of rows against the user's question** (Headroom's BM25/embedding anchors): the seam has no question; retrieval via the footer covers it.
- **Output shaping / effort routing.** Different feature (output tokens), changes model behaviour; not asked.
- **RollingWindow.** Upstream retired it; SenClaw has `auto_compact`.
- **Dedup of identical rows, change-point detection, TOON/CSV tabular rendering.** Possible later gains; not needed for the first cut.

## 5. Design as built

```
tool.call → result_for_assistant ──► context_compress::apply(tool, call id, chat dir, text)
                                       ├ not eligible / < 800 B / setting off ► unchanged
                                       ├ tier 1: unwrap MCP envelope + compact JSON      (same data)
                                       └ tier 2: rows/lines left out + full text saved + footer
                                  ──► ContentBlock::ToolResult  (history keeps this text)
UI card = tool `data` (full, unchanged)
```

Safety rules (each has a test): isError / image / extra-field MCP results untouched; Bash never JSON-reformatted; ids above 2⁵³ preserved (rows copied by byte range, never re-printed from `Value`); save failure ⇒ no cut; output deterministic; a cut must reach ≤50 % (array) / ≤70 % (document) or is skipped; small arrays (<20 rows) never cut.

Setting: `contextCompression` (default true) in `config.json` via `GET/POST /api/agent-behavior`, beside `memoryRecall`. Read per tool result after cheap checks, so a typical small result never opens `config.json`; takes effect on the next result, no restart. Clients: web `AgentBehaviorSettings.tsx` ("Context" section), desktop `settings_screen.dart` + Vietnamese string.

## 6. Measured (synthetic fixtures, bytes)

`cargo test --lib context_compress::tests::measured -- --nocapture`. Fixtures are the shapes MCP tools/commands return; they are **best-case** (regular rows, one failure), not a sample of real traffic. Token counts not measured (no tokenizer available offline); tier 2 removes content so tokens fall about as bytes do; tier 1 removes mostly whitespace/escapes, which BPE already merges, so its **token** saving is smaller than its byte saving.

| fixture (what produced it) | before | after | saved |
|---|---:|---:|---:|
| 150 emails, MCP result, array nested in an object (tier 2) | 86,021 | 6,226 | 92.8 % |
| 400 kanban cards, MCP result, bare array (tier 2) | 153,976 | 4,562 | 97.0 % |
| 12 emails, MCP result, under the 20-row cut limit (tier 1 only) | 7,018 | 4,630 | 34.0 % |
| 3,000-line service log, Bash (tier 2) | 160,884 | 2,161 | 98.7 % |
| 900-test `cargo test` output with 1 failure, Bash (tier 2) | 28,364 | 968 | 96.6 % |

"After" includes the ~170-byte footer. Headroom's own seeded runs on real agent tasks were 21–57 %; expect the real number nearer theirs than these.

## 7. Verification

- 3,509 lib tests green (full suite, 0 failed), incl. 49 new: byte-exactness of ids, nested arrays, trouble/rare/outlier rows survive, determinism, setting on/off at the real `run_tools` seam, save failure ⇒ no cut, the `/api/agent-behavior` handlers with the wire names a client sends (`agent_behavior_config::tests`; an HTTP-level test could not be built — see below).
- `tsc` (web), `dart analyze` (desktop) clean.
- **Not done:** the daemon binary was not built or started (the disk had 1–1.7 GB free and a build hit ENOSPC once), so nothing was exercised over real HTTP or in a live chat; no real-model chat driven with the feature on; no task-success eval (`scripts/evals/run.py`) with vs without; not installed on the live daemon.

## 8. Risks and trade-offs

1. **Aggregate questions on a cut list** ("how many are unread?"). The model sees 15 rows + "kept 15 of 150" + a footer telling it to Read the file. A strong model reads the file; a small local model may answer from 15 rows. This is the main default-ON risk; the switch is the escape. Footer wording is imperative to help.
2. **Browser snapshot tools** return element lists the model clicks by index → `__browser_` tools are excluded.
3. Thresholds are guesses informed by Headroom's defaults, not tuned on SenClaw traffic.
4. Artifacts accumulate per chat (quota 50 MB, oldest pruned) and hold tool output on disk (0600), like L1 and trajectories.

## Unresolved questions

1. Run the eval harness (`scripts/evals/run.py`) with the switch on/off on gemma-4-e4b + a hosted model before the next release? Needs a model; I did not.
2. Show savings in the Usage page (counter exists only as an `info` log line `context_compress`)?
3. Should `Task` / sub-agent final answers (prose) be eligible? Left out: nothing to crush structurally.
4. Interaction with `auto_compact`: compressed history reaches the 80 % trigger later — wanted, but unmeasured.
