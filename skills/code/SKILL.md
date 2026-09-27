---
name: code
description: Read, search, edit, and verify source code in the chat's working directory with the built-in tools (Read/Grep/Glob/Edit/Write/Bash/Task), and hand multi-file refactors to a Cowork DAG. Use when the user asks to fix a bug, add a feature, refactor, write tests, or explain code in a repository.
version: 2.0.0
---

# Code — working on a repository from chat

Coding in SenClaw runs **inside the ordinary chat agent**: there is no separate
code engine or code MCP server. The chat's working directory (chosen in
"New chat" → folder) is the repository; every built-in tool below operates
relative to it, and `Bash` runs under the sandbox exec policy when it is
enabled in Plugins → Sandbox.

## Tools you actually have

| Tool | Use it for |
|------|------------|
| `Glob` | find files by pattern (`src/**/*.rs`) |
| `Grep` | find text/regex across the tree (ripgrep semantics) |
| `Read` | read a file, optionally a line range; images and PDFs too |
| `Edit` | exact `old_string → new_string` replacement; `old_string` must be unique |
| `Write` | create a file or overwrite it whole |
| `NotebookEdit` | edit a Jupyter cell |
| `Bash` | build, lint, test, git; read-only commands never prompt |
| `TodoWrite` | track a multi-step task |
| `EnterPlanMode` / `ExitPlanMode` | design first when the change is large or ambiguous |
| `Task` | delegate an isolated sub-investigation to a subagent |
| `ToolSearch` | discover deferred MCP tools (kanban, memory, wiki, dispatch, …) by keyword |
| `find_symbol` / `find_references` / `symbol_body` / `repo_map` | tree-sitter index of the repository: where a name is defined, who uses it, one definition's source, a ranked outline — precise where Grep is noisy |

When the system prompt carries a `<repo_map>` block, start from it: it lists
the files and signatures that matter most for this repository (and for the
files your prompt mentions). It is an outline, not the code — read before you
edit.

Do **not** call `read_file`, `edit_file`, `get_skeleton`, `graph_*` or any
`senclaw-code*` server — they do not exist and the call will fail.

## Workflow

1. **Orient**: the `<repo_map>` block, then `find_symbol` / `find_references` for a named thing, `Glob` + `Grep` for text. Never guess a path.
2. **Read before you write**: `Edit` requires the exact current text; read the region first.
3. **Change the smallest thing**: one concern per edit; keep the project's conventions (naming, error types, test layout).
4. **Verify**: run the narrowest useful command (`cargo test -- <name>`, `npx tsc --noEmit`, `flutter analyze <file>`), then broaden when shared code changed. Paste the failing output verbatim if something fails — do not hide it.
5. **Report**: list changed files as `path:line`, what was verified, and what was not.

## When to use a Cowork DAG instead

Use `dispatch_task` (discover with `ToolSearch`) when the change spans **several
independent modules** that can be edited in parallel, or when review/test should
be a separate agent. Each task declares the paths it will write (write-set);
tasks with overlapping write-sets are serialized automatically. Prefer a single
agent for anything touching fewer than ~4 files — a DAG costs a planning turn.

**Isolated branches.** In a git repository, a DAG task or a `Task` subagent can
run with `isolation: "worktree"`: it edits its own checkout on a branch
`senclaw/…` and its result reports the branch and diff; the shared working
directory stays untouched until a person merges (Changes / Kanban UI, or
`POST /api/worktrees/merge`). Use it when two agents must touch the same files,
or when the user wants to review before anything lands. Outside a git
repository the task runs in the shared directory and says so.

## Rules

- Stay inside the working directory; `../` escapes are rejected.
- Never run destructive git (`reset --hard`, `push --force`, branch deletion) without an explicit ask.
- Never commit unless the user asked; if asked, use conventional commits.
- Secrets in files you read stay out of the reply.
