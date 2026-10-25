---
title: "Datagen 100%: three more pipelines, web and desktop UI"
date: 2026-10-12
summary: "doc_qa, reasoning_traces, executable_qa (Seatbelt-confined), records/delete API, Datagen pages on web and desktop; second review caught a reference-answer leak via judge feedback and a hidden form field"
---

# Datagen 100%: three more pipelines, web and desktop UI

## What happened
- User asked to free disk and finish the feature 100%. Freed ~26 GB of regenerable caches (Go build cache, npm, uv, Homebrew, pnpm prune, Cursor update downloads, idle repos' cargo targets, senclaw leftover .o/release); left Docker (42 GB, may hold volumes), HF/MLC models, iOS DeviceSupport and worktrees alone.
- Added doc_qa, reasoning_traces, executable_qa (runs model code via crate::sandbox run_once, refuses without an enforced backend), shared common.rs, records/delete REST, unbundled MCP registration, fsync manifests, DatagenPage.tsx (web) and lib/features/datagen (desktop); client-parity row with mobile N/A + reason.
- E2E on an isolated daemon with a fake OpenAI endpoint: executable_qa labels came from real Seatbelt runs (66/91/105), a crashing program was rejected; web page exercised in the built-in browser.

## Review findings worth remembering
- A "the generator never sees the reference" rule is broken by any judge that sees the reference and writes feedback the generator reads. Keep the judge blind and redact the reference from feedback.
- Quote grounding by bag-of-words lets a model stitch real words into a false fact; require an in-order match inside a short window, and count multi-hop by distinct sentences.
- Multi-pipeline forms leak hidden field values (antd preserve, Flutter controllers kept in a map). Reset per pipeline.
- A tool that reads user-named paths must refuse SenClaw's own folder and not follow symlinks.

## Next steps
- Commit the three feat/datagen-core branches (senclaw, web-app, desktop) — not committed yet.
- Flow router (shadow) using an expand_seeds eval set; sen-sysone plan_local fix.

> Historical work record — not durable authority. Prefer docs/specs/ADRs for current decisions.
