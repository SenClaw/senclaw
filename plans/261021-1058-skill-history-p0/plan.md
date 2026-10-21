# Skill history + write-path scan (P0a)

> Status: IMPLEMENTED, uncommitted · 2026-10-21 · branch `feat/skill-history` (from main 32863d4)
> Source: `SenClaw/plans/reports/research-261021-1020-openspace-skill-evolution-system-one-gate.html` §6–§8 (P0).
> Scope here = the part of P0 that needs no model call and none of the open decisions: history, write-path scan (P0a) and the usage ledger (P0b). P1+ (proposer, System One review, apply) are separate.

## Outcome

Every change to a skill that goes through SenClaw leaves a revision: content snapshot, diff against the previous revision, scan result, and whether it shadows a lower-precedence skill of the same name. A person can list history, read a diff and roll back over REST. `create` / `install` refuse content the existing security scanner blocks (policy from `config.security`).

## Constraints

- No model call. No change in behaviour for a skill that scans clean, except the extra fields in responses.
- History is its own store (`skill_revisions` table + `<senclaw_home>/skill-history/`), bounded (30 snapshots per skill, size caps), 0700/0600 on unix.
- Out-of-band edits in the managed dir (CLI, kits, hand edits) are caught by a reconcile pass, not by hooking each writer.
- Never write to bundled/app skills in place; rollback restores into the dir the revision came from, and refuses `app:*`.
- Test with `--lib` against the shared target; never rebuild `target/debug/senclaw` (live desktop app re-execs it).

## Non-goals

UI in web / desktop / channel_app (REST fields only in the daemon change; the clients are separate branches, see "Clients" below). Refusing a shadowing create (flag only; decision pending). Usage ledger. Evolution, proposer, System One review. Scanning fenced code inside SKILL.md as code (needs calibration; existing prompt-injection rules still apply to SKILL.md).

## Design

| Piece | Where |
|---|---|
| Scan a skill dir | `security::scan::scan_skill_dir` (`TargetKind::Skill`), reuses the rule table |
| Snapshot, hash, diff, restore, prune | `src/skills/history.rs` (fs only) |
| Rows | `src/db/skill_history.rs`, tables in `db/schema.rs` |
| Shadow map | `skills::scan::load_all_local_skills_with_shadows` |
| Hooks | `gateway/ui_server/skills.rs`: create, readme save, install, uninstall + new history / rollback routes |
| Out-of-band | `history::reconcile_managed` at daemon start and (throttled, background) after `GET /api/skills` |

Revision key = `(source, skill_name)`. `origin`: `MANUAL` (api create / readme save), `IMPORTED` (hub install), `ROLLBACK`, `REMOVED`, `DETECTED` (found by reconcile, includes first baseline). Same content hash as the latest non-removed revision records nothing.

## Acceptance

1. Unit tests: snapshot/hash stable and order-independent, diff text, dedupe, rollback round-trip, prune keeps newest 30, reconcile (new, changed, removed), scan_skill_dir (clean, injection phrase, credential read), DB insert/list/get.
2. Create with a Critical finding is refused and writes nothing; Warn writes and reports; overwrite keeps the old content in history.
3. Install that scans Critical leaves the installed skill untouched (extracted and scanned beside it first).
4. Uninstall leaves a REMOVED revision and a restorable snapshot.
5. `GET /api/skills` lists `shadows` for a managed skill that overrides another source.
6. OpenAPI spec guard test passes with the new routes; existing `skills::`, `security::scan`, `db::` tests stay green.

## Follow-ups (not in this change)

fenced-code scan for SKILL.md after calibration; events table when P1 needs stage-by-stage records.

## Result (2026-10-21)

All six acceptance items met. 3424 lib tests pass (112 of them in the touched areas), plus `tests/skill_history_api.rs` (5, over real HTTP). OpenAPI spec regenerated (`x-route-count` 402 → 405). Scan measured on 229 installed skills: 214 allow, 14 warn, 1 block (`bot-channels`, `CRED002`). Not run: a full daemon end to end (the shared target is nearly full and a bin build would overwrite `target/debug/senclaw`, which the desktop app re-execs); the startup reconcile hook in `run_daemon` is therefore covered only by the reconcile unit tests.

## P0b result (2026-10-21)

Usage ledger in: `skill_usage` table, `skills::usage` (`note_load` at the `Skill` tool and `force_skill_reminder`; `record` beside `failures::record`), `GET /api/skill-usage` and `/summary` (per skill, per content version, `fallbackRate`, `toolFailureRate`). 3439 lib tests (one in `zen_core::engine` proves the forced-load and `Skill`-tool paths note a load and a hint does not), plus `tests/skill_usage_api.rs` (1) and `tests/skill_history_api.rs` (5). OpenAPI spec regenerated (407 routes). Not exercised: a live chat with a real model loading a real skill. A "new input closes the previous load" rule was dropped on purpose: input and forced loads travel different paths and can arrive in either order.

Gate for P1 (from the research): read `/api/skill-usage/summary` after a week or two of real use; if no skill shows a repeated, non-trivial `fallbackRate`, stop and fix skills by hand.

## Clients (2026-10-21)

One branch `feat/skill-history` per client repo, each in its own worktree under `.worktrees/` (`web-app-skill-history`, `desktop-skill-history`, `senclaw-connect-skill-history`), uncommitted. Web: `npm run build` green, eyeballed in the browser pane against a mock daemon that serves the real response shapes (timeline, scan findings, coloured diff, restore with confirmation, save warning, "overrides bundled" badge). Desktop: `flutter analyze` clean, 546 tests green including `test/skill_history_dialog_test.dart` (models, dialog, row chip, Vietnamese coverage). Connect: `flutter analyze` clean, 52 tests green (parsers, plus `test/skill_history_screen_test.dart` — the screen takes an injectable `PluginsApi`). Connect over a real local relay: 12 of 12 checks passed (`connect-relay-e2e/`). Not run: any of the three against a real daemon build with these routes; the macOS and mobile windows were not driven.
