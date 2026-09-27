# Control-plane seam map (scout output, old repo @ f0f31bd)

Read-only scout report, lightly verified by the lead. Treat every file:line as a pointer to check, not a fact.
**Known correction:** exact-duplicate tool-call interception DOES exist — `src/zen_core/conversation.rs:1494`
("Intercept exact-duplicate tool calls…", `is_duplicate_exempt` at :795); the scout's "no exact-duplicate detection"
note under §3 is wrong. No date/time is injected into the system prompt (verified by grep).

## 1. System prompt / context assembly
- `zen_core/prompt.rs:15` `SYSTEM_PROMPT` (base); `:117` `auto_memory_prompt(memory_dir)` once per session; `:170`
  `project_docs_block(working_dir)` if docs/ exists; `:350` `render_deferred_tools_reminder(deferred)` grouped by MCP
  server; `:500` `render_skills_reminder(skills)`.
- `agent/prompt_directives.rs` per-mode reminders (Plan, DAG, Agent). `zen_core/context.rs:31` `EngineStore` holds `CoreConfig`.
- Varies per turn: user input, deferred-tools reminder, skills reminder, pending-input injections, mode reminders.
  Not in the core prompt: date/time, user profile, language, repo map.
- Prompt cache: Anthropic `cache_control` on the system block + last tool block (QueryConfig ~:476).
- Seam: `conversation::query()` after message assembly, before `query_llm()`; or the `prompt::render_*` functions.

## 2. Compaction
- `conversation.rs:330` `auto_compact()` (LLM summary, fallback), `:285` `compact_now()` (after-process), `:75`
  `compact_messages()` (deterministic keep-recent), `:185` `summarize_history()` (COMPRESSION_PROMPT).
- `COMPACT_KEEP_RECENT=12` (:37), `AUTO_COMPACT_THRESHOLD_RATIO=0.75` (:45), `AFTER_PROCESS_MIN_MESSAGES=16` (:50).
- Triggers: in-loop at ≥75% of the context window; after-process at ≥16 messages.
- Output: `[CompactNotice (user), Summary (assistant)] + kept current turn`; events `CompactStart`, `CompactExec`
  (tokens before/after, rate, summary, err_msg), `LlmUsage{source=compact}`; hooks `PreCompact`/`PostCompact`.
- Failure: LLM failure → truncation fallback; truncation failure → unchanged + error event.

## 3. Tool execution (`zen_core/run_tools.rs`)
- `run_tools()` :294 → `run_concurrently()` :320 (all read-only, join_all) / `run_serially()` :340 (writes, tool list
  refresh) → `run_single_tool()` :390.
- Resolution in `ctx.tools`, fallback `ctx.tools_resolver()` (:402–407). `resolve_path_inputs()` :93 runs before
  validation and permission (`Tool::path_fields()`).
- Validation: schema `validate_tool_input()` :464, then async `tool.validate_input()` :494 → `ToolExecutionError`.
- Permission (write tools): `PreToolUse` hook → `PrePermission` hook → user prompt (ResponseRegistry) →
  `PermissionRequest` hook; fail-closed.
- Results via `gen_tool_result_message()`; 32 KB truncation only in the trajectory, not in the LLM result.
- `TOOL_ERROR_NUDGE` after N consecutive errors; `TOOL_ERROR_FINAL_NUDGE` + hard stop at 2×N.
- Seams: `PreToolUse`, `PrePermission`, `PostToolUse`; events `ToolExecutionComplete/Error`.

## 4. Tool trait & registry
- Trait `zen_core/mod.rs:998`: `name, description, input_schema, is_read_only (:1005), permission_name (:1014),
  path_fields (:1031), validate_input (:1036), call (:1046), gen_tool_result_message, get_display_title,
  gen_tool_permission, search_hint, should_defer, always_load, aliases, renamed_from`.
- Built-ins (engine.rs / tools/mod.rs): Read, Grep, Glob, Time, ToolSearch, Profile get/update, WebFetch, Write, Edit,
  NotebookEdit, Bash, AskUser, AskUserQuestion, FormUI, Task, DispatchCreateParent, DispatchCreateParentAndRun,
  ExitPlanMode, EnterPlanMode, Skill, TodoWrite, PersonaUpdate, EmitWidget. MCP tools wrapped by the manager;
  deferred tools loaded through ToolSearch; `ToolsResolver` refreshes each turn.

## 5. Permissions (`zen_core/permissions.rs`)
- `PermissionManager` :42; skip flags `skip_file_edit/skip_bash/skip_skill/skip_mcp` (:48–51, `update_skip_flags()`).
- `is_safe_command()` :143 (shell safety classifier: per-pipe readonly whitelist, dangerous flags, injection checks);
  `derive_prefix()` :152 → saved `Bash(prefix:*)`, `matches_saved_prefix()` :169; `get_permission_key()` :113.
- In-memory `allowed_tools`, `global_edit_granted`; `PrePermission` hook classified by `classify_pre_permission()` :213.
- Decision gate plugs into `PermissionBridge::handle_permission_request_gated` via the `PermissionGate` trait.

## 6. Loop limits & errors
- Max turns 30 (`SENCLAW_MAX_AGENT_TURNS`, per-engine `max_turns_override`) — conversation.rs:534.
- Stall: 4 consecutive tool-only turns (`SENCLAW_STALL_TOOL_TURNS`) → `STALL_NUDGE`, hard stop at 2×N (:546).
- Empty retries 2 (`SENCLAW_EMPTY_RETRIES`, 500 ms × attempt) (:592); completion nudges 2 (`SENCLAW_COMPLETION_NUDGES`) (:603).
- `query_llm.rs`: `LLM_TURN_TIMEOUT` 180 s (:54), `LLM_TURN_TIMEOUT_LOCAL` 900 s (:70). CancellationToken after tools.

## 7. Todo
- `tools/todo_write.rs` `TodoWriteTool`, state in `zen_core/state.rs` `StateManager`, event `EngineEvent::TodosUpdate`.

## 8. Events & observers
- `zen_core/events.rs:74` EventBus (tokio broadcast 512, fire-and-forget); `EngineEvent` (:22): SessionReady/
  Interrupted/Error, InputReceived, MessageComplete, ConversationUsage, LlmUsage, Thinking/TextChunk,
  ToolPermissionRequest/Response, ToolExecutionComplete/Error, TodosUpdate, CompactStart/Exec, FileReference,
  Form*, PlanExit*, TaskAgentStart/End, Workbench*. ResponseRegistry :114 (oneshot pairs).
- Trajectory `~/.senclaw/trajectories/<jid>/<turn>.jsonl` (role user/assistant/tool/meta; 32 KB tool truncation;
  0600/0700; `SENCLAW_TRAJECTORY=1` or `enabled.json`; `force_enable(jid)` process-only).
- Failure ledger: one row per failure episode (tool, args shape, error ≤600 B, outcome); `/api/failures/summary`.

## 9. Skill routing
- `decision/skill_route.rs` (`decide` :75, `legacy_route` :66, `RouteReport` :45), `skills/matching.rs`.
- Engine only co-signs the keyword top-1 (agreement, or whole-phrase match with engine pick in top 3); modes off/shadow/on;
  `ROUTE_TIMEOUT=1500ms`, `CANDIDATES=8`; output `SkillRoute{name, force}`.

## 10. Background & watches
- `scheduler/watch.rs`: `WatchConfig` :122, `DoneWhen` :86 (path + op Exists/Equals/NotEquals/Contains/NotContains/In/NotIn),
  `MAX_ERROR_STREAK=5` :44, `MAX_TOOL_ERROR_STREAK=2` :51, `MAX_RESULT_CHARS=4000` :38.

## 11. Evals
- `scripts/evals/run.py`, `evals/cases/*.json`, trajectory JSONL, `OneShotOptions.trajectory_jid`, `force_enable`.

## 12. Usage & cost (`src/usage/mod.rs`)
- `UsageEvent` :88 (ts, source, jid, agent_id, session_id, app_id, profile, provider, model, input_tokens,
  output_tokens, cache_creation_tokens, cache_read_tokens, latency_ms, ok, estimated); `UsageSource` :34
  (Agent, Subagent, Compact, Hook, Bridge, Cognitive, Embedding, AppDirect); `UsageRecorder` :151 (MPSC 10K, flush 5 s/100);
  table `llm_usage_log`; `total_input()` :144.

## Highest-risk seams
1. Moving content between system and user roles changes token accounting and cache hits.
2. Re-resolving paths in a new hook double-joins relative paths.
3. An auto-Allow through `PrePermission` bypasses the user prompt entirely.
4. Changing read-only classification after dispatch runs writes concurrently.
5. Compaction thresholds must stay consistent with `count_tokens()` and the 0.75 ratio.
6. Editing messages mid-turn can drop injected pending inputs.
7. Re-splicing full tool results bloats the context.
8. ResponseRegistry oneshots panic if fired twice.
9. Three-level budget resolution (env > per-engine > default) must stay single-sourced.
10. `force=true` skill loads inject full schemas.
