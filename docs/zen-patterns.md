# Zen Patterns

A **pattern** is a named system prompt for one text transform: text in, text
out, **one LLM call, no tools, no loop**. Because the output always has the
same shape it can be stored, diffed between runs, and piped into a next step —
which a prompt retyped each time cannot.

The on-disk format is [Fabric](https://github.com/danielmiessler/fabric)'s, so
its library imports with no converter — and **255 of its patterns plus all 9
strategies are vendored into this repo** under `assets/patterns` /
`assets/strategies`, alongside 6 SenClaw wrote. A fresh install therefore has a
working library offline, on first launch. Provenance and how to re-vendor:
[`assets/patterns/NOTICE.md`](../assets/patterns/NOTICE.md).

Code: [`src/patterns/`](../src/patterns/) · HTTP:
[`src/gateway/ui_server/patterns.rs`](../src/gateway/ui_server/patterns.rs) ·
MCP: [`src/mcp/patterns_server.rs`](../src/mcp/patterns_server.rs) · UI:
`web/src/components/plugins/PatternsPanel.tsx` +
`desktop_app/lib/features/plugins/patterns_panel.dart`

## Pattern vs skill

|  | pattern | skill |
|---|---|---|
| what it is | text → text | teaches the agent to use tools |
| loop | no, one call | yes, until done |
| tools | none | Bash / Read / browser / MCP |
| output | fixed structure | whatever fits |
| cost | one request | a whole agent turn |

**Patterns are deliberately not skills.** [`src/skills/scan.rs`](../src/skills/scan.rs)
loads every skill it finds into one registry, and each contributes a name, a
description and `triggers` to the pre-turn matcher. A few hundred entries drown
the real skills (`web-research`, `agent-browser`) and flood the slash-command
namespace. Patterns get their own registry and reach the agent through **one**
bundled skill (`skills/pattern`) plus **four** MCP tools, no matter how many
are installed.

## Layout

```text
~/.senclaw/patterns/            # SENCLAW_PATTERNS_DIR
  sources.json                  # the ledger: where patterns come from, in priority order
  user/<name>/system.md         # the local source — always resolved FIRST
  sources/<id>/…                # git checkouts
  strategies/<name>.json        # reasoning wrappers, shared across every source
```

A pattern is a directory containing `system.md`, optionally `user.md`. A
directory without a `system.md` is not a pattern — Fabric's tree has stray
folders, and treating those as empty patterns would put unusable names in the
picker.

### Shadowing

Sources resolve in ledger order and **the first hit wins**, with `user` pinned
first. That one rule is what lets someone fix a pattern they dislike: save a
copy under the same name into `user` and it shadows the checkout, surviving
every later `git pull`. The list shows `shadowedIn` so "I edited it and nothing
changed" always has a visible cause.

A git source is **read-only** through the API. Writing into a checkout would be
reverted by the next sync, and the user would have lost work with nothing to
show for it.

## Rendering

```text
system.md ─┬─ {{var}} substitution ─┬─ + strategy prompt ─ + language rule ─▶ system
user.md ───┘                        └─────────────────────────────────────▶ user
input ────────────────────────────────────────────────────────────────────┘
```

Two conventions come from Fabric and are load-bearing:

- **`{{input}}` is where the text goes when the pattern says so.** Most
  patterns end with a bare `# INPUT:` and expect the input as the *user
  message*; a minority interpolate `{{input}}` mid-prompt. Handling both from
  one call is what makes 250 patterns work without per-pattern code. The input
  is **never sent twice** — for a long transcript that is a doubled bill and a
  truncated context.
- **An unknown `{{placeholder}}` is left verbatim**, never blanked, the same
  rule [`src/scaffold/`](../src/scaffold/) follows. It comes back in
  `unresolved` so a caller can say which variable is missing.

### Strategies

Two fields, and that is the point — *how to think* separated from *what to do*,
so one `cot.json` applies to every installed pattern:

```json
{ "description": "Chain-of-Thought (CoT) Prompting",
  "prompt": "Think step by step to answer the question. Return the final answer in the required format." }
```

Not to be confused with `adaptive_thinking` in
[`src/zen_core/query_llm.rs`](../src/zen_core/query_llm.rs): that sets the
model's thinking *budget*, this sets the *method* and is plain prompt text.

## HTTP

| route | does |
|---|---|
| `GET /api/patterns` | resolved list + sources + strategies |
| `GET /api/patterns/:name` | one pattern's files and the source it resolved to |
| `POST /api/patterns/run` | render, and unless `dryRun` execute |
| `POST /api/patterns` | create/overwrite in a writable source |
| `POST /api/patterns/import` | multipart zip of pattern folders |
| `DELETE /api/patterns/:name` | delete from a writable source |
| `GET`/`POST /api/patterns/sources` | list / add a git source |
| `POST /api/patterns/sources/:id/sync` | clone or pull |
| `POST /api/patterns/sources/:id/toggle` | enable/disable without deleting |
| `DELETE /api/patterns/sources/:id` | de-register and delete its files |

## MCP — four tools, not four hundred

`senclaw-patterns`, prefix `pattern_`:

- `pattern_list` — find the name. Hundreds exist; the agent must not guess.
- `pattern_get` — the **rendered prompt**, no model call. The agent follows it
  inside the turn it is already having. Free.
- `pattern_run` — a separate one-shot call in a clean context. Use when the
  output must be reproducible or must not inherit the conversation.
- `pattern_sync` — refresh a source from git. Slow.

Like [`ocr_server`](../src/mcp/ocr_server.rs) the subprocess owns no state: it
calls the daemon over loopback. The registry, the LLM config resolution and the
git checkouts all live in the daemon, and a second copy would race the first.

## Typing a pattern name in chat

`/summarize` and `#extract_wisdom` work with no new syntax: the composer
directive pass ([`src/agent/prompt_directives.rs`](../src/agent/prompt_directives.rs))
already resolved `/name` against the skill list, and now falls through to the
pattern list when nothing matches. The expansion appends a reminder telling the
agent to fetch the rendered prompt with `pattern_get` and follow it.

**Skills win a shared name.** The skill set is small and curated; a library of
several hundred pattern names must never take a skill's token away.

The lookup uses `PatternRegistry::names`, not `list` — `list` reads every
`system.md` to build descriptions, which is 255 file reads on a path that runs
for every message containing a slash.

## The catalog

`GET /api/patterns/catalog` is what the "add a source" screen offers before the
user types anything, and `POST /api/patterns/catalog/:id/install` installs one.
Three entries ([`src/patterns/catalog.rs`](../src/patterns/catalog.rs)):

| id | what | network |
|---|---|---|
| `senclaw` | the 261 bundled patterns + 9 strategies | none — compiled in |
| `senclaw-git` | the same tree from SenClaw's own repo (`assets/patterns`) | clone |
| `fabric` | Fabric upstream, pinned at the vendored tag | clone |

The bundled entry exists because the first version of the add-source dialog was
five blank fields, and filling them correctly required having already read
someone else's repository layout — Fabric keeps patterns in `data/patterns` and
strategies in `data/strategies`, which nobody should have to know.

`BUNDLED_PATTERNS` and `BUNDLED_STRATEGIES` are walked out of `assets/` by
`build.rs`, not hand-listed: re-vendoring replaces the tree wholesale, so a list
would be wrong within one update — and wrong in the silent direction, where a
file is in the repo, missing from the binary, and nobody notices until someone
is offline.

Bundled patterns install into a **`senclaw` local source**, not `user`, so the
user's own copy of a name still wins and uninstalling is a directory delete.

## Kits

A Zen Kit can ship patterns two ways ([docs/zen-kits.md](zen-kits.md)):

```jsonc
"patterns": [{ "name": "summarize", "system": "# IDENTITY…" }],
"patternSources": [{
  "id": "fabric",
  "url": "https://github.com/danielmiessler/fabric",
  "ref": "v1.4.470",             // pin a TAG — see "Trust" below
  "subdir": "data/patterns",
  "strategiesSubdir": "data/strategies",
  "syncOnInstall": true
}]
```

Inline patterns land in a **kit-owned source** (`kit-<id>`), never in `user`:
uninstall becomes a directory delete that cannot take a hand-written pattern
with it, and a kit pattern never silently outranks one the user wrote.

The installer only **registers** git sources; cloning happens in the HTTP layer
(`sync_kit_pattern_sources`), for the same reason Space App installs do — it is
network I/O, and keeping it out of the installer keeps `cargo test` offline.

The bundled **Fabric Patterns** kit ([`assets/kits/fabric.json`](../assets/kits/fabric.json))
is offered in Plugins → Kits with no marketplace configured, because its whole
payload is a git URL and a pinned ref. See
[`src/kits/builtin.rs`](../src/kits/builtin.rs) — that module is for kits whose
payload is a *reference*, not a place to bundle content.

## Trust

**A pattern is placed in the system-prompt position of a real LLM call.**
Following a moving branch therefore lets an upstream commit silently rewrite
instructions the agent then obeys — plain prompt injection, with the repo owner
as the injector.

Nothing in the daemon can decide that for the user, so the sync does two things
instead: it records what it fetched, and `SourceSyncOutcome::pinned` reports
whether the ref is a tag/sha or a branch. The UI shows unpinned as the risk it
is, and the "add source" dialog says so before the fact.

Names are the other hostile input — they arrive from a git directory listing, a
kit manifest and a UI field. `store::sanitize_name` is the single choke point
that keeps `../../.ssh` out of a path, and the zip importer only ever writes
`system.md`/`user.md` under one sanitized directory component.

## Traps

- **Fabric patterns are written in English** and most pin the output language
  in `# OUTPUT INSTRUCTIONS`. Without `language: "auto"` a Vietnamese user
  feeding Vietnamese text gets an English summary. The rule is appended at
  *render* time, after the pattern's own instructions so it wins — never
  patched into the file, which the next sync would revert.
- **`{{input}}` decides whether the input is also the user message.** A pattern
  that interpolates it gets an empty user message on purpose.
- **A `subdir` is joined component by component with `..` dropped**, so a
  hand-edited `sources.json` cannot point the scanner outside the checkout.
- **`sources.json` failing to parse reads as "just the user source"**, not as
  an error — losing the ledger costs the git sources, erroring out would take
  the whole patterns API down. Same trade as
  [`src/kits/receipt.rs`](../src/kits/receipt.rs).
- **The checkout is shallow, and a refresh re-clones.** Depth 1: nothing reads
  a pattern's git log, and the measurement is not marginal — a full
  `danielmiessler/fabric` clone took **402 s** against **32 s** shallow, same
  255 patterns and 9 strategies. `source::fetch` falls back to a full clone
  when the shallow one fails, because a raw-sha `ref` and a server with shallow
  fetches disabled both legitimately need history.
- **Strategies are global, not per-source.** They are two-line wrappers with
  conventional names; one `cot` is the useful outcome, not one per repo. Import
  skips a name that already exists.
