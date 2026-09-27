# Workflows — saved, parameterized DAGs of agent + script steps

A **workflow** is a reusable, declarative pipeline: a Markdown file with YAML
frontmatter that describes a DAG of steps, where each step is either an
**agent** step (an isolated one-shot persona session) or a **script** step
(a shell command). Workflows are started with a set of **inputs**, run in a
persistent **workspace directory**, and every run is recorded with full
per-step results.

Compared to the other multi-agent mechanisms:

| | Workflow | DAG Team (dispatch) | Cowork |
|---|---|---|---|
| Shape | fixed DAG, authored up front | planned dynamically by a lead agent | persistent team + triggers |
| Steps | agent (persona) or shell script | agent tasks | member agents |
| Coupling | fully decoupled from AgentPool | runs inside the agent pool | built on dispatch |
| Reuse | saved `.md`, parameterized inputs | per-request | per-team |
| Best for | repeatable pipelines (research reports, ETL, release checks) | open-ended tasks | ongoing collaboration |

Implementation lives in [`src/workflow/`](../src/workflow/) (registry →
service → executor → step_runners), a Rust port of the upstream SemaClaw
TypeScript module.

## Where definitions live

```
~/senclaw/workflows/<name>.md        # definitions (hot-reloaded on each list/start)
~/senclaw/workflow-data/<name>/      # default persistent workspace (cwd of every step)
~/.senclaw/workflow-runs.json        # run history (statuses, results, observe outputs)
```

Invalid definition files are skipped with a warning — they never poison the
list.

## Definition format

```markdown
---
name: shopee-research
description: Research best-selling products and produce a cited report.
version: "1"

# Run-level parameters. The UI renders a form from these.
inputs:
  - name: category
    required: true
    default: "electronics"
    description: Product category to research
  - name: year
    required: true
    default: "2026"

# Workflow-level rules, prepended to every agent step's guidance.
guidance: >
  Prefer real sales data. Never invent numbers; say so when data is missing.

# Optional custom workspace (default: ~/senclaw/workflow-data/<name>/)
# workspace: /path/to/dir

steps:
  - id: gather
    kind: agent
    persona: browser-agent          # PersonaRegistry name
    prompt: >
      Search {{input.category}} best-sellers of {{input.year}} and extract
      the top 10 with prices and sold counts.
    guidance: Only cite numbers you actually saw on the page.
    timeout: 600                    # seconds, default 600
    observe:                        # optional human-facing intermediate output
      label: Raw data
      from: result                  # result | { file: <path relative to workspace> }
      as: inline                    # inline (markdown on the node) | artifact (Workbench viewer)

  - id: normalize
    kind: script
    dependsOn: [gather]
    run: python3 normalize.py       # or scriptFile: scripts/normalize.py
    timeout: 300

  - id: report
    kind: agent
    persona: writer
    dependsOn: [normalize]
    prompt: >
      Write a market report from this data:
      {{steps.gather.result}}
    observe:
      label: Final report
      from: { file: report.md }
      as: artifact
---

Free-form Markdown body — use it for notes; only the frontmatter is executed.
```

Validation on load: unique step ids, `dependsOn` references exist, the graph
is acyclic, `agent` steps declare a `persona`, `script` steps declare exactly
one of `run` / `scriptFile`.

## Templating — two deliberately separate channels

- **Agent prompts / guidance**: `{{input.X}}` and `{{steps.ID.result}}` are
  substituted as plain text (safe — the value becomes LLM input). The
  template language is intentionally logic-free: no conditionals, no loops.
- **Scripts**: values are **never** interpolated into the shell command
  (injection risk). They are exported as environment variables the script
  reads itself, with the run workspace as the working directory:
  - `WF_INPUT_<NAME>` — each run input
  - `WF_STEP_<ID>_RESULT` — each completed step's result
  - `WF_RUN_DIR` — the run's shared workspace
  - `WF_OBSERVE_DIR` — observe convention dir (`<run_dir>/.observe`)
  - `WF_WORKFLOW_DIR` — the workflow's persistent dir

## Execution model

- The **executor** schedules the DAG with up to 5 concurrent steps; a step
  starts when all of its `dependsOn` are `done`. If a dependency fails, the
  step is `skipped` and the run ends `partial-failed`.
- **Agent steps** spawn an isolated one-shot session with the persona's
  config — no AgentPool, no shared history. The step result is the final
  agent message.
- **Script steps** spawn `sh -c` in their own process group (so cancel can
  kill the whole tree). The step result is stdout.
- **Cancel** stops dispatching new steps, aborts in-flight ones, and marks
  the run `cancelled`. A daemon restart marks unfinished runs `interrupted`.
- Run statuses: `running · done · partial-failed · cancelled · interrupted`;
  step statuses: `pending · running · done · failed · skipped`.

### Runtime settings

Persisted next to the run store and applied live (Settings → Workflows):

| Setting | Default | Meaning |
|---|---|---|
| `llmParallel` | `1` | How many **agent** steps may talk to the LLM at once. Keep 1 for single local models / providers that reject concurrent requests; waiting steps stay `pending` and their timeout starts only when they get a slot. |
| `agentRetries` | `1` | Extra attempts for an agent step that errored or returned no text. |

## HTTP API

| Route | Method | Purpose |
|---|---|---|
| `/api/workflows` | GET / POST | list definitions / create one |
| `/api/workflows/:name/definition` | GET / PUT / PATCH / DELETE | read / replace / patch / delete a definition |
| `/api/workflows/:name/run` | POST | start a run (body = inputs map) |
| `/api/workflows/runs` | GET | run history |
| `/api/workflows/runs/:id` | GET / PATCH / DELETE | run detail / rename / delete |
| `/api/workflows/runs/:id/cancel` | POST | cancel a running run |
| `/api/workflows/runs/:id/activity` | GET | live activity feed (per-step streaming log) |
| `/api/workflows/draft` | POST | **AI-draft**: describe the pipeline in natural language; a one-shot agent authors a full draft definition for the editor |
| `/api/workflows/settings` | GET / PUT | runtime settings above |

Run progress is also pushed over the WebSocket gateway, so the desktop app /
Web UI render the DAG, per-step status, observe outputs, and live activity in
real time.

## Authoring tips

- Start from the UI's **AI draft** (describe what you want → editable
  definition) rather than writing frontmatter by hand.
- Use `observe` on the steps a human should be able to eyeball — `inline`
  for short markdown, `artifact` + `from: { file: ... }` for full reports
  the step wrote into the workspace.
- The workspace persists **across runs** of the same workflow: steps can
  cache downloads or build on previous outputs; write into files rather than
  relying only on `{{steps.X.result}}` when data is large.
- Keep per-step `guidance` for constraints ("only real numbers, cite
  sources") and workflow-level `guidance` for tone/rules shared by all
  steps.
