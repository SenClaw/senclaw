---
name: workflow
description: Author and run reusable multi-step workflows — fan out parallel agent (persona) tasks plus deterministic script steps as a saved, parameterized routine. Use when the user wants to codify a repeatable routine instead of re-typing orchestration each time.
version: 1.0.0
triggers:
  - "workflow"
  - "quy trình"
  - "routine"
---

# Reusable Workflows

A **workflow** is a saved, parameterized DAG of steps that runs as a fixed routine. Use it when the user has a *repeatable* routine (e.g. "weekly competitor research: N angles in parallel, then one summary report") that they'd otherwise re-describe every time. For one-off creative orchestration, just dispatch directly — don't make a workflow.

Each step is either an **agent** step (an isolated persona doing judgement work) or a **script** step (deterministic shell: fetch/transform/file ops). Steps form a DAG via `dependsOn`. The executor only spawns isolated sessions — it never touches live group agents.

## Where definitions live

User workflow definitions are Markdown files with YAML frontmatter at:

```
~/senclaw/workflows/<name>.md          # (override: $SENCLAW_WORKFLOWS_DIR)
```

Your job (the agent) is usually to **author / edit the `.md` definition** when the user describes a routine. Running is normally done by the user via CLI or the UI; you may run it for them when asked.

## Definition format example

```yaml
---
name: market-research                 # unique; falls back to filename
description: multi-angle research + summary report
version: 1.0.0
inputs:                               # run-time parameters
  - { name: topic, required: true }
  - { name: depth, default: "standard" }
guidance: |                           # workflow-level rules, applied to ALL agent steps
  Be concise; cite evidence.
# workspace: ~/research/llm          # optional: custom workspace dir (default ~/senclaw/workflow-data/<name>/)
steps:
  - id: research_tech                 # unique id; referenced by {{steps.research_tech.result}}
    kind: agent
    persona: researcher               # must exist in ~/senclaw/virtual-agents/researcher.md
    prompt: |
      Research "{{input.topic}}" from a technical angle, depth {{input.depth}}.
    guidance: |                       # step-level rules (merged after workflow guidance)
      Last 2 years only; structure as maturity/risks/trends; no market data.
    timeout: 300                      # seconds (default 600)
    observe: { label: "Tech research", from: result, as: inline }

  - id: research_market
    kind: agent
    persona: researcher
    prompt: "Research \"{{input.topic}}\" from a market angle."

  - id: fetch_metrics
    kind: script
    run: |                            # host tools available (cwd = this workflow's persistent workspace)
      curl -s "https://api.example.com/price?q=$WF_INPUT_TOPIC" > "$WF_RUN_DIR/metrics.json"
      echo fetched
    observe: { label: "Raw metrics", from: { file: metrics.json }, as: artifact }

  - id: summary
    kind: agent
    persona: analyst
    dependsOn: [research_tech, research_market, fetch_metrics]
    prompt: |
      Summarize into a report:
      Tech: {{steps.research_tech.result}}
      Market: {{steps.research_market.result}}
      Metrics are in metrics.json inside the run workspace.
    observe: { label: "Final report", from: result, as: inline }
---
(optional body: human-readable notes about this workflow)
```

### Step fields

| field | applies to | meaning |
|---|---|---|
| `id` | all | unique; downstream refer via `{{steps.<id>.result}}` |
| `kind` | all | `agent` \| `script` |
| `dependsOn` | all | upstream step ids; empty = entry node. **Auto-inferred** from data refs (see below) — only needed for ordering-only deps with no data ref |
| `timeout` | all | seconds (default 600) |
| `observe` | all | optional human-facing output (see below) |
| `persona` | agent | persona name in `~/senclaw/virtual-agents/` — **must exist** |
| `prompt` | agent | the task; supports `{{}}` |
| `guidance` | agent | rules/constraints; supports `{{}}` |
| `run` | script | inline shell command |
| `scriptFile` | script | path to a script (relative to the def file or absolute; must be executable) |

## How data flows between steps

Two channels:

1. **`result` string** — every step produces a `result` (agent = final message, script = stdout). Reference it downstream with `{{steps.<id>.result}}`.
2. **Shared workspace** — one persistent dir **per workflow** (not per run); every step's cwd. Pass real files/data here (script writes `data.csv` → agent reads it); it also carries state across runs (see below).

**Large script results auto-spill.** If a script's result exceeds ~5000 chars, the full output is written to `<workspace>/.results/<stepId>.txt` and `result` becomes a pointer line (`[truncated: N chars total; full output saved to <path>]`) plus a 300-char preview. This keeps downstream env (`WF_STEP_*_RESULT`) and the run record small. If a downstream step needs the **full** data, read the file at that path — or better, have the producer write to a known file under `WF_RUN_DIR` and pass the path explicitly rather than relying on a huge `result`.

**Templating** (agent `prompt` / `guidance`): `{{input.<name>}}` and `{{steps.<id>.result}}` — plain substitution, no logic.

**Data refs auto-create dependencies.** Referencing a step's result — `{{steps.X.result}}` in a prompt/guidance, or `$WF_STEP_X_RESULT` in an inline `run` — automatically adds `X` to that step's `dependsOn` (union with whatever you declared). So a ref and its dependency can never drift out of sync, and you rarely need to write `dependsOn` by hand. Caveats: referencing a non-existent step is a load error (fails loud, not a silent empty); `scriptFile` bodies aren't scanned, so declare their `dependsOn` explicitly; `{{steps.*}}` is **not** allowed in workflow-level `guidance` (it applies to every agent step).

**Script env vars** (scripts don't get `{{}}` — they read env, safer):
- `WF_INPUT_<NAME>` — each run input (name upper-cased)
- `WF_STEP_<ID>_RESULT` — each completed upstream step's result
- `WF_RUN_DIR` — the workspace (see below) — **same dir as `WF_WORKFLOW_DIR`**
- `WF_WORKFLOW_DIR` — the workspace (alias of `WF_RUN_DIR`)
- `WF_OBSERVE_DIR` — observe scratch dir (`<workspace>/.observe`)

## Working directory & files (important)

Every step's **cwd is the workspace**, which is **one persistent directory per workflow, shared across all runs** — `~/senclaw/workflow-data/<name>/` by default (override with the `workspace` field). Think of it as a continuing project folder, not a throwaway scratch dir. `WF_RUN_DIR` and `WF_WORKFLOW_DIR` both point here. Implications — read before writing scripts:

- **It persists.** Output written here (notes, reports, a `venv`, caches) survives across runs and accumulates. That's the point: same task type = one project. The workspace is **never auto-deleted** (only the last 10 run *records* per workflow are kept for history; files stay).
- **It is NOT a sandbox.** The host env is inherited: `PATH`, system `python3`/`node`/`curl`, installed tools all work. Missing *files* — not "no environment" — is the usual problem.
- **To read the user's files** (a PDF, a repo, a CSV): **pass an absolute path as an input** and read it directly — don't assume it's in cwd. e.g. `inputs: [{ name: paper_path, required: true }]` → `"$WF_INPUT_PAPER_PATH"`.
- **Organize complex/repeated output in subdirs you create** — since the workspace is shared, a workflow that produces a distinct deliverable per run should make its own subdir (per topic/date/paper), e.g. `mkdir -p "$WF_RUN_DIR/$(date +%F)"`. Agents: create a sensible sub-project dir for each distinct piece of work instead of dumping everything in the root.
- **Build heavy environments once, idempotently:**
  ```bash
  [ -d "$WF_WORKFLOW_DIR/venv" ] || python3 -m venv "$WF_WORKFLOW_DIR/venv"
  "$WF_WORKFLOW_DIR/venv/bin/pip" install -q -r requirements.txt
  ```
- **Concurrency:** the same workspace can't run twice at once — a second concurrent run of the same workflow (or any run pointing at the same custom dir) is rejected, so steps never clobber each other mid-run.

## Agent steps = three layers (don't conflate)

| layer | source | role |
|---|---|---|
| identity | `persona` (its system prompt) | who the agent is |
| **rules** | **`guidance`** (workflow + step, merged) | how/constraints — the layer to tune |
| task | `prompt` | what to do this run (varies with inputs) |

**When authoring, infer a sensible `guidance` for each agent step** (output format, scope limits, tone) — that's the field the user will most want to tweak. Keep `prompt` as the parameterized task, `guidance` as the stable rules.

## observe (optional human-facing output)

Pure observation — does NOT affect the DAG. Two forms:
- `as: inline` → short markdown shown on the node (`from: result` or `from: { file }`).
- `as: artifact` → a richer file (HTML/report) referenced by path (`from: { file: report.html }`).

Omit `observe` and the step just shows status. Use it on the steps whose output a human wants to glance at.

## Common pattern: fan-out → aggregate

"N personas in parallel → 1 summarizer" = **N sibling agent steps (no deps) + 1 aggregator step with `dependsOn: [all N]`**. No special syntax needed (see `market-research` above). Parallelism is automatic, capped at 5 concurrent steps per run.

## Authoring (write the definition)

**Before writing, verify personas.** List `~/senclaw/virtual-agents/` first and use ONLY personas that exist there — an agent step naming a missing persona fails at runtime, and this is the single most common authoring bug. If no persona fits, either use script steps or tell the user which persona to create first.

**After writing, hand off to the tuning flow.** End your reply with: the workflow name, its inputs, and this pointer — *"Mở Plugins → Workflow, bấm nút Tinh chỉnh để sửa guidance từng bước, rồi bấm Chạy."* Do NOT run the workflow yourself unless the user explicitly asks — running is the user's action.

Write the `.md` to the workflows dir. Either use the Write tool, or heredoc:

```bash
mkdir -p ~/senclaw/workflows
cat > ~/senclaw/workflows/daily-digest.md <<'WF_EOF'
---
name: daily-digest
description: fetch + summarize
inputs:
  - { name: feed_url, required: true }
steps:
  - id: fetch
    kind: script
    run: |
      curl -s "$WF_INPUT_FEED_URL" > "$WF_RUN_DIR/raw.txt"
      wc -l < "$WF_RUN_DIR/raw.txt"
  - id: digest
    kind: agent
    persona: summarizer
    dependsOn: [fetch]
    prompt: "Summarize raw.txt ({{steps.fetch.result}} lines) into 5 bullet points."
    guidance: "One sentence per bullet."
    observe: { label: "Digest", from: result, as: inline }
---
WF_EOF
```

## Running & listing

```bash
senclaw workflow list                                     # list available workflows
senclaw workflow show market-research                     # inspect steps/inputs/deps
senclaw workflow run market-research -i topic=local-llms -i depth=deep
senclaw workflow run market-research -i topic=X --json    # full run record as JSON
senclaw workflow runs --name market-research              # run history (newest first)
```

`run` prints each step's status, result preview, and observe output. runId = `<workflow-name>-<NNNN>` (monotonic per workflow).

The daemon also exposes REST endpoints (`GET /api/workflows`, `GET /api/workflows/runs`, `POST /api/workflows/<name>/run`, `POST /api/workflows/runs/<id>/cancel`) and pushes `workflow:update` WS events on every state change.

**Workspace & history**: each workflow shares **one persistent workspace** (default `~/senclaw/workflow-data/<name>/`, customizable via the `workspace` field). All steps run there; artifacts accumulate across runs and are **never auto-deleted**. `~/.senclaw/workflow-runs.json` keeps only the most recent **10 run records** per workflow (for history/status); evicting old records never touches workspace files.

## Constraints & gotchas

- **An agent step's `persona` must already exist** in `~/senclaw/virtual-agents/`, or that step fails immediately. Confirm/create the persona first.
- **`dependsOn` may only reference existing step ids and must be acyclic** (validation failures skip the file at load time); referencing a step's result auto-joins its dependsOn, and referencing a non-existent step is an error.
- **Scripts do deterministic work only** (fetch/transform/files/external APIs); don't secretly start agents inside a script — use an agent step (otherwise it escapes concurrency limits and the UI can't see it).
- **Script steps use POSIX shell syntax** (`$VAR`, `>>`, `[ -d … ]`). macOS/Linux use `/bin/sh`. On Windows the fallback is `cmd.exe` (POSIX syntax fails) — point `SENCLAW_WORKFLOW_SHELL` at any POSIX shell (e.g. Git Bash) to override. **Agent-only workflows are unaffected.**
- **Failures cascade**: a failed step → dependents are skipped → the run is `partial-failed`.
- Static fan-out (fixed N) is supported; **dynamic fan-out** (count decided by an upstream result), approval gates, and conditional branches are not yet.
- Definition file edits take effect on the next list/run (re-scanned every time) — no restart needed.
- **Restarts interrupt in-flight runs**: on daemon restart, runs still `running` are reconciled to `interrupted` (running steps → failed, pending → skipped). Runs don't auto-resume; re-trigger them.
