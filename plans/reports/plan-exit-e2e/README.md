# Plan approval and skill guard: end-to-end check

A scratch daemon (own ports 48790/48791, own `HOME`/`SENCLAW_HOME`, run under a `sandbox-exec` profile that
refuses connects to the real daemon's 18788-18790) with a scripted LLM (`mock-llm.mjs`, copied from
`../code-v3-e2e` and extended). The test puts the script in the user message as `§§STEPS{…}§§`; the mock replays
it and records every request it receives, so the checks read what the model was actually sent.

```bash
./up.sh <senclaw binary>      # builds nothing: pass a binary
node drive.mjs <label>        # 20 checks, result-<label>.json under run/
./down.sh
```

| Script | What it checks |
|---|---|
| P1 | Plan mode, prose reply (hidden nudge → `ExitPlanMode`), approval with "clear context and start fresh": the next request holds the request and the plan, no old assistant turn, no Chinese stub, and the script's `afterRestart` step runs |
| P2 | `ExitPlanMode` with an empty `plan`: refused with `Error: …`, no approval card, a text-only reply after it is sent back, the real plan then goes through |
| P3 | Skill `web-research` loaded, answer from memory: sent back once naming the skill, the tool is then called, no warning |
| P4 | A model that keeps answering from memory: sent back twice, the reply carries `⚠️ Not looked up` |

Results, 2026-10-25:

- `result-before.json`: the installed daemon, v0.2.7 (2a97404), **5 of 20** pass. After the approval the only message was
  `按照以下计划进行实现： P1-PLAN: …` (no tag: the script and the request were gone, the reply was the mock's fallback `[]`), the
  empty plan opened an approval card, and P3/P4 answered with invented figures and no warning.
- `result-after.json`: this branch, **20 of 20**, twice in a row.

Not covered: the stored mode in `pending_agent_modes` (unit test in `agent_pool/tests.rs`; an engine rebuild cannot be
triggered from outside), and a skill forced in before the turn.

Found while reading the traffic, not fixed here: a restart also drops the first-turn `<system-reminder>` (today's date, the
profile, the project's instructions), because that block rode on the first user message the clear removes.
