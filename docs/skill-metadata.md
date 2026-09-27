# Skill metadata (OpenClaw-compatible)

SenClaw skills live in a directory containing a `SKILL.md` = YAML frontmatter
+ markdown body. The frontmatter is parsed by [`src/skills/metadata.rs`] using
`serde_yaml`, so both nested YAML and OpenClaw's single-line JSON form are
accepted. This document is the canonical schema.

## Frontmatter fields

### Core (Anthropic/Claude)

| Key | Type | Default | Meaning |
|---|---|---|---|
| `name` | string | dir name | Unique skill identifier. |
| `description` | string | — | One-line summary shown to the model. |
| `allowed-tools` | list \| csv string | `[]` | Tools to prioritise while the skill runs. Also accepts `allowed_tools` / `allow_tools`. |
| `when-to-use` | string | — | Guidance on when the skill applies. |
| `model` | string | — | Force a model while the skill is active. |
| `max-thinking-tokens` | int | — | Extended-thinking budget. |
| `disable-model-invocation` | bool | `false` | Keep the skill out of the model prompt (still user-invocable). |
| `argument-hint` | string | — | Free-text hint about expected arguments. |
| `version` | string | — | Semantic version. |

### OpenClaw additions

| Key | Type | Default | Meaning |
|---|---|---|---|
| `triggers` | list \| csv string | `[]` | Keywords that hint when the skill applies. Also accepts `trigger`. |
| `user-invocable` | bool | `true` | Expose the skill as a user slash command. |
| `command-dispatch` | string | — | `tool` routes a slash command straight to a tool. |
| `command-tool` | string | — | Tool invoked when `command-dispatch: tool`. |
| `params` | list of objects | `[]` | Custom argument schema (see below). SenClaw convention. |
| `metadata.openclaw` | object \| JSON string | — | Load-time gating block (see below). Aliases: `metadata.clawdbot`, `metadata.clawdis`. |

### `params`

A list of argument descriptors, surfaced to the model when the skill activates:

```yaml
params:
  - name: city
    type: string
    required: true
    description: The city to query
  - name: units
    type: string
```

Each item: `name` (required), `type` (default `string`), `required` (default
`false`), `description` (optional).

### `metadata.openclaw` — load-time gating

Mirrors OpenClaw. A skill that fails any gate is **excluded at load time** (a
reason is logged). Supports two equivalent shapes.

Nested YAML:

```yaml
metadata:
  openclaw:
    os: [darwin, linux]
    primaryEnv: GEMINI_API_KEY
    requires:
      env: [GEMINI_API_KEY]
      bins: [uv]
      anyBins: [curl, wget]
      config: [browser.enabled]
```

Single-line JSON (OpenClaw's embedded-parser form — also accepted here):

```yaml
metadata.openclaw: '{"os":["darwin"],"requires":{"bins":["jq"],"env":["API_KEY"]}}'
```

| Path | Gate |
|---|---|
| `os` | Host OS must match. Accepts `darwin`/`macos`, `win32`/`windows`, `linux`. |
| `requires.env` | All listed env vars must be set (non-empty). |
| `requires.bins` | All listed binaries must be on `PATH`. |
| `requires.anyBins` | At least one listed binary must be on `PATH`. |
| `requires.config` | Declarative only — **not** gated (no global `openclaw.json`). |
| `primaryEnv` | Names the skill's main credential; filled from config `apiKey`. |

## Env injection (`config.json`)

Frontmatter only *declares* what a skill needs. Values are provided in the
global `config.json` (path = `PathsConfig::global_config_path`), under
`skills.entries.<name>`, mirroring OpenClaw's `skills.entries.*`:

```json
{
  "skills": {
    "entries": {
      "image-lab": {
        "enabled": true,
        "apiKey": "sk-...",
        "env": { "GEMINI_API_KEY": "..." }
      }
    }
  }
}
```

On activation ([`SkillTool`]), each `env` var is injected into the process
**only if not already set**, and `apiKey` is written to the skill's
`primaryEnv`. Injection is scoped to the host process (single-process daemon);
unlike OpenClaw it is not sandbox-isolated per agent run.

## Where it lives

- [`src/skills/metadata.rs`] — schema, frontmatter parsing, eligibility gates.
- [`src/skills/config.rs`] — `skills.entries.*` config + env injection.
- [`src/skills/scan.rs`] — scanning; carries parsed metadata + `eligible` flag; runtime loaders drop ineligible skills.
- [`src/tools/skill.rs`] — activation: env injection + params surfaced to the model.
- [`src/cli/commands/skills.rs`] — `skills info`/`list` surface triggers, eligibility, requirements, params.

[`src/skills/metadata.rs`]: ../src/skills/metadata.rs
[`src/skills/config.rs`]: ../src/skills/config.rs
[`src/skills/scan.rs`]: ../src/skills/scan.rs
[`src/tools/skill.rs`]: ../src/tools/skill.rs
[`src/cli/commands/skills.rs`]: ../src/cli/commands/skills.rs
[`SkillTool`]: ../src/tools/skill.rs
