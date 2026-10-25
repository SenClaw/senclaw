---
title: Datagen core in the daemon
date: 2026-10-12
summary: expand_seeds + export_logs as background jobs; review caught cwd-relative seeds_file and unscoped MCP tools
---

# Datagen core in the daemon

## What happened
- Researched camel-ai/camel (v0.2.91a7) for SenClaw flows; report `plans/reports/research-261012-2244-camel-flows-datagen-memory.md`. User chose to start with datagen core, inside the daemon.
- Built `src/datagen/` (verdict, filters with ROUGE-L, accept-until-N loop, store, jobs registry), pipelines `expand_seeds` (new|evolve, balanced label quotas, blind classifier label check that fails closed) and `export_logs` (trajectory → sharegpt/alpaca/messages), REST `/api/datagen/*`, MCP `senclaw-datagen` (3 tools).
- First E2E showed labels 5/5/2: per-round batches filled the target before the last label. Fixed with an even quota per label.

## Review findings worth remembering
- A relative path in an MCP tool must not be resolved against the subprocess cwd: MCP children are spawned without `current_dir`, so their cwd is the daemon's (the app bundle on desktop). Resolve against the chat's workspace state file, or refuse.
- Chat-facing tools must be scoped by the chat from the server's env (`SENCLAW_CHAT_JID`), never a parameter — same rule as `watch_stop`. Jobs now carry an `owner`; export from a chat covers that chat only.
- A run with zero records is a failure; say why (model error vs "25 banned_phrase").
- `deny_unknown_fields` works on params inside `#[serde(flatten)]` + internally tagged enum (verified by test) — misspelled snake_case keys now 422 instead of silently using defaults.

## Incidents
- Disk filled (117 MB free) from `senclaw/target` (24 GB). Reclaimed `target/debug/incremental` (9.7 GB) and leftover `deps/*.rcgu.o` (8.3 GB); rebuilt with `CARGO_INCREMENTAL=0`.
- Port 28788 was held by another session's test daemon; one stray job landed there and was cancelled at 0 LLM calls. Check `lsof` + owner cwd before trusting a test port.

## Next steps
- Commit `feat/datagen-core` (not committed yet).
- Then: flow router in shadow mode using the eval set from `expand_seeds`; DAG planner/recovery/quality gate; sen-sysone `plan_local` fix (laya-browser auto-picked for general callers).

> Historical work record — not durable authority. Prefer docs/specs/ADRs for current decisions.
