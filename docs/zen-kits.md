# Zen Kits

A **kit** is one JSON manifest that installs a working setup in a single call:
personas, skills, workflows, hooks and scheduled jobs. Think of it as a Space
App bundle without the app — the same `personas` + `skills` idea from
`senclaw-manifest.json`, plus the two things an app bundle cannot carry
(scheduled work and hooks), plus workflows.

Kits used to be installed by the client (the Flutter app drove `/api/subagents`,
`/api/background/tasks` … one call at a time). That put install ordering, the
never-overwrite rule and the removal ledger in every client that wanted kits.
The daemon owns all of it now; a client posts a manifest and reads a report.

Code: [`src/kits/`](../src/kits/) · HTTP: [`src/gateway/ui_server/kits.rs`](../src/gateway/ui_server/kits.rs)

## Manifest

```jsonc
{
  "manifest": 2,                    // absent = v1 (agents + jobs only)
  "id": "daily-report",             // required, unique per daemon
  "name": "Daily Report Kit",
  "version": "1.1.0",
  "description": "…",

  "agents": [{
    "name": "Zen Daily Reporter",   // registered name — jobs point at this
    "description": "…",
    "systemPrompt": "…",            // `prompt` also accepted (v1 spelling)
    "tools": ["Read", "Bash"],      // optional allowlist
    "maxConcurrent": 2
  }],

  "skills": [{
    "name": "zen-report-format",
    "description": "…",
    "content": "# …",               // SKILL.md body; front-matter added if absent
    "triggers": ["report"]
  }],

  "workflows": [{
    "name": "zen-morning",
    "content": "---\nname: zen-morning\n…\n---\n…"   // complete workflow .md
  }],

  "hooks": [{
    "event": "PreToolUse",          // see "Hooks" below for the accepted list
    "matcher": "Bash",              // glob over the tool name; absent = all
    "if": "rm -rf",                 // regex over the tool input; absent = all
    "prompt": "…",                  // prompt hooks only — never shell
    "timeout": 30,
    "blocking": false
  }],

  "jobs": [{
    "name": "Báo cáo sáng 09:00",
    "agentRef": "Zen Daily Reporter",
    "cron": "0 9 * * *",            // `cronExpression` also accepted
    "input": "…",
    "maxFailures": 5,
    "enabledOnInstall": true        // false installs the task paused
  }],

  "patterns": [{                    // prompt patterns — see docs/zen-patterns.md
    "name": "tom-tat-hop",
    "system": "# IDENTITY and PURPOSE\n…",   // `content` also accepted
    "user": "…"                     // optional user.md template
  }],

  "patternSources": [{              // a git library of patterns, e.g. Fabric
    "id": "fabric",
    "url": "https://github.com/danielmiessler/fabric",
    "ref": "v1.4.470",              // pin a TAG: a pattern becomes a system prompt
    "subdir": "data/patterns",
    "strategiesSubdir": "data/strategies",
    "syncOnInstall": true           // default — clone during install
  }],

  "params": [{                      // asked before install — see "Params"
    "key": "workdir",               // fills {{param.workdir}}
    "label": "Working folder",
    "type": "folder",               // string | number | boolean | select | folder
    "required": true,
    "default": "~/Projects"
  }],

  "mcpServers": [ … ],              // parsed, NOT installed — see below
  "apps": [ … ]                     // parsed, NOT installed — see below
}
```

`manifest: 1` (or the key absent) means the file predates everything except
`agents` and `jobs`; the other lists are ignored there, because a v1 author had
no such fields to mean anything by. A manifest declaring a version newer than
this build is refused with HTTP 422 and "update SenClaw" rather than installed
half-understood.

## HTTP API

| Route | Does |
|---|---|
| `GET /api/kits` | kits installed on this daemon, from the receipt ledger |
| `POST /api/kits/preview` | parse + validate, report counts, params and warnings, install nothing |
| `POST /api/kits/install` | install; returns a per-item report |
| `DELETE /api/kits/:id` | remove what the receipt says this kit created |

The body of `preview`/`install` may be the manifest itself, or wrapped as
`{"manifest": {...}}` or `{"kit": {...}}` — clients disagree and a 400 over a
wrapper is a miserable thing to debug.

```bash
curl -X POST localhost:18788/api/kits/install \
     -H 'Content-Type: application/json' -d @kit.json
```

```jsonc
{
  "ok": true,
  "report": {
    "kitId": "daily-report",
    "items": [
      { "type": "agent",    "name": "Zen Daily Reporter", "status": "created" },
      { "type": "workflow", "name": "zen-morning",        "status": "skipped",
        "detail": "a workflow with that name already exists" }
    ],
    "warnings": []
  }
}
```

## Params

A kit that hardcodes the folder it runs in, the API key it talks to, or how many
times a workflow repeats is a kit only its author can install. `params` turns the
manifest into a template: the client renders a form, the daemon substitutes the
answers, and nothing reaches disk half-templated.

| `type` | control | notes |
|---|---|---|
| `string` | text field | `secret: true` renders masked |
| `number` | number field | `min` / `max` / `step` are enforced daemon-side too |
| `boolean` | switch | accepts `true`/`false`, and the `"true"`/`"1"`/`"yes"` spellings a form sends |
| `select` | dropdown | `options: [{value, label}]`; an empty list is refused at parse time |
| `folder` | path field + picker | a `string` with a native folder chooser beside it |

Every field beyond `key` is optional: `label` (falls back to `key`),
`description`, `placeholder`, `default`, `required`, `secret`.

Answers ride along with the manifest, in the wrapper:

```bash
curl -X POST localhost:18788/api/kits/install -H 'Content-Type: application/json' \
     -d '{"manifest": {…}, "params": {"workdir": "/Users/me/reports", "runs": 3}}'
```

`params` is deliberately overloaded on the wire — a *declaration array* inside the
manifest, an *answer object* in the wrapper — and the daemon keys on the JSON
type. That is what lets a bare manifest carrying its own `params: []` still post
without a wrapper.

`POST /api/kits/preview` returns the declarations (so a client can render the
form) plus `paramError`: the answers it was given, validated, without installing
anything. Every problem is reported at once, so a form with three empty required
fields lights up three fields instead of revealing them one install at a time.

### Substitution

Placeholders are namespaced — `{{param.workdir}}`, never bare `{{workdir}}` —
because skill and workflow bodies are Markdown that legitimately contains other
`{{…}}` syntax. Substitution covers every string in the kit, including
`mcpServers` and `apps`: the daemon does not install those, but the client that
does would otherwise get an entry still carrying `{{param.apiKey}}`.

Three rules of its own:

* **One pass over the whole manifest.** `agents[].name` and the `jobs[].agentRef`
  pointing at it are rewritten together — the persona registry keys on that name,
  so rewriting one without the other installs a job that runs with no persona.
* **A declared param left blank substitutes as empty**, so a field the user
  never touched and one they cleared land identically (the form sends nothing
  for one and `""` for the other). Declare a `default` if you want a fallback —
  that is what it is for.
* **An *undeclared* placeholder stays verbatim**, and `preview` warns about it
  (`undeclaredParam`). That case is an author's typo, and blanking it would hide
  the mistake instead of surfacing it.
* **A param key that cannot appear in `{{param.<key>}}`** — anything outside
  letters, digits, `_` and `-` — is refused at parse time rather than installed
  as a placeholder nothing can ever fill.

### Secrets

`secret: true` marks a credential. It renders masked, and it is **left out of the
receipt**: `installed.json` is plain JSON in `~/.senclaw/kits`, and an API key
belongs in it no more than in a log. Non-secret answers are kept (minus blank
ones, which are noise), so the UI can show what a kit was installed with. This is presentation and hygiene, not a
security boundary — the substituted value still lives wherever the kit put it.

## Where things land

| Item | Location |
|---|---|
| agent | `<virtual_agents_dir>/kit-<kit>__<name>.md` |
| skill | `<managed_skills_dir>/<name>/` + `.senclaw-kit.json` marker |
| workflow | `<workflows_dir>/<name>.md` |
| hook | `<kits_dir>/hooks/<kit>.json` |
| job | a `background_tasks` row, `owner_kind=app`, `owner_id=<kit>` |
| receipt | `<kits_dir>/installed.json` (plus the non-secret param answers) |

`kits_dir` defaults to `~/.senclaw/kits` (`SENCLAW_KITS_DIR`).

The `kit-<id>__` filename prefix marks ownership **only**. A persona registers
under its front-matter `name:`, and background tasks resolve `persona` against
that same key — so the name in the file must stay verbatim. (A client that
wrote the slugged filename into the job instead produced jobs that ran with no
persona at all, with nothing but a warning in the log to show for it.)

## Three rules

1. **Never overwrite.** An item whose name is taken is `skipped`, whoever put
   it there. Persona collisions are detected by reading the front-matter name,
   not the filename, because that is what the registry keys on. Reinstalling a
   kit therefore never undoes an edit — including an edit to the kit's own hook
   file.
2. **Never stop halfway.** Every item reports an outcome even after an earlier
   failure. A half-installed kit the user can see beats an opaque error.
3. **Only remove what was created.** Skipped items never enter the receipt, so
   uninstall cannot delete something the user made. If any removal fails the
   receipt is kept — it is the only record of what is still out there.

## Authoring traps

Two things bite kit authors, both found by installing real kits and reading
back what the daemon actually stored.

### Cron day-of-week is Quartz-style, not Unix

`jobs[].cron` goes through the [`cron`](https://docs.rs/cron) crate, which
numbers the day-of-week field **1 = Sunday … 7 = Saturday** — not the Unix
convention where `0`/`7` is Sunday and `1` is Monday.

| Day | Mon | Tue | Wed | Thu | Fri | Sat | Sun |
|---|---|---|---|---|---|---|---|
| value | 2 | 3 | 4 | 5 | 6 | 7 | 1 |

`0 8 * * 1` fires **Sunday** 08:00. Nothing rejects a wrong value, because
every value in range is a valid cron — the job simply runs on a different day
than the label promised. Five-field expressions are accepted and a seconds
field is prepended internally; times are the engine host's local timezone.

### Front-matter values are quoted for you

Personas and skills are Markdown with a YAML front-matter block, and the skill
scanner parses it with a real YAML parser. A perfectly ordinary description —
`Khung brainstorm: 5W, 6 mũ` — would open a nested mapping and take the whole
block down with it: the skill still installs, but its description and triggers
come back empty, so nothing ever matches it.

`persona_markdown` and `skill_markdown` therefore quote any value where YAML
would read it differently (a `: ` inside it, a leading `-`, a `#`, padding,
newlines) and leave plain values plain. Authors do not need to know YAML's
punctuation rules; a round-trip test covers the awkward cases.

## Hooks

Kit hooks live in their own file per kit, handed to the hook loader through the
same `extra_files` slot marketplace plugin hooks use
([`agent::hook_config_loader`](../src/agent/hook_config_loader.rs)). Two
consequences, both intended:

* **Uninstall is a delete.** A kit never edits the user's `hooks.json`, so
  removing one cannot mangle hooks written by hand.
* **A kit can only ever register a prompt hook.** The manifest has no field for
  a command hook and the writer emits `"type": "prompt"` unconditionally. A
  command hook is `sh -c` at daemon privilege — supply-chain RCE plus a
  restart-surviving foothold — and that is not something a one-tap install
  should be able to arrange. This mirrors the marketplace policy, which still
  gates the load side and is now expressible per package:
  `SENCLAW_MARKETPLACE_COMMAND_HOOK_PLUGINS=<name>` admits one package whose
  scan report the operator has read, while
  `SENCLAW_ALLOW_MARKETPLACE_COMMAND_HOOKS=true` admits every installed plugin
  at once — including ones installed later that nobody has looked at, which is
  why the pre-install scan treats *that* setting as the blocking one.

Accepted events: `UserPromptSubmit`, `PreToolUse`, `PostToolUse`,
`PermissionRequest`, `Stop`, `SessionStart`, `PreCompact`, `PostCompact`. An
entry naming anything else is dropped with a warning and the rest of the kit
still installs; if *nothing* survives, the hook item fails rather than writing
an empty file that would claim hooks were installed.

### Hooks now actually run

Worth stating plainly, because it changes existing behaviour: the hook
subsystem was fully built — matching, command and prompt executors, PreToolUse
gating — but nothing ever fed it a config. Every engine started with
`HookManager::empty()` and `load_zen_hook_config` had no call site, so
`~/.senclaw/hooks.json` sat on disk doing nothing.

`ZenEngine::new` now calls `ZenEngine::reload_hooks()`, which loads the user's
global and workspace `hooks.json` plus every installed kit's hook file. On a
daemon with no hooks anywhere the manager stays empty, exactly as before; on one
with a `hooks.json`, those hooks start firing.

## What the daemon does not install

`mcpServers` and `apps` are parsed, counted in `preview`, and reported as
`unsupported` with a pointer to the right endpoint. They belong to subsystems
with their own consent flow — installing an app pulls a whole program onto the
machine, and doing that as a side effect of "install kit" is not a decision the
daemon should make on the user's behalf. A client drives them through
`/api/mcp-servers` and `/api/marketplace/hub/install`.
