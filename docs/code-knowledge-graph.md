# Code Knowledge Graph

> **Lịch sử:** các module mô tả trong tài liệu này (`code_engine`, `code_graph`, MCP `senclaw-code*`, trang web Code) đã bị gỡ ở commit 83e9720 (2026-06-20). Thiết kế hiện hành: [`plans/260912-0141-code-v2/`](../plans/260912-0141-code-v2/README.md).

Inspired by [GitNexus](https://github.com/abhigyanpatwari/GitNexus).

## Rust module map

| Module | Path | Description |
|---|---|---|
| `types` | `src/code_graph/types.rs` | `NodeKind`, `EdgeKind`, `Language`, `ParsedFile`, `IndexStats`, `ImpactNode`, `CallEdge` |
| `parser` | `src/code_graph/parser.rs` | tree-sitter per language → `ParsedFile` |
| `schema` | `src/code_graph/schema.rs` | `apply_code_graph_schema()` DDL |
| `indexer` | `src/code_graph/indexer.rs` | `CodeGraphIndexer` — discover → parse → store → resolve |
| `query` | `src/code_graph/query.rs` | `GraphQuery` — all read queries |
| `code_graph_server` | `src/mcp/code_graph_server.rs` | `senclaw-code-graph` MCP stdio server |
| `helper` | `src/mcp/helper.rs` | `code_graph_mcp_config()` — builds `McpServerConfig` |
| `code_engine/server` | `src/code_engine/server.rs` | `senclaw-code` MCP server (read/write/edit/bash) |

### Entry points

```
senclaw code-graph-server   # starts senclaw-code-graph over stdio
senclaw code-server          # starts senclaw-code over stdio
```

Both are registered in `main.rs` and listed in `McpManager::get_builtin_servers()`.

## Overview

The code knowledge graph converts a codebase into a queryable graph of symbols (nodes) and relationships (edges). AI agents can answer questions like "who calls `authenticate`?", "what breaks if I change this interface?", or "show me the call tree from `handle_request`" without reading entire files.

## Pipeline

```
Source files
    │
    ▼
[1] Parser (tree-sitter)
    │  AST per file
    ▼
[2] Extractor
    │  RawNode: name, kind, signature, line range
    │  RawEdge: from_name → to_name, kind (calls/imports/…)
    ▼
[3] Indexer → SQLite
    │  cg_symbols  — one row per symbol
    │  cg_edges    — one row per relationship
    │  resolve_edges — cross-file to_sym_id fill
    ▼
[4] Query layer
       callers / callees / impact BFS / call-tree DFS / FTS5 search / skeleton
```

## Graph entities

### Node kinds

| Kind | Languages |
|------|-----------|
| `function` / `async_function` | Rust, TS, JS, Python |
| `method` | Rust (impl), TS/JS (class), Python (class) |
| `class` | TS/JS, Python |
| `struct` | Rust |
| `trait` | Rust |
| `interface` | TS |
| `enum` | Rust, TS |
| `type` | Rust, TS |
| `const` | Rust, TS |
| `module` | Rust |
| `file` | all |

### Edge kinds

| Kind | Meaning |
|------|---------|
| `calls` | A calls B (direct invocation) |
| `imports` | File A imports module B |
| `extends` | Class A extends B |
| `implements` | Class A implements interface B |
| `defines` | Module/impl A defines symbol B |
| `uses_type` | A references type B |

## SQLite schema

```sql
-- Symbols (nodes)
CREATE TABLE cg_symbols (
  id          INTEGER PRIMARY KEY,
  project_id  TEXT NOT NULL,
  file_path   TEXT NOT NULL,
  name        TEXT NOT NULL,
  kind        TEXT NOT NULL,
  signature   TEXT,
  start_line  INTEGER,
  end_line    INTEGER,
  language    TEXT,
  indexed_at  INTEGER
);
CREATE INDEX cg_sym_proj_name ON cg_symbols(project_id, name);
CREATE INDEX cg_sym_proj_file ON cg_symbols(project_id, file_path);

-- Relationships (edges)
CREATE TABLE cg_edges (
  id          INTEGER PRIMARY KEY,
  project_id  TEXT NOT NULL,
  from_sym_id INTEGER REFERENCES cg_symbols(id) ON DELETE CASCADE,
  from_name   TEXT NOT NULL,
  from_file   TEXT NOT NULL,
  to_sym_id   INTEGER REFERENCES cg_symbols(id) ON DELETE SET NULL,
  to_name     TEXT NOT NULL,
  to_file     TEXT,
  kind        TEXT NOT NULL,
  at_line     INTEGER
);
CREATE INDEX cg_edge_proj_to   ON cg_edges(project_id, to_name, kind);
CREATE INDEX cg_edge_proj_from ON cg_edges(project_id, from_name, kind);

-- Incremental index state
CREATE TABLE cg_index_state (
  project_id    TEXT NOT NULL,
  file_path     TEXT NOT NULL,
  mtime_secs    INTEGER NOT NULL,
  symbol_count  INTEGER DEFAULT 0,
  edge_count    INTEGER DEFAULT 0,
  PRIMARY KEY (project_id, file_path)
);

-- FTS5 for full-text symbol search
CREATE VIRTUAL TABLE cg_symbols_fts USING fts5(name, signature, content='cg_symbols', content_rowid='id');
```

## Languages supported

| Language | Extensions | Symbols | Edges |
|----------|-----------|---------|-------|
| Rust | `.rs` | fn, struct, enum, trait, type, const, mod, impl methods | calls, imports (use), defines |
| TypeScript | `.ts`, `.tsx` | function, class, interface, type, const, method | calls, imports, extends, implements |
| JavaScript | `.js`, `.jsx` | function, class, const, method | calls, imports, extends |
| Python | `.py` | function, class, method | calls, imports |

## MCP server: `senclaw-code-graph`

Spawned by `McpManager` as a subprocess. Env vars: `SENCLAW_DB_PATH`, `SENCLAW_PROJECT_ID`, `SENCLAW_WORKSPACE`.

### Tools

| Tool | Description |
|------|-------------|
| `graph_reindex` | Build/update index. `incremental=true` skips unchanged files (mtime). |
| `graph_find_callers` | Who calls symbol X? Returns file + line for each call site. |
| `graph_find_callees` | What does symbol X call? |
| `graph_impact` | Blast radius BFS: which symbols/files are affected if X changes? |
| `graph_symbol_context` | Full context: signature + callers + callees + file skeleton. |
| `graph_trace_flow` | DFS call tree from entry point. |
| `graph_search` | FTS5 full-text search on symbol name + signature. |
| `graph_skeleton` | Token-efficient skeleton: only signatures, no body. File or whole project. |
| `graph_file_deps` | File-level imports/importers. |

### Usage example (agent session)

```
User: before I refactor `authenticate`, what's the blast radius?

Agent calls:
  graph_impact { name: "authenticate", depth: 3 }
  → ⚠️ 12 symbols across 4 files affected at depth 1-2

  graph_symbol_context { name: "authenticate" }
  → signature, 8 callers, 3 callees, file skeleton

User: show me the call tree from `handle_login`

Agent calls:
  graph_trace_flow { entry: "handle_login", max_depth: 4 }
  → DFS tree: handle_login → authenticate → verify_token → decode_jwt
```

## Integration

### Register in agent config

```rust
use crate::mcp::helper::code_graph_mcp_config;

let cfg = code_graph_mcp_config(
    &config.paths.db_path.to_string_lossy(),
    &project_id,
    &workspace_path,
);
agent_pool.register_mcp_server(cfg);
```

### Incremental indexing

The indexer checks `mtime_secs` in `cg_index_state`. On `graph_reindex { incremental: true }` (default), only files with changed mtimes are re-parsed. A full workspace index of ~500 files typically completes in < 2s.

### Skipped paths

`node_modules`, `.git`, `target`, `dist`, `build`, `__pycache__`, `.venv`, `venv`, `.senclaw-code`

Max file size: 512 KB.

## Feasibility notes

- **tree-sitter** provides deterministic, error-tolerant parsing — works on partial/incomplete files.
- **Cross-language**: the same SQLite schema handles all languages; adding a new language requires only a new `walk_*` function in `parser.rs`.
- **Incremental cost**: after initial index, only changed files are re-parsed; query latency is SQLite-bound (sub-ms for typical graph queries).
- **Token savings**: `graph_skeleton` gives agents a file map in ~10% of the tokens of reading the full file.
- **Limitation**: call resolution is name-based (not type-inferred), so overloaded names may produce false edges. Dynamic dispatch and runtime reflection are not tracked.
