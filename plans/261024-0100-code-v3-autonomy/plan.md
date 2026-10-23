# Code v3 — unattended runs, project understanding, local-model fit

Branch `feat/code-v3` (worktree `.worktrees/senclaw-code-v3`, from `main` 0.2.6).
Research basis: `plans/reports/researcher-261024-0100-openhands-and-peer-coding-agents.md`.

## Outcome

A coding chat on a local model finishes a normal task (read, edit, build, test, commit)
with **no approval cards** unless something cannot be taken back, understands the repo it
was pointed at (its own instructions, how to build and test it), and keeps working when
the model is small.

## Non-goals

- A sandbox. `auto` grades what the command line shows; Plugins → Sandbox stays the
  hard boundary for repositories that are not the person's own.
- UI work in `web-app` / `desktop` (the setting is served by `/api/agent-behavior`; the
  clients ignore keys they do not know). Listed as a follow-up.
- Anything that changes `ask` behaviour: a chat outside a code session / git repo, or
  with `codeAutonomy: ask`, prompts exactly as before.

## Acceptance

1. In a code session or a git-repo chat, `cargo test`, `npm run build`, `git commit`,
   `mkdir`, `rm <file in project>`, edits inside the project run with no card; `git push`,
   `sudo`, `rm -rf ~`, `curl | sh`, an edit outside the project, `.env`, `.git/hooks` ask,
   and the card says why.
2. `auto`/`full` never apply where a checkpoint cannot undo the change (not a git repo,
   not a code session, `$HOME`, `/`).
3. The project's own `SENCLAW.md` / `AGENTS.md` / `CLAUDE.md` reaches the model in a code
   session (it never did: `instance_uses_workspace` returned `false`).
4. The model is told how to build and test the repo it is in, from the files in it.
5. `GET /api/code/autonomy` shows `ranUnattended` vs `askedAnyway`.

## Phases

| # | Phase | Status |
|---|---|---|
| 1 | Risk grading + `Autonomy` in `PermissionManager` + `codeAutonomy` setting | done |
| 2 | Project instructions (`AGENTS.md`/`CLAUDE.md`/`SENCLAW.md`) + detected build/test commands in the first turn | done |
| 3 | Verify-before-finish nudge; syntax check after edits; `Edit` miss shows the closest lines; coding working-style reminder | done |
| 4 | Docs ([`docs/code-autonomy.md`](../../docs/code-autonomy.md)), OpenAPI refresh, e2e on an isolated daemon (22/22, `plans/reports/code-v3-e2e/`) | done |

Status: implemented on `feat/code-v3`, **uncommitted**, not installed on the real daemon. 3567 lib tests pass.

## Decisions

- **Reversibility, not suspicion** — tiers are Low (reads), Medium (stays in project),
  High (leaves, publishes, deletes, privileged, not understood). Only High asks.
- **Unknown = High.** A wrong guess costs one prompt.
- **Standing user grants still win** ("never ask again" rules and saved allow-lists are
  checked before the grading), so a person's explicit choice is never second-guessed.
- **`full` is not "skip flags".** It is a per-chat level honoured only where reversible,
  and it never answers `browser_approve` (the person's own yes).
- Pre-existing hole found on the way: `env <cmd>` was on the read-only first-word list,
  so `env rm -rf x` ran with no prompt. Fixed with a regression test.

## Risks

- A pattern the grader calls Medium that is not (e.g. an interpreter running a script
  the model just wrote). Accepted, same as any agent that may write and run code; the
  sandbox is the answer, documented in `docs/code-autonomy.md`.
- False High on legitimate unusual commands → a prompt, then "never ask again".

## Found on the way

- `env <cmd>` ran with no prompt (read-only first-word list) — fixed.
- A double `lock()` on one mutex inside a struct literal hung every chat at "Preparing MCP servers" — found only by the daemon e2e; regression test added.

## Not done

- Real-model run (gemma e4b): the e2e used a scripted LLM, so approval counts are exact but model behaviour (does it follow the working-style reminder, how often does a grade surprise it) is unmeasured.
- Sandbox as the approval boundary (research idea 1), prefix-rule tests, nested `AGENTS.md` on file touch, observation masking.

## Unresolved

- Whether to also ask the decision gate (Jev) about `Unknown` programs before prompting.
- UI switch placement (Settings → Agent behavior) in web and desktop.
