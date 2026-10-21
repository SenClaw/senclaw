# Skill history, the write-path scan and the usage ledger

Every change to a skill that goes through SenClaw leaves a **revision**; `create` and `install` run the security scan on the content **before** it is written; a skill that replaces another source's skill of the same name says so. Code: [`src/skills/history.rs`](../src/skills/history.rs), [`src/db/skill_history.rs`](../src/db/skill_history.rs), handlers in [`src/gateway/ui_server/skills.rs`](../src/gateway/ui_server/skills.rs), scanner entry `security::scan::scan_skill_dir`. Plan and what is left: [`plans/261021-1058-skill-history-p0/plan.md`](../plans/261021-1058-skill-history-p0/plan.md).

No model is called anywhere in this. It is the groundwork for letting a skill be changed automatically later: there is a baseline to compare against, a scan every write passes, and a way back.

## What is recorded

One row in `skill_revisions` per change, plus the content on disk under `<managed skills dir>/../skill-history/<source>/<skill dir>/<revision>/`.

| Field | Meaning |
|---|---|
| `source`, `skillKey` | Where the skill lives (`clawhub-managed`, `bundled`, …) and its **directory name**. Two sources can hold the same skill name, and a hub slug need not equal the frontmatter name, so history is keyed by directory. |
| `origin` | `MANUAL` (API create, overwrite, readme save) · `IMPORTED` (hub install) · `ROLLBACK` · `REMOVED` · `DETECTED` (found changed on disk, or captured just before the daemon overwrote it) · `BASELINE` (first sighting) |
| `parentId`, `generation` | The previous revision of the same skill. |
| `contentHash` | SHA-256 over file names, bytes and the executable bit. |
| `changeSummary`, `diff` | `1 changed, 1 added (+14 -3)`; unified diff against the parent (capped at 64 KiB; a binary file is named, not diffed). |
| `scan` | Verdict, max severity, risk score, findings (rule, severity, title, file, line). **Never the matched text** — it can be the secret the rule found. |
| `shadows` | The lower-precedence source this skill replaces, when it does. |
| `hasSnapshot`, `truncated` | Whether the content is still on disk; whether a size cap left something out (such a revision is never restorable). |

A snapshot is the skill's own files: no `.clawhub/` provenance, no `.env*`, no `.git` / `node_modules` / `__pycache__` / `target` / `.venv`, no symlinks. Caps: 200 files, 2 MiB per file, 10 MiB total. The newest 30 snapshots per skill keep their content; older rows stay and say `hasSnapshot: false`.

Recording the same content again records nothing. Removing a skill records `REMOVED` and keeps the content of the revision before it, so a removal can be undone.

## Where a change is recorded

| Path | Recorded as | Scan |
|---|---|---|
| `POST /api/skills/create` (incl. `overwrite`) | `MANUAL`, after a `DETECTED` capture of what was there | **Enforced**: the new `SKILL.md` is scanned first; a block at the configured level is a 403 and nothing is written |
| `POST /api/skills/install` (hub) | `IMPORTED`, after a `DETECTED` capture of the old install | **Enforced**: extracted to `.staging-*` beside the target, scanned, then swapped in; a block leaves the installed skill untouched |
| `PUT /api/skills/:name/readme` | `MANUAL`, after a `DETECTED` capture | Reported, **not refused** (a person typing in the editor) — the verdict is in the response |
| `DELETE /api/skills/:name` | `DETECTED` capture, then `REMOVED` | — |
| `POST /api/skills/:name/rollback/:rev` | `ROLLBACK` (`restoredFrom`) | Enforced on the restored content |
| CLI installs, kit installs, hand edits, git pulls | `DETECTED` / `BASELINE` / `REMOVED` by the reconcile pass | Stored on the revision, nothing refused |

The **reconcile pass** walks the managed directory and compares each skill with its newest revision. It runs at daemon start and, in the background at most every 30 s, after `GET /api/skills`. Skills carrying `.senclaw-app.json` (installed by a Space App or kit) are left alone: they belong to the app.

## REST

| | |
|---|---|
| `GET /api/skills/:name/history?source=&limit=` | Revisions, newest first, without diffs. `:name` is the live skill's name, else the managed directory's entry of that name (how a removed skill is reached). |
| `GET /api/skills/:name/history/:rev` | One revision with its diff. |
| `POST /api/skills/:name/rollback/:rev` | Restore that revision. Managed skills only (bundled skills are re-unpacked on every build; `app:*` belong to their app). 409 when the revision has no content (`REMOVED`, pruned, truncated, or damaged on disk). |
| `GET /api/skills` | Each entry gains `shadows: ["bundled", …]`. |
| create / install / readme responses | Gain `revision`, `scan`, and (create, install) `shadows`. |

## Scan policy

The existing knobs apply: `SENCLAW_SCAN_BEFORE_INSTALL=false` turns the scan off here too; `SENCLAW_SCAN_BLOCK_LEVEL` (default `critical`) sets what is refused. `SKILL.md` is read as a prompt file (instruction-override, forged system framing, hidden characters, `allowed-tools` claiming Bash/Write/WebFetch, read-then-send worms); helper scripts get the code rules. Fenced code inside `SKILL.md` is **not** read as code: a skill that documents `curl … | sh` installers would be a Critical block.

Measured 2026-10-21 on the 229 skills installed on the maintainer's machine (managed, `~/.claude/skills`, the repo's bundled set): 214 allow, 14 warn, **1 block** (`bot-channels`, `CRED002`: it documents editing `~/.senclaw/config.json`). A Critical forged-framing rule (`INJ002`) also fires on a line starting `Human:` / `Assistant:`, which an AI-written few-shot example can contain — if that turns up in practice, the answer is an explicit override on `create` (not built), not a lower block level.

## Rules for Claude

- **History is a safety net, never a gate on the operation it records.** A failed recording is a `tracing::warn!` and the write proceeds (`with_history`). The scan is the only thing that refuses, and only on `create`, `install` and `rollback`.
- **Scan before writing, not after.** `create` scans the text in a throw-away directory; `install` extracts beside the target and swaps with `rename`. Writing first and cleaning up leaves a window where a blocked skill is loadable.
- **A recorded finding carries no matched text.** `scan_summary` drops `evidence` on purpose.
- **A stored snapshot is checked against its row's hash on load.** A mismatch is an error, not a restore of whatever is on disk.
- **Restore syncs only what a snapshot covers.** Files a snapshot never holds (`.clawhub/`, `.env*`, dependency folders) survive a rollback; files inside its scope that it lacks are removed.
- **Never point a rollback at a directory the API did not resolve.** `managed_skill_dir` accepts one plain path component; a key from a request or a row is never joined onto the managed directory without it.
- **Revisions are never deleted, only their content.** `rolled back` and `removed` are rows too.
- **The usage ledger never closes a row on a new input.** A person's message is fired on a different path than a forced load and can reach the ledger after the load it belongs with; only a final answer, an error or an interruption closes rows (`op_for`).
- **Out-of-band edits are found, not hooked.** Do not add a record call to each writer (CLI, kits); the reconcile pass covers them, and an app's own skills are excluded by marker.

## Usage ledger

The history says what a skill *is*; the ledger says how it *behaves*. One row in `skill_usage` each time a skill's instructions are loaded into a chat — the model calls the `Skill` tool (`how: tool`), or the engine loads it (`#name`, `/name`, pre-trigger, the router: `how: forced`). Skills that are always on are not loads and are not recorded. Code: [`src/skills/usage.rs`](../src/skills/usage.rs), [`src/db/skill_usage.rs`](../src/db/skill_usage.rs), hooks in `zen_core::engine` (`note_load`, two sites) and `agent_pool::engine` (`record`, beside the failure ledger).

A row is opened at the load and closed by the chat's own events: `completed` (the main agent's final answer), `errored` (a session error; `loopStopped` when it was a tool-error loop), `interrupted`. `toolFailures` counts failing tool calls while the row was open, and `firstFailedTool` names the first. A row a stopped daemon left open becomes `unknown` at the next start. `contentHash` is the hash of the skill directory at load — the same hash a revision carries — so the numbers split by version without a join key of their own.

| | |
|---|---|
| `GET /api/skill-usage?skill=&limit=` | Recent loads, newest first (`skill` = directory key or name). |
| `GET /api/skill-usage/summary?days=30` | Per skill and per content version: loads, completed / errored / interrupted / unfinished, `withToolFailures`, `fallbackRate` (of loads whose turn ended, the share that errored or was interrupted) and `toolFailureRate`. Each version names its revision when the history has one. Rates are `null` until a load has ended. |

It is not under `/api/skills/…` on purpose: that path has a `:name` capture, and a skill called `usage` would answer a `DELETE` with 405.

**It only records.** Nothing reads it back into a prompt, and no proposal comes out of it. Whether any skill fails often enough, and repeatedly enough, for an automatic change to be worth building is a question for `summary` after a week or two of real use — the same gate the failure ledger sets. A failing tool in a turn where a skill loaded is not evidence the skill is at fault; the ledger counts, it does not attribute.

## Clients

All three show the same things: the revision timeline (kind, who, when, change summary, scan badge, "overrides …" badge, the ledger's numbers per version), a revision's diff and scan findings, **Restore** with a confirmation (disabled, with the reason, for the current revision, a removal, a pruned or truncated one, and any skill outside the managed directory), and an "overrides bundled" chip on the skill itself.

| Client | Where |
|---|---|
| Web (`web-app`) | Plugins → Skills → a skill → **History** tab (`components/plugins/SkillHistoryPanel.tsx`, `lib/skillHistoryApi.ts`); saving `SKILL.md` warns with the scan's rules |
| Desktop (`desktop`) | Plugins → Skills → the History button of a row (`features/plugins/skill_history_dialog.dart`) |
| SenClaw Connect (`senclaw-connect`) | Plugins → Skills → the History button of a row (`screens/plugins/skill_history_screen.dart`) |

## Not done (see the plan)

No refusal when a skill shadows a bundled one: it is flagged only, pending a decision on whether that override is intended. No override flag on `create` for a scan that blocks a legitimate skill. A skill's own page does not yet show the usage numbers outside the History view.
