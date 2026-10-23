# Context compression

A tool result enters a chat's history once and is re-sent with **every** later
call of the conversation. A 60 KB list an app returned on turn 3 is paid for
again on turns 4 to 40, and it is what fills a local model's context first.
Context compression shrinks such a result **once, where it is created**
(`zen_core::run_tools`), and the history keeps the shrunk text.

On by default. **Settings → Agent behavior → Context compression** (web and
desktop) turns it off; the daemon reads the switch on the next tool result, no
restart. Stored as `contextCompression` in `config.json`, served at
`GET`/`POST /api/agent-behavior` next to `memoryRecall`.

Research behind the design (Headroom, what was ported and what was not, measured
sizes): [`plans/reports/research-261023-1209-headroom-context-compression.md`](../plans/reports/research-261023-1209-headroom-context-compression.md).
Code: [`src/context_compress/`](../src/context_compress/mod.rs).

## What it does

Two tiers. Only the first is applied without leaving a way back.

| Tier | Input | Result |
|---|---|---|
| **1 — same data, fewer bytes** | An MCP tool result. The bridge prints the whole JSON-RPC result pretty, around a text block that is itself JSON: envelope, indentation and string escaping carry no meaning. | `{"content":[{"type":"text","text":…}]}` unwrapped, JSON printed compactly. Strings and numbers are copied byte for byte (an id above 2⁵³ stays an id). |
| **2 — rows left out, original saved** | A JSON array of ≥ 20 rows in an MCP / `WebFetch` result, or a long build / test / service log from `Bash` / `PeekBgJob`. | Array: the first 5 rows, the last 3, up to 7 unusual rows, and an even spread to 15; a `"[… N items omitted …]"` element stands for each run left out. Log: first 8 and last 12 lines, failures (the first two of each kind — numbers blanked — and the last, at most 30) with 2 lines of context and the indented stack under them, the last summary lines, the first of each distinct warning; `[… N lines omitted …]` between. |

Tier 2 saves the full text under the chat's workspace
(`~/.senclaw/control-plane/workspace/<chat>/artifacts/<call id>-full`, one array
row per line so `Grep` finds one row) and ends the result with

```
[context-compress: kept 15 of 150 items. If the task needs the others, Read or Grep <path> (the full 84 KB) before answering.]
```

The model reads the rest with the `Read` / `Grep` it already has. The card the
user sees in the chat is the tool's full `data`, not this text.

**Unusual rows** are chosen from the rows themselves, with no model: a row that
names a failure (`error`, `exception`, `fail…`, `timeout`, `denied`, `lỗi`, …
at the start of a word; an `"error": null` on every row does not count), a
value that is rare in a field where the others agree, a field only a few rows
have, a number more than 3σ from its column.

## What it never touches

- **`Read`, `Edit`, `Write`, `NotebookEdit`, `Grep`, `Glob`, `Skill`, `Task`,
  `ToolSearch` …** Only an allow-list is processed (`eligible`: `mcp__*`,
  `WebFetch`, `Bash`, `PeekBgJob`). An unknown tool is skipped: skipping costs
  tokens, a wrong guess costs an edit that no longer matches.
- **Browser tools (`…__browser_*`).** A snapshot lists the elements the model
  clicks next by index; leaving some out would leave them unclickable.
- **Bash is never reformatted as JSON** — `cat package.json` then an `Edit` quoting
  it must still match. Bash gets only the log shrinker, and only for text that
  reads like a log (levels / timestamps / test verdicts on ≥ 30 % of lines).
- An `isError` MCP result, or one carrying an image or any field besides
  `content` / `isError`.
- Anything under 800 bytes, and anything that would not shrink by a clear margin
  (tier 1: ≥ 200 bytes and ≥ 8 %; an array: to ≤ 50 %, the document to ≤ 70 %).
- **If the full text cannot be written, nothing is left out**: a JSON result
  falls back to tier 1, a log is returned whole.

## Why it is safe for the prompt cache

The text is produced once and stored in the history, so every later request
repeats the same bytes: a hosted provider's prompt cache and a local engine's
exact-prefix KV reuse (sen-mlx) both keep hitting. The result is a pure function
of the tool output — no clock, no randomness — and a test pins that.

## Rules for Claude

- **Compress at the creation seam, never on the way out.** Rewriting history per
  call changes the prefix and defeats both caches. `apply` runs once, in
  `run_tools`, before the result becomes a `ContentBlock::ToolResult`.
- **Copy rows by byte range; never print a parsed `Value`.** `serde_json` turns
  `12345678901234567890123` into a float. `json::minify` and the array scanner
  work on the text.
- **Add a tool to `eligible` only if nothing is edited against its output.**
- **A cut without a saved file is a bug.** `apply` falls back to tier 1 or the
  original when `save_artifact` fails; keep it that way.
- The saved file is named `<call id>-full`, not the bare call id: L1 offload
  (`controlPlane.workspace.substituteToolOutput`, off by default) names its file
  by the call id and would overwrite it. Both can be on; compression runs first.
- Thresholds are constants in `context_compress` (`MIN_BYTES`, `array::MIN_ITEMS`,
  `ARRAY_SHARE` …). Tune them against recorded trajectories, not by feel.
