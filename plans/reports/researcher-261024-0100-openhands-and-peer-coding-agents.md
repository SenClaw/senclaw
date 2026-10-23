# OpenHands + peer coding agents — what SenClaw's code feature can take

Date 2026-10-24. Digest of a subagent's web/source research (143 tool calls). Evidence: [S] source read,
[D] official docs, [B] vendor blog numbers (not replicated). Cursor/Windsurf/Devin internals are closed — blog claims only.

## Executive summary

OpenHands V1 (`OpenHands/software-agent-sdk`; the old Python app is gone from `OpenHands/OpenHands`, `openhands-aci` archived 2026-04)
is strong on a typed event loop, a robust condenser and shell-risk analyzers. It is weak where SenClaw wants to be strong:

- **Approval is coarse**: AlwaysConfirm / NeverConfirm / ConfirmRisky on model-self-reported LOW/MED/HIGH. "Always" = session-global
  NeverConfirm; no per-command rules, nothing persisted; headless is always-approve. Its answer to "few questions" is
  "sandbox + NeverConfirm" — the weakest policy design in the set.
- No repo map/index (grep + instruction files). V1 editor lost lint-after-edit. Edit miss gives no nearest-match hint.
- Targets 32B+ models with 22–32k context; not e4b-class. Its prompt even says "consult the user first" before installs/tests.

Better ideas for "few questions, safely, locally" come from peers: OS sandbox as the approval boundary (Codex, Cursor −40 % stops [B]);
layered decision order (rules → read-only/in-workspace auto-allow → classifier that never sees tool results: Claude Code auto mode);
unit-tested prefix rules amended from approvals (Codex execpolicy); tiered fuzzy edit match + introduced-only lint diff (Codex,
Gemini, OpenCode, Aider, SWE-agent); tool output spilled to files + observation masking.

## OpenHands mechanisms worth knowing

| Area | Mechanism | Source |
|---|---|---|
| Loop | `LocalConversation.run()`: stop hooks can deny finishing and inject feedback; stuck check; iteration cap 500; dollar cap | `conversation/impl/local_conversation.py` |
| Tool-call tolerance | `normalize_tool_call`: aliases (bash/execute_bash→terminal), recover malformed names, infer editor `command` from args, `[Tool-argument error]` hint with a heredoc example; XML `<function=…>` fallback for non-native models | `agent/utils.py`, `llm/mixins/fn_call_converter.py` |
| Risk | `SecurityRisk` LOW/MED/HIGH/UNKNOWN; Pattern analyzer (regex), PolicyRail analyzer (tree-sitter shell AST: fetch-to-exec, raw-disk, catastrophic-delete), Ensemble = max severity, analyzer error = HIGH; model's own label only escalates | `security/*` |
| Confirmation | status WAITING_FOR_CONFIRMATION; approve = `run()` again; reject = `UserRejectObservation`; CLI offers Yes/No/Always/"Auto LOW/MED" | `OpenHands-CLI` |
| Sandbox | Docker (default), Process ("unsafe, but fast"), Remote. Local mode keeps tools/events/condenser, loses isolation | docs |
| Repo understanding | none besides grep/glob/terminal + instruction files; skills with keyword / task / **path triggers** (rule injected into the tool observation when a matching file is touched); `AGENTS.md`/`CLAUDE.md`/`.cursorrules` auto-loaded, symlinks deduped, nested ones become path-scoped | `skills/*`, `context/agent_context.py` |
| Context | `LLMSummarizingCondenser` (max 80 events, keep first 4, HARD/SOFT triggers, atomic action+observation units, summary template with CODE_STATE/TESTS/CHANGES); V0 had cheap non-LLM tiers (observation masking, amortized forgetting) that V1 dropped | `context/condenser/*` |
| Stuck | last 20 events: same action+obs ×4, same action+error ×3 (one nudge), monologue ×3, A/B alternation ×6, context-window loop | `conversation/stuck_detector.py` |
| Edit | literal match, retry with `old.strip()`; miss → "did not appear verbatim" (no hint); multi-match lists line numbers; ±4-line snippet on success; undo history; atomic write | `file_editor/editor.py` |
| Verify | benchmark prompt: write a repro first, minimal fix, run tests for touched files; critics (empty patch / not finished); goal judge: "claimed-but-unverified is NOT satisfied" | `benchmarks`, `critic/*` |

## Comparison (approval / edit / context / small-model fit)

| Agent | Approval | Edit | Notable | Small-model fit |
|---|---|---|---|---|
| OpenHands V1 | Always/Never/ConfirmRisky + analyzers | str_replace exact | skills with path triggers, condenser | medium |
| Aider | auto-commit + /undo | SEARCH/REPLACE, whole, udiff per model | PageRank repo map; "Did you mean these actual lines?" on a miss; lint+test reflections ×3 | high |
| Claude Code | rules deny>ask>allow, modes, classifier (auto) | exact, read-before-edit | broad allow rules dropped in auto; classifier sees no tool results | low |
| Codex CLI | approval policy + OS sandbox + execpolicy prefix rules + guardian | apply_patch with fuzzy seek | rules carry match/not_match examples validated at load; approvals amend rules | medium |
| Gemini CLI | TOML policy engine, modes | exact→flexible→regex→LLM fixer | loop detection; tool-output masking with files | medium |
| OpenCode | allow/ask/deny patterns, last match wins | exact + whitespace/quote normalization | `doom_loop` permission | medium |
| Goose | Autonomous/Approve/SmartApprove | str_replace | inspector pipeline; tool-pair compaction | medium |
| SWE-agent | none | linter-guarded edit (reverted on syntax error: +3.0 pts [B]) | recovery drops 90.5 %→57.2 % after one failed edit → prevent, don't retry | mini: any model |
| Cursor | sandbox + classifier | fast-apply | sandboxed agents stop 40 % less [B]; shell result names the blocking constraint | closed |

## Ranked transferable ideas (and where SenClaw stands)

| # | Idea | SenClaw today | This branch |
|---|---|---|---|
| 1 | Sandbox as the approval boundary; denial-aware results | sandbox exec exists (Plugins → Sandbox), not tied to approvals | follow-up |
| 2 | Prefix-rule engine, tested rules, compound splitting, amendments | saved prefixes + `tool_rules`; no splitting-aware grading | **graded `auto` covers the common cases**; rule tests follow-up |
| 3 | Deterministic risk floor (+ optional classifier) | Jev gate per Bash command | **`util::command_risk`** (Low/Medium/High, fail-closed) |
| 4 | Tiered edit match + self-correcting miss message | fuzzy tiers only if `editFormat=fuzzy`; miss has no hint | **nearest-lines hint** |
| 5 | Introduced-only diagnostics after edit | LSP diagnostics appended | **tree-sitter syntax check, introduced errors only** |
| 6 | Small-model tool-call tolerance | unknown | follow-up (measure with failure ledger) |
| 7 | Tiered context (spill → mask → summarize) | tool-result compression + compaction | follow-up: observation masking |
| 8 | Loop ladder | duplicate interception, stall, error nudge, turn cap | exists |
| 9 | Path-triggered / nested `AGENTS.md` | project doc never injected (bug) | **fixed + `AGENTS.md` + detected commands**; nested rules follow-up |
| 10 | Evidence-based finish gate | none | **verify-before-finish nudge** |
| 11 | Retrieval subagent returning file+line ranges | `Task` + repo map tools | follow-up |
| 12 | Per-model capability profiles | `EditFormat` per model | exists (partly) |

Also: drop any prompt text that makes the agent ask for permission it does not need (OpenHands' "consult the user first").

## Unresolved

- Edit-failure rate of e4b-class models on SenClaw's Edit is unmeasured; it sizes the value of an LLM edit-fixer tier.
- A local small model as risk *classifier* has no public data; measure before shipping one. Never let a classifier override a deterministic ask.
- Sandbox tuning for build/test on macOS (package caches, localhost ports) is the practical risk of idea 1.
- Vendor numbers (40 %, 46.9 %, 17 % FNR, 3×) are unreplicated.
