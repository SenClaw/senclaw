# Phase 06 — Control plane P0–P1 (`senclaw/src/control_plane/`)

Owner: one agent, after phase 01 lands. Repo: `/Users/benji/Projects/SenClaw/senclaw` (only).
Source of truth: `/Users/benji/Projects/SenClaw/JEV Architecture v2.html` (v2.2) §3–§9, §12–§13, §15–§17 — extract its text
(strip tags) and read it fully. Prior research in the old repo (read-only): `plans/reports/research-260925-0215-jev-senclaw-integration-design.md`,
`plans/reports/research-260926-1254-jev-tool-discovery.md`, `plans/reports/research-260926-1500-ai-agents-in-depth-vs-senclaw.md`
(whichever exist). Decisions go to Laya/Jev only through `decision::client` → the `decision` runtime (phase 01).
Start from the seam map `plans/reports/explore-260927-0129-control-plane-seams.md` (scout output — verify every
file:line against the current source; phase 01 has changed some files since).

## Principle

§17 P0: "Agent mode, Policy Gate, Tool Registry + Executor, two-zone Context Assembler, status bar, Workspace, Trace,
minimal Eval Harness run with JEV_OFF". P1: "Jev client, file Registry, Router/guard/done specs in **shadow**, prefix
regression (G1)". Build the layer and wire it at the existing seams; **default behavior only changes where §4 says a layer
cannot be switched off** (Policy Gate, per-call permission check, budget, trace) and even there existing tests must stay
green. Everything new that would change an agent's decisions runs in shadow (logged, never acted on) or behind a switch.
Reuse before adding: the engine already has exact-duplicate tool-call interception, `TOOL_ERROR_NUDGE`, the
`tool_error_loop` session error, `auto_compact`, the failure ledger (`src/failures`), trajectories (`src/trajectory`),
`scripts/evals/run.py` + `evals/cases`, the decision gate and skill router. Map each to its §6 component; do not duplicate.

Start with a short design report (component → existing code → change → switch/default) in the reports folder, then build.

## Build

1. **`control_plane` module + settings**: `controlPlane` block in `config.json` (read through the existing config seam) and
   `SENCLAW_JEV_OFF=1` (env wins): JEV off skips every Jev tier (ablation baseline §13).
2. **Decision ladder** `rule → Jev → LLM → human`: `Decision {spec@ver, by, answer, confidence, band: act|fallback|review,
   latency_ms, shadow}`; per-spec thresholds and timeout; `on_uncertain` per spec; every decision recorded in the trace
   (spec, answer, confidence, band — never the state text).
3. **Spec registry (file-based, P1)**: bundled `assets/specs/*.json` compiled in by `build.rs` (walk like patterns) + user
   overrides `~/.senclaw/registry/specs/`; schema per §5/§12 (id, version, type noul|choice|score, question, options/levels,
   state fields, bands, on_uncertain, mode off|shadow|active, lifecycle candidate|shadow|active|decaying|retired|invalid,
   lang); strict validation + tests; REST `GET /api/control-plane/specs`, `PUT /api/control-plane/specs/:id/mode`.
   Bundled specs: `route.skill` and `tool.risk` wrap the existing router and gate **with their current modes and
   behavior**; `input.guard` (3 noul), `clarify.needed`, `task.done`, `loop.next_step` in **shadow**.
4. **Trace neutral-v1** (§16): per turn `{trace_id, lang, format, decisions, llm_calls{tokens_in, tokens_out, cache_read,
   compact}, tool_calls{id, name, fingerprint, error_type, artifact}, verdict, attribution, synthetic_placeholders, outcome}`
   written from the existing per-chat event seam; metadata only (no message/file content), 0600, retention-bounded;
   `GET /api/traces`, `GET /api/traces/:id`.
5. **Workspace** (§6/§9): per session `progress.md` (decisions / constraints / failed_paths), `todo.json` (mirror of the todo
   tool), `artifacts/`, `handoff.md`, with quota and path confinement; failed paths appended from the loop controller;
   compaction always keeps `progress.md`; L1 offload of large tool output to `artifacts/<call_id>` with a head+tail preview
   that says it was cut and where the full file is (§8 "no silent edits").
6. **Context assembler** (§7): Zone A static prefix byte-stable (soul, core tools, static rules, skill catalog, fixed
   few-shot) then `CACHE_BOUNDARY`, then append-only trajectory; move per-turn-varying content after the boundary; log a
   prefix hash per LLM call; test byte stability across turns. `<agent_status>` tail ≤ 300 tokens computed by code (TODO,
   call k/N, budget, elapsed, pending events, original goal; never web content), appended per call, not persisted; switch
   `controlPlane.agentStatus` (default on). Compaction at 80% with `[COMPRESSED]` + breaker (3 failures → escalate) — align
   `auto_compact` rather than adding a second compactor.
7. **Loop controller** (§9): 4-tier error taxonomy, `hash(tool, canonical args)` fingerprints, stuck detection (same
   fingerprint 3× or same error type 2× → notice + no retry_same), per-tool circuit breaker, three budgets (turn / task /
   session) + latency budget with defaults equal to today's effective limits, last error verbatim.
8. **Tool registry metadata** (§8 B7): `when_to_use, not_for, examples, risk_tier, reversible, idempotent,
   concurrency_safe, requires_preview, cancellable` for every built-in tool + a linter test; aliases forward it.
9. **Policy Gate** (code only): move the shell danger list + parsing from `decision/gate` here; unparseable → never
   auto-approved (fail-closed); `risk_tier ≥ 3 && !reversible` never auto-approved by any engine.
10. **Minimal eval harness** (§13): extend `scripts/evals/run.py` + `evals/cases` to the task schema (id, source, lang,
    difficulty, split, initial_state, user_scenario, criteria{env_assertions, decision_assertions, veto}, versions),
    `--k` repeats → Pass@1 / Pass^k, `--jev-off`, cache_hit_ratio and cost from traces; **30–50 handcrafted tasks, at least a
    third in Vietnamese**, realistic and gradeable by environment state; a `--dry-run` that validates every task. G1 prefix
    regression: record decision inputs only when `controlPlane.recordDecisionInputs` is on (default off), replay them against
    current specs, assert answer ∈ acceptable ∉ forbidden.
11. Docs: `docs/control-plane.md` (component map, switches, how to run the baseline) + a CLAUDE.md section with the rules.

## Acceptance

`cargo test --workspace` green (baseline 2632 passing tests' behavior preserved, adjusted only where moved), new modules
unit-tested, `python scripts/evals/run.py --dry-run` validates all tasks, a live smoke on an isolated home showing a trace
file with decisions from shadow specs after one chat turn (needs a configured model — if none is available, show the
trace from a turn against a `local:` model or report that the live part is pending). Report what is live vs shadow.
