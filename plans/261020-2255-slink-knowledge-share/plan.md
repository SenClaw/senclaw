# SLink: ask a paired SenClaw what it knows (policy + recall)

Source: `plans/reports/research-261020-2247-slink-knowledge-sync.md` (in the main checkout; recommends *mirror, not merge*, starting with query-time recall). Branch `feat/slink-knowledge`, cut from `main` (v0.2.2). Local only: nothing pushed, tagged or installed.

## Outcome

A paired SenClaw can be told, per peer, which of its knowledge that peer may read, and an agent or person can ask the peer a question and get matching facts and notes back, over the existing sealed channel. Nothing is copied, nothing is written on either side.

## Scope (P0 + P1 of the research)

- **P0 policy**: `Peer.knowledge` = `{facts, notes, profiles[], noteTypes[]}`, everything off by default, editable with `PATCH /api/sen-link/peers/:id`; `GET /api/sen-link/peers/:id/knowledge` shows the policy and a preview (counts and 5 sample titles) of what it would expose; the Agent Card advertises `knowledge: 1`.
- **P1 recall**: sealed method `knowledge/recall {query, limit, profile?}` → hits; `POST /api/sen-link/peers/:id/recall` and MCP tool `link_recall` ask a peer. Keyword search only (no model call, no embedding), as the memory engine's tools do.

## Exposure rules (decided here, enforced before ranking)

- Facts: only the policy's profiles (bank = profile folder), `origin = ''` (not said in one chat), not replaced.
- Notes: only `profiles/<folder>/memory/*.md` with frontmatter, not a daily log, not `conversation-summary-*`, type in `noteTypes` ⊆ {project, reference, feedback} (`user` can never be shared), authored for the whole agent (`originSessionId` empty or the folder name), never a chat's.
- A peer with nothing enabled is refused with its own code (`-32010`, "has not shared"), distinct from an older daemon's `-32601` ("update it"): the asker is already trusted to be paired, and the refusal says nothing about what exists.
- At most 10 hits, each text cut at 600 characters; at most 30 knowledge calls a minute per peer.

## Non-goals

Copying (mirror) of facts or notes, wiki, graph, project banks, profile files, deletion sync, UI for the policy in web/desktop (follow-up), merge of any kind.

## Acceptance

1. Default peer: recall refused; policy edits persist across restart and survive re-pairing.
2. With policy on, only rule-conforming facts/notes come back; each excluded class has a test.
3. Two isolated real daemons: pair, enable, recall over the sealed channel; a recording relay sees none of the query or the answer text.
4. Peer that lacks the method (v0.2.x) gives a clear "update it" message.
5. `cargo test --lib` green, OpenAPI spec test updated.

## Result (2026-10-20)

Done as planned. The web and desktop screens followed (branches `feat/slink-knowledge` of web-app and desktop). `cargo test --lib`: 3385 passed, 0 failed (was 3369). New: 15 tests in `sen_link::knowledge` (every excluded class, scope, limits, rate, links and big files, policy validation, the asking side's messages, and a recording relay that sees none of the question or answer), 1 in the fact store (`Scope::AgentWide`), card capability in `a2a`.

Live, two isolated daemons from a debug build with the real wiring (`with_knowledge(db, profiles_dir)`): closed by default; after opening profile `main` a question returned the agent-wide fact and the one reference note and none of seven planted secrets (a day log, a conversation summary, a `user` note, a chat-written note, a chat-scoped fact, and a second profile's note and fact); asking for a profile outside the policy was refused; the second profile appeared only once named; the 28th question in a minute was refused (`-32011`); three bad policies got 400 and changed nothing; closing the policy closed the answers. The preview counts matched the excluded classes. Test trap: a scratch daemon's profiles live under `$HOME/senclaw/profiles`, not `$SENCLAW_HOME/data/profiles`.

Not done / open: peers on older builds only say "update SenClaw"; notes are scored by token overlap, not by the FTS index, so ranking is plainer than the agent's own search; mirror (research P2/P3) not started.
