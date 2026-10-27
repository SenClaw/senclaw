# Spec-driven coding tools: survey + SenClaw recommendations (2026-10-27)

Versions: Spec Kit v1.2.3 (2026-10-22), OpenSpec v1.15.0 (2026-10-24), superpowers v7.0.1 (2026-10-21), spec-kit-red-team v1.0.3. Kiro / Claude Code / Cursor = live docs read 2026-10-27, no version shown. Raw template/skill/doc markdown read, not summaries. Companion: [research-261027-1505-egm-elephant-goldfish.md](research-261027-1505-egm-elephant-goldfish.md) (the reference the user picked).

## 1. Findings

| Tool | (a) Storage / format | (b) Flow | (c) Edit gate | (d) Resume | (e) Review |
|---|---|---|---|---|---|
| Kiro | `.kiro/specs/<name>/{requirements,design,tasks}.md` (bugfix.md for bugs). Reqs = user stories + EARS (`WHEN..THE SYSTEM SHALL..`). Steering: `.kiro/steering/{product,tech,structure}.md` + `~/.kiro/steering`, frontmatter mode `always\|fileMatch\|manual\|auto` | `/spec new` -> requirements -> design -> tasks -> `/spec run`; Quick Spec = no gates | UI approve per phase ("Happy?" loop). No engine block documented. Hooks `PreToolUse` can block | `/spec <name>`; `#spec` injects all 3 files in full; live task status; steering always-on | "Analyze Requirements": inconsistencies, ambiguities, conflicting constraints, unstated assumptions, missing edge cases. Questions with suggested fixes, dismissable. No severity |
| Spec Kit | `specs/NNN-slug/{spec,plan,tasks}.md` + research, data-model, contracts/, checklists/requirements.md; `.specify/memory/constitution.md`; `.specify/feature.json` = active feature dir | constitution -> specify -> clarify (max 5 Qs, logged under `## Clarifications / Session date`) -> plan -> tasks (`- [ ] T001 [P] [US1]`) -> analyze -> implement (ticks `[X]`) | Prompt convention. implement scans checklists, asks "proceed anyway?". Third-party hard gates: spec-gates (PreToolUse), Plan Review Gate (PR merge first) | feature.json + checkboxes; commands re-read files | `analyze`: strictly read-only; passes: duplication, ambiguity, underspec, constitution, coverage gaps, inconsistency; CRITICAL/HIGH/MEDIUM/LOW; <=50 findings, stable IDs; constitution breach = auto CRITICAL; advisory only. `specify` caps `[NEEDS CLARIFICATION]` at 3 |
| OpenSpec | `openspec/specs/<domain>/spec.md` (truth) vs `openspec/changes/<name>/{proposal,design,tasks}.md` + delta specs (ADDED/MODIFIED/REMOVED) + `.openspec.yaml`. `### Requirement:` SHALL/MUST + `#### Scenario:` GIVEN/WHEN/THEN | `/opsx:propose\|explore\|new\|continue\|ff\|apply\|verify\|sync\|archive`; archive merges deltas into `specs/`; `config.yaml` injects `context:` + `rules:` | None by design ("fluid not rigid, no phase gates"). CLI state blocked/ready/all_done = artifacts exist only | change folder + `[x]` progress; `openspec list/status --json` | `validate`: errors (no SHALL/MUST, no scenario, empty) vs warnings (req >500 chars, >10 deltas, TBD purpose). `/opsx:verify`: Completeness/Correctness/Coherence, CRITICAL/WARNING/SUGGESTION, never blocks archive |
| superpowers | `docs/superpowers/specs/DATE-topic-design.md`, `.../plans/DATE-feature.md` (committed); ledger `.superpowers/sdd/<plan>/progress.md` (gitignored) | brainstorming (one question/message, read code before asking, playback every 200-300 words, approaches in own message) -> design doc -> writing-plans -> executing-plans | `<HARD-GATE>` prose in skill: nothing built before human approves doc. Prompt-only | ledger names plan file; `Task N: complete` lines; resume at first undone | "builder check": cold subagent reads doc, returns only ranked questions it cannot decide. Final reviewer Critical/Important/Minor + verdict |
| Claude Code plan mode | plan file `~/.claude/plans/` or `plansDirectory` | Shift+Tab / `/plan` -> research -> ExitPlanMode -> approve or keep planning | Tool-layer: edits blocked until approval | No documented plan pickup in new sessions | none |
| Cursor plan mode | virtual md file; "Save to workspace" -> repo | Shift+Tab -> Q&A -> research -> plan -> Build | undocumented | undocumented | none |

Multi-lens review: only spec-kit-red-team (community, immature) has named lenses. Lens = YAML `{name, description, core_questions (>=3), trigger_match, severity_weight, finding_bound=5}`; 3-5 read-only subagents in parallel; report `ID | Lens | Severity | Location | Finding | Suggested Resolution | Status`; each finding resolved as spec-fix / new-OQ / accepted-risk / out-of-scope (never auto-applied); `before_plan` gate PROCEED / SATISFIED / HALT; waiver = recorded accepted risk. Spec Kit `checklist` = "unit tests for English": Completeness, Clarity, Consistency, Measurability, Coverage.

Gap: none of the six hash the approved text or bind approval to a person. Only Claude Code gates at tool layer.

## 2. Recommendations (ranked)

1. Hard gate in the tool layer, not prose (prompt gates fail on weak models). Gate Edit/Write and write-capable Bash.
2. Split storage: spec text in the repo; approval state (approved hash, approver, time, review results) in daemon data the agent cannot write. A clone elsewhere = unapproved (safe default).
3. Person-only approval: UI/REST/typed command; no model-callable tool, no SLink peer, no scheduled/background agent.
4. Hash the body (LF-normalized, trailing whitespace trimmed); checkbox progress excluded. Mismatch -> stale, gate closes, person re-approves.
5. Resume = short capsule (~300-500 tokens: path, status, goal, non-goals, next unchecked task) + read-receipt: first gated edit fails until the session read the spec. Re-inject after context clear.
6. Size caps for small models (e4b): one question per message, few open questions; split large specs.
7. Review: deterministic pre-checks first (0 tokens: sections, placeholders, missing paths), then LLM lenses; sequential for local models. Findings bound to the spec hash; Blocker disables approve unless waived with a reason.
8. Minimum frontmatter: id, title, status (informational mirror), created; approval fields live in the daemon.
9. Requirements with IDs + testable scenarios; tasks as checkboxes.
10. Trivial fixes: a person-only "no spec" escape.

## 3. Unresolved

- Kiro hard enforcement, Cursor storage, Claude Code cross-session pickup: not in official docs.
- Gate global for code sessions or opt-in? Edits outside the spec's scope?
- Bash writes: gate or rely on the sandbox?
- One active spec per folder or several?
- Which clients count as "a person" for approval?

## Sources

- Kiro: https://kiro.dev/docs/specs , /steering , /hooks/types , /specs/analyze-requirements
- Spec Kit: https://github.com/github/spec-kit/tree/main/templates/commands ; github.com/ashbrener/spec-kit-red-team, luno/spec-kit-plan-review-gate, schwichtgit/spec-gates
- OpenSpec: https://github.com/Fission-AI/OpenSpec/tree/main/docs , src/core/validation/constants.ts
- superpowers: https://github.com/obra/superpowers/tree/main/skills
- Claude Code: https://code.claude.com/docs/en/permission-modes.md
- Cursor: https://cursor.com/docs/agent/plan-mode.md
