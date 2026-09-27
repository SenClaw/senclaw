# Incremental agent replies (`agent:delta`) and voice chat

A chat reply reaches the client in two forms: `agent:delta` while the model is
writing, and `agent:reply` when a message is complete. Voice chat depends on the
first one — it cuts the growing text into sentences and speaks each as soon as it
is ready, so the assistant starts talking while the model is still writing.

## What was broken (fixed 2026-08-08)

The desktop overlay had a complete streaming-TTS pipeline
(`StreamingSentenceFeeder` → `SpeechStreamSession`) fed by `agent:delta`.
**The daemon never sent one.** `notify_agent_delta` had no callers and
`EngineEvent::TextChunk` had no publisher, so every client could only act on the
completed `agent:reply` — the streaming path was dead code and replies were only
ever spoken after the fact.

The chain is now connected end to end:

| Where | What happens |
|---|---|
| [`query_llm.rs`](../src/zen_core/query_llm.rs) | The OpenAI/Anthropic SSE parsers call a `TextDeltaSink` per text delta instead of only accumulating. |
| [`conversation.rs`](../src/zen_core/conversation.rs) | The turn loop passes a sink that emits `EngineEvent::TextChunk { agent_id, content, delta }`. |
| [`agent_pool/engine.rs`](../src/agent/agent_pool/engine.rs) | Bridges `TextChunk` → per-jid handler, dropping sub-agent text (only `MAIN_AGENT_ID` is the user-facing reply). |
| [`agent_pool/pool.rs`](../src/agent/agent_pool/pool.rs) | `bind_events` forwards each delta to the UI sink. |
| [`lib.rs`](../src/lib.rs) | `WsAgentEventSink::notify_agent_delta` → `agent:delta` on the WebSocket. Deliberately **not** persisted — `agent:reply` is the record of the turn, and a row per token would replay duplicated text on history load. |
| Clients | Web (`useWebSocket.ts`) and desktop (`conversation_provider.dart`) accumulate deltas into a streaming bubble; voice chat additionally cuts them into sentences and speaks them. |

`AgentEventSink::notify_agent_delta` defaults to a no-op: channels that can only
deliver whole messages (Telegram, Feishu…) ignore it.

## Traps

- **SSE lines split across TCP chunks were silently dropped.** Each network chunk
  was decoded on its own and iterated with `.lines()`, so a `data:` event cut in
  half failed to parse and was skipped — losing that text from the reply, and
  mangling multi-byte characters (Vietnamese diacritics) cut mid-sequence.
  `SseLines` buffers bytes and only decodes complete lines.
- **A turn is not over at the first completed message.** An agent turn can
  complete several messages (answer → tool call → answer). Voice chat used to
  re-arm the mic on the first one, cutting the assistant off; it now queues each
  completed message for speech and ends the turn on `agent:state = idle`, with a
  3 s backstop if that event never lands.
- **Local models stay non-incremental on purpose.** `local-mlx` and
  `local-candle-native` stream raw tokens still carrying `<think>` / harmony
  markers, stripped only at the end by `stream_parser::parse_complete` —
  streaming them would put markup into the bubble and have TTS read it aloud.
- **The daemon must be rebuilt and the app restarted** for streaming to appear;
  a running old daemon simply never emits the event.

## Tests

- [`tests/llm_stream_deltas.rs`](../tests/llm_stream_deltas.rs) — runs a real SSE
  server and asserts deltas arrive **before** the request resolves, and that a
  line split mid-UTF-8 survives.
- `SseLines` unit tests in `query_llm.rs` (`mod sse_tests`).
- `desktop_app/test/streaming_sentence_feeder_test.dart` — sentence cutting,
  early cut, decimal-dot guard.
