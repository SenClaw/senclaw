# Curated Memory — Design

> Agent-authored, human-readable memory with save + recall, modeled after Claude Code's
> file-based auto-memory. This is a **curated layer on top of the existing basic-memory
> infrastructure** — no new storage, no new DB schema.

## Motivation

SenClaw already ships two memory systems, but neither does what we want:

| System | Tools | Writes? | Nature |
|---|---|---|---|
| **Basic memory** (`senclaw-memory`) | `memory_search`, `memory_get` | ❌ read-only — syncs existing `.md` files, chunks + FTS5 + vector | read-only over `agents/{folder}/memory/*.md` |
| **Cognitive** (`senclaw-cognitive`) | `cog_add`, `cog_cognify`, `cog_search`, `cog_recall`, `cog_forget` | ✅ but into a **knowledge graph** (triplets + Hebbian dynamics), not human-readable files | entity/relation graph, multi-hop reasoning |

The gap: there is no way for an agent to **deliberately write one structured memory**
(frontmatter `name/description/type`, body, `[[wikilinks]]`), **maintain a `MEMORY.md`
index**, and have relevant memories **auto-recalled into context at the start of a
conversation** — exactly what Claude Code does.

Lucky break: `MemoryManager` (`src/memory/manager.rs`) already watches `memory/*.md`,
chunks, embeds, and FTS-indexes them. So **save** just has to write a well-formed file;
**recall** reuses the existing `hybrid_search`, scoped to curated files.

## Non-goals

- No new SQLite tables. `memory_files` / `memory_chunks` / `memory_chunks_fts` /
  `embedding_cache` are sufficient.
- Not a replacement for cognitive memory. The graph is for automatic entity-relation
  extraction and multi-hop reasoning; curated memory is for human-readable notes the
  agent (or user) writes and edits by hand.
- No LLM dependency on the save path (unlike `cog_cognify`).

## File layout

```
agents/{folder}/memory/
├── MEMORY.md              # index: one line per memory, "- [Title](file.md) — hook"
├── {kebab-name}.md        # one memory = one concept, with frontmatter
└── YYYY-MM-DD.md          # daily log (already exists — untouched by this feature)
```

Each memory file:

```markdown
---
name: is-admin-removed
description: Per-chat is_admin removed — every chat is admin; DAG dispatches only to virtual personas
metadata:
  node_type: memory
  type: project            # project | reference | feedback | user
  originSessionId: <group/session id>
  createdAt: 2026-07-01     # absolute date, never "today"
---

<body>. For type=project|feedback: **Why:** ... **How to apply:** ...
Cross-link other memories with [[their-slug]].
```

Invariants:
- `name` == filename (sans `.md`), kebab-case, no path separators.
- `description` is the **recall hook** (≤120 chars) — this is what gets embedded / FTS-matched.
- `MEMORY.md` sorted newest-first (recency signals relevance), never alphabetical.

### Frontmatter `type` semantics

- **`project`** — findings about THIS codebase: decisions, bugs fixed, gotchas, port
  research. Body: `**Why:**` + `**How to apply:**`.
- **`reference`** — solution-agnostic how-to / runbook. Body: imperative instructions,
  commands, caveats.
- **`feedback`** — a user request or design decision. Body: `**Request:**` +
  `**Decision:**` + `**Status:**`.
- **`user`** — freeform note the user hand-wrote for their own recall.

## MCP tools (server `senclaw-memory`)

Added to `src/mcp/memory_server.rs`, following the `memory_<verb>` naming convention.

### `memory_save`

```rust
#[rmcp::tool]
async fn memory_save(
    &self,
    name: String,                 // kebab-case slug == filename
    description: String,          // recall hook (required, short)
    body: String,                 // markdown body
    mem_type: Option<String>,     // project|reference|feedback|user; default "project"
    supersede: Option<bool>,      // true = overwrite existing file (update, not duplicate)
) -> String
```

Behavior:
1. Path-traversal guard; normalize `name` to kebab-case.
2. If `{name}.md` exists and `supersede != true` → return a warning ("already exists;
   pass supersede to update") instead of creating a duplicate. Enforces
   *update-not-duplicate*.
3. Write frontmatter (`node_type: memory`, `type`, `originSessionId` = folder/group,
   `createdAt` = absolute date) + body.
4. Update `MEMORY.md`: insert/replace the `- [Title](name.md) — description` line,
   newest first.
5. `manager.mark_dirty(folder, Some("{name}.md"))` → the watcher re-chunks + re-embeds +
   re-FTS-indexes on the next search.

### `memory_recall`

```rust
#[rmcp::tool]
async fn memory_recall(
    &self,
    query: String,
    max_results: Option<usize>,   // default 5
) -> String
```

- Calls the existing `hybrid_search`, filtered to curated files only (`source == "memory"`,
  excluding daily-log `YYYY-MM-DD.md` — exclusion logic already present in `memory_search`).
- Returns each matching memory with `name`, `type`, `description` + the matched body
  snippet, so the agent can cite `[[name]]`.

> Implementation note: `memory_recall` is essentially `memory_search` narrowed to the
> curated set and presented per-memory rather than per-chunk. A separate tool (vs. a
> `curated_only` flag on `memory_search`) is preferred because it signals intent more
> clearly to the agent.

### `memory_delete`

```rust
#[rmcp::tool]
async fn memory_delete(&self, name: String) -> String
```

- Deletes `{name}.md` + removes its line from `MEMORY.md`. Mirrors `cog_forget`.
- `mark_dirty` so the index drops the stale chunks.

## Auto-recall at conversation start

This is what turns "memory" into "self-remembering" — the behavior the screenshot shows.

At the `MessageRouter` / `AgentPool` point where a session for a group is set up:

1. Read `MEMORY.md`, and/or run `hybrid_search(folder, incoming_message_text,
   curated_only)`.
2. Take the top 3–5 matched memories.
3. Inject into the agent's system prompt as a `<system-reminder>`:

```
<system-reminder>
Relevant memories for this group (point-in-time; verify before asserting):
- [is_admin removed](is-admin-removed.md) — every chat is admin; DAG targets only virtual personas
- [channel_app migration](channel-app-migration.md) — web features migrated to Flutter over relay
</system-reminder>
```

4. Append a *staleness* caveat when `createdAt` is old (memories are point-in-time
   observations, not live state).

Attachment point: the prompt-build path in `src/agent/` (where persona + tools are already
injected). A single helper: `build_memory_reminder(folder, trigger_text) -> Option<String>`.

## Lifecycle rules (enforced in code where possible)

| Rule | Enforcement |
|---|---|
| Update-not-duplicate | `memory_save` refuses to overwrite a same-`name` file unless `supersede=true` |
| Delete wrong memories | `memory_delete(name)` removes file + index line |
| Don't store derivable facts (CLAUDE.md, git) | tool description + prompt guidance; not code-enforceable |
| Index always matches files | every save/delete atomically edits `MEMORY.md`; CLI `senclaw memory reindex` rebuilds if drifted |

## Relationship to cognitive memory

- **Curated memory** (this feature): human-readable notes — events, decisions, gotchas —
  authored and hand-edited by agent or user. Recall = "give me the relevant notes".
- **Cognitive graph** (existing): automatically extracted entity-relations for multi-hop
  reasoning. Recall = spreading activation.
- Optional future bridge: after `memory_save`, also call `cog_add(body)` so content lands
  in the graph too. Deferred — avoids pulling an LLM dependency onto the save path.

## Implementation scope

| Work | File | Size |
|---|---|---|
| `memory_save` / `memory_recall` / `memory_delete` tools | `src/mcp/memory_server.rs` | ~150 lines |
| Frontmatter write + `MEMORY.md` maintenance | new `src/memory/curated.rs` | ~120 lines |
| Auto-recall reminder builder | `src/agent/...` (prompt build) | ~40 lines |
| Config wiring (reuse `SENCLAW_FOLDER` / `SENCLAW_DB_PATH`, no new env) | `src/mcp/helper.rs` | small |
| CLI `senclaw memory reindex/list` | `src/cli/` | optional |

**No new DB schema** — `memory_files` / `memory_chunks` / FTS already suffice; save just
creates a file and lets `MemoryManager` ingest it.

---

# v2 — Auto-consolidation & recall injection (the `memoryRecall` toggle)

v1 gave agents *tools* (`memory_save` / `memory_recall` / `memory_delete`). v2 makes the
loop automatic, mirroring how Claude Code operates its own memory:

1. **History → memory consolidation** — when compaction drops conversation history, the
   dropped content is distilled into curated `memory/*.md` files.
2. **Recall injection** — every request runs a hybrid FTS5/vector search over the curated
   memories and injects relevant ones into the prompt, plus a hint that the agent can call
   `memory_recall` for deeper retrieval.

Both are gated by ONE new global toggle: **`memoryRecall`** (Settings → Agent behavior →
"Memory recall"), alongside `preTriggerSkill` / `preCognitive` / `afterProcess`. Default
OFF (opt-in, like its siblings).

## When is history "not injected"? (research findings)

Four conditions drop history from agent requests:

| # | Condition | Where |
|---|---|---|
| 1 | Fresh group / cursor filters everything | `message_router.rs` `run_agent` — empty prompt → skip |
| 2 | **Context overflow auto-compact** (≥75% of context) | `zen_core/conversation.rs` `auto_compact` — history before the current turn is LLM-summarized |
| 3 | `/reset` command | `command_dispatcher.rs` — wipes `group_messages` + cursor |
| 4 | FIFO retention (`max_messages`, default 100) | `db/messages.rs` `insert_group_message` — trims on insert |

**Consolidation attaches to #2** (and the `afterProcess` proactive `compact_now` path,
which reuses `auto_compact`): it is the only point where the dropped history still exists
*and* an LLM summary of it is produced. #1/#3/#4 drop raw rows without a summarization
moment — out of scope for v2.

## Consolidation pipeline

```
auto_compact (zen_core)             agent_pool (daemon)
  summarize_history(history)          on_compact_exec handler:
  → CompactExecData{summary} ──emit──▶  if memoryRecall && summary:
                                          tokio::spawn(consolidate_summary(...))
```

- `zen_core` stays memory-agnostic: the only change is that the event bridge's
  `CompactExecData` (`agent_pool/types.rs`) now carries `summary: Option<String>`
  (previously a unit struct that dropped the payload; `agent_pool/engine.rs` forwards it).
- `src/memory/consolidate.rs` — `consolidate_summary(base, folder, summary, llm, date)`:
  - **LLM distill** (via the existing `create_cognitive_llm` factory, same one cognify
    uses): system prompt asks for ≤3 durable memories as JSON
    `{"memories":[{name, description, type, body}]}`; each is saved via `curated::save`
    with `supersede=true` (re-emitted slugs update in place). An explicit
    `{"memories":[]}` verdict saves nothing — that is a success, not a fallback trigger.
  - **Verbatim fallback** (no LLM / call failed / unparsable): the summary itself is
    saved as `conversation-summary-YYYY-MM-DD` (same-day compactions update one file), so
    dropped history stays recallable even on LLM-less installs.
  - Fire-and-forget: never fails or delays the turn; failures log only.
  - After saving, `MemoryManager::mark_dirty(folder)` so the new files are indexed on the
    next search.

## Recall injection pipeline

Injection joins the existing pre-retrieval stage in `agent_pool/pool.rs` (three
independent backends now):

| Backend | Toggle | Block |
|---|---|---|
| MEMORY.md verbatim | env `memory.pre_retrieval` | `<memory>` |
| Cognitive graph (spreading activation) | `preCognitive` | `<cognitive_memory>` |
| **Curated memories (hybrid FTS5/vector)** | **`memoryRecall`** | **`<memory_recall>`** |

`curated_pre_retrieval(query, folder, max_results)` mirrors `cognitive_pre_retrieval`'s
contract (never fails the turn; empty string on error/no-hits):

1. `MemoryManager::search(folder, query, source="memory")` — the daemon-side manager
   already wires DB + embedding provider + dirty-sync.
2. Filter to curated files only: skip `MEMORY.md` and any `YYYY-MM-DD.md` daily log.
3. Dedupe per file, take top 3–5, present per-memory: `name (type) — hook` + snippet
   (char-safe truncation for VN/CJK).
4. Append the tool hint: *"If you need more detail, call the memory_recall tool (search)
   or memory_get (read a full memory file)."* — this is the prompt piece that teaches the
   agent the escalation path.

The block also opens with a staleness caveat ("point-in-time notes — verify before
asserting"), matching Claude Code's convention.

## Settings plumbing (all four layers)

| Layer | Change |
|---|---|
| `group_manager/types.rs` | `GlobalConfig.memory_recall: Option<bool>` (JSON `memoryRecall`) |
| `group_manager/llm.rs` | `get_/save_memory_recall_enabled` (+ `mod.rs` re-export) |
| `ui_server/agent_behavior_config.rs` | `memoryRecall` in GET/POST `/api/agent-behavior` |
| Flutter `settings_screen.dart` | fourth `_ToggleRow` "Memory recall" |
| Web `AgentBehaviorSettings.tsx` | fourth row, same field |

## Relationship to the other memory stages

- `preCognitive` and `memoryRecall` are independent and composable — graph recall answers
  "what entities/relations connect to this?", curated recall answers "what notes did we
  keep about this?".
- `afterProcess` (proactive compaction) *feeds* consolidation: with both on, every
  after-turn compaction also distills memories. With only `memoryRecall` on,
  consolidation still fires on threshold-triggered auto-compacts.
- UI rename: the sidebar item for the cognitive screen is now labelled **Knowledge**
  (was "Memory") to keep "memory" unambiguous for this feature.
