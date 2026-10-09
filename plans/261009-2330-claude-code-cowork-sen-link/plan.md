# Plan: Claude Code cowork + MCP bridge + sen-link (A2A)

Status: implemented (uncommitted) · e2e verified with a fake `claude`; live Claude Code run pending re-login · Branch: `feat/claude-code-cowork` · Research: `../reports/research-261009-2326-senclaw-claude-code-cowork.md`

## Outcome
- Kanban card with assignee `claude-code` runs in Claude Code (user's own `claude` binary) inside the card's worktree, not the built-in agent.
- SenClaw verifies: Stop-hook gate runs the verify command inside the session; independent re-run after exit; read-only reviewer session; feedback loop via `--resume`; card reaches Done only on pass.
- Claude Code reaches SenClaw's sub-MCPs (schedule, background, memory, wiki, space/calendar, dispatch, send, usage, profile) + kanban over Streamable HTTP `/mcp/{core|kanban}` (token-gated).
- sen-link: A2A v1.0 JSON-RPC between two daemons — agent card, peer pairing by invite, `SendMessage`/`GetTask`/`ListTasks`/`CancelTask` mapped onto a kanban board.

## Defaults taken (open questions in research report, user said "proceed")
1. Worker permission: `acceptEdits` + configurable `allowedTools` (default Bash + SenClaw MCP), isolation = worktree.
2. Reviewer: second Claude Code session, `--permission-mode plan`, structured verdict.
3. sen-link v1 scope: task delegation only (no memory/calendar replication).
4. Lives in the senclaw repo (`src/claude_code/`, `src/sen_link/`).
5. Transport: direct HTTP(S) (LAN/Tailscale); hub relay later.
6. Claude Code only.

## Constraints
- No engine/LLM code in the daemon; spawn the CLI. Never read/store/forward Claude OAuth tokens.
- No main-DB schema change: settings in global `config.json` (`claudeCode`, `senLink` keys), peers in `~/.senclaw/sen-link/peers.json`, tasks are kanban cards.
- Format only own hunks (repo not rustfmt-clean).

## Phases
| # | Phase | Files | Status |
|---|---|---|---|
| 1 | Settings, detection, CLI runner (stream-json parse, run registry, ephemeral tokens) | `src/claude_code/{mod,settings,runner,registry}.rs` | done |
| 2 | Worker + verify (Stop hook endpoint, verify cmd, reviewer, resume loop); dispatcher routing | `src/claude_code/{worker,verify,hooks}.rs`, `src/agent/mcp_dispatch/mod.rs` | done |
| 3 | MCP bridge `/mcp/{server}` (Streamable HTTP ⇄ stdio child per session) | `src/claude_code/mcp_bridge.rs`, `src/mcp/helper.rs` (reuse) | done |
| 4 | sen-link A2A server/client, peers, invite, REST | `src/sen_link/*` | done |
| 5 | Wiring (lib.rs routes), REST `/api/claude-code/*`, docs, tests | `src/lib.rs`, `docs/claude-code-cowork.md`, `docs/sen-link.md` | done |

## Acceptance
- `cargo build` + `cargo test` for new modules pass; existing tests unaffected.
- Unit tests: stream-json parsing, settings defaults, hook decision, A2A state mapping, JSON-RPC dispatch, token checks.
- Live e2e (needs `claude` login, currently expired): card → Claude Code → verify → Done.

## Result (2026-10-10)
- `cargo test --lib`: 2773 passed, 0 failed (new modules included).
- E2E on isolated daemons (SENCLAW_HOME scratch, ports 28788/38788, fake `claude` speaking stream-json):
  happy path (Stop hook blocked once → fixed → verify + review pass → Done, branch note);
  failure path (3 Stop blocks, 2 resume rounds → Blocked with findings);
  `/mcp/core` 75 tools incl. calendar/schedule/memory, `schedule_task` created in the scoped chat; `/mcp/kanban` 20 tools;
  sen-link pair (one-time code), Triage vs autoRun, A→B task completed by Claude Code with artifacts, List/Cancel, B→A.
- Code review findings fixed: card `verify:` line no longer honoured (shell from untrusted text); peer cannot pick
  assignee; worker guidance no longer contradicts; process-group kill + bounded output; unchecked "done" → Blocked;
  bridge session caps, id rewriting, run-token chat pinning; invite misses don't write; canceled tasks stay canceled;
  trust boundary documented (worker is not sandboxed from the daemon).
- Not done: desktop/web UI screens, hub relay for NAT, A2A streaming/push, live run with real Claude Code.

## Risks
- Live `claude` auth expired on this machine → e2e unverified until re-login.
- http-hook response semantics taken from docs, not observed.
