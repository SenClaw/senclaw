/**
 * Tests for the "Space App serves an LLM" subpath (`@senclaw/space-sdk/llm`).
 *
 * Ported from the Rust SDK's `app-space-sdk/src/llm.rs` tests. Run against
 * `dist/` — the artefact `npm publish` ships — like the rest of the suite.
 *
 * The bytes are the contract: SenClaw's OpenAI adapter accumulates
 * `delta.tool_calls[].function.{name,arguments}` by *concatenation* keyed on
 * `index`, so the streaming tests read the actual SSE `data:` lines back, both
 * through the pure `streamSse` core and end-to-end through the real Express
 * router. A second tool call that reuses index 0 produces `get_weatherget_time`
 * with both argument objects glued together, and no test of the event builder in
 * isolation would notice.
 *
 * Run: `npm test` (builds first). Node's own runner — no test framework.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

import {
  MODELS_CACHE_PATH,
  modelCard,
  parseChatRequest,
  chunk,
  ChunkSink,
  renderChunk,
  renderNonStreamBody,
  renderModels,
  streamSse,
  assembleNonStream,
  handleChatCompletion,
  publishModels,
} from '../dist/llm.js';

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** A provider that emits a fixed list of chunks. */
function fake(chunks = []) {
  return {
    models: () => [modelCard('m', 4096, 512, false)],
    async chat(_req, sink) {
      for (const c of chunks) sink.send(c);
    },
  };
}

/** A provider whose generation throws after emitting `chunks`. */
function boom(chunks = [], message = 'boom') {
  return {
    models: () => [modelCard('m', 4096, 512, false)],
    async chat(_req, sink) {
      for (const c of chunks) sink.send(c);
      throw new Error(message);
    },
  };
}

function reqFor(model = 'm', stream = false) {
  return parseChatRequest({ model, messages: [{ role: 'user', content: 'hi' }], stream });
}

async function collect(gen) {
  const out = [];
  for await (const line of gen) out.push(line);
  return out;
}

/** Parse SSE `data:` lines back into the JSON objects a consumer would see. */
function ssePayloads(lines) {
  return lines
    .join('')
    .split('\n')
    .map((l) => l.trim())
    .filter((l) => l.startsWith('data: '))
    .map((l) => l.slice('data: '.length))
    .filter((d) => d !== '[DONE]')
    .map((d) => JSON.parse(d));
}

// ---------------------------------------------------------------------------
// Request parsing
// ---------------------------------------------------------------------------

test('a request without messages is refused rather than run', () => {
  assert.ok('error' in parseChatRequest({ model: 'm' }));
  assert.ok('error' in parseChatRequest({ model: 'm', messages: [] }));
  assert.ok('error' in parseChatRequest({ messages: [{ role: 'user' }] })); // no model
});

test('both spellings of the output ceiling are read; the newer wins', () => {
  const messages = [{ role: 'user', content: 'hi' }];
  const a = parseChatRequest({ model: 'm', messages, max_tokens: 100 });
  assert.equal(a.maxTokens, 100);

  // `max_completion_tokens` is the current spelling and wins when both appear.
  const b = parseChatRequest({ model: 'm', messages, max_tokens: 100, max_completion_tokens: 200 });
  assert.equal(b.maxTokens, 200);
});

test('multipart content survives the request parse', () => {
  // Image turns arrive as an array of content parts. Re-typing the message would
  // flatten that to a string and drop the image.
  const req = parseChatRequest({
    model: 'm',
    messages: [
      {
        role: 'user',
        content: [
          { type: 'text', text: 'what is this' },
          { type: 'image_url', image_url: { url: 'data:image/png;base64,AAA' } },
        ],
      },
    ],
  });
  const parts = req.messages[0].content;
  assert.equal(parts.length, 2);
  assert.equal(parts[1].image_url.url, 'data:image/png;base64,AAA');
});

// ---------------------------------------------------------------------------
// Chunk rendering
// ---------------------------------------------------------------------------

test('empty text or reasoning emits no chunk at all', () => {
  const counter = { value: 0 };
  assert.equal(renderChunk('id', 'm', chunk.text(''), counter), null);
  assert.equal(renderChunk('id', 'm', chunk.reasoning(''), counter), null);
  // A non-empty delta does render.
  assert.notEqual(renderChunk('id', 'm', chunk.text('hi'), counter), null);
});

test('a tool-call turn finishes as tool_calls, not stop; empty reasoning is omitted', () => {
  const withCall = renderNonStreamBody('m', '', '', [{ id: 'c1' }], null);
  assert.equal(withCall.choices[0].finish_reason, 'tool_calls');

  const plain = renderNonStreamBody('m', 'hi', '', [], null);
  assert.equal(plain.choices[0].finish_reason, 'stop');
  assert.equal(plain.choices[0].message.reasoning_content, undefined);

  const withReasoning = renderNonStreamBody('m', 'hi', 'why', [], null);
  assert.equal(withReasoning.choices[0].message.reasoning_content, 'why');
});

// ---------------------------------------------------------------------------
// Streaming (pure core, fake provider)
// ---------------------------------------------------------------------------

test('two tool calls stream at DISTINCT indices', async () => {
  const payloads = ssePayloads(
    await collect(
      streamSse(
        fake([
          chunk.toolCall('call_a', 'get_weather', '{"city":"Hanoi"}'),
          chunk.toolCall('call_b', 'get_time', '{}'),
        ]),
        reqFor('m', true),
      ),
    ),
  );

  const calls = payloads.map((p) => p.choices?.[0]?.delta?.tool_calls?.[0]).filter(Boolean);
  assert.equal(calls.length, 2);
  assert.equal(calls[0].index, 0);
  assert.equal(calls[0].function.name, 'get_weather');
  // A reused index welds the two calls together into `get_weatherget_time`.
  assert.equal(calls[1].index, 1);
  assert.equal(calls[1].function.name, 'get_time');
});

test('a stream always terminates with data: [DONE]', async () => {
  const lines = await collect(streamSse(fake([chunk.text('hi')]), reqFor('m', true)));
  assert.equal(lines.join('').trimEnd().endsWith('data: [DONE]'), true);
});

test('usage arrives on its own chunk with an empty choices array', async () => {
  const payloads = ssePayloads(
    await collect(streamSse(fake([chunk.text('hi'), chunk.usage(12, 3)]), reqFor('m', true))),
  );
  const usage = payloads.find((p) => p.usage);
  assert.ok(usage, 'a usage chunk must be emitted');
  assert.equal(usage.usage.prompt_tokens, 12);
  assert.equal(usage.usage.total_tokens, 15);
  assert.equal(usage.choices.length, 0);
});

test('a generation failure mid-stream is an error chunk, then [DONE], never a silent truncation', async () => {
  const lines = await collect(streamSse(boom([chunk.text('par')], 'weights missing'), reqFor('m', true)));
  const payloads = ssePayloads(lines);
  const err = payloads.find((p) => p.error);
  assert.ok(err, 'the failure must ride an error chunk');
  assert.equal(err.choices[0].finish_reason, 'error');
  assert.equal(err.error.message, 'weights missing');
  // Whatever text arrived is kept, and the stream is still terminated.
  assert.equal(payloads[0].choices[0].delta.content, 'par');
  assert.equal(lines.join('').trimEnd().endsWith('data: [DONE]'), true);
});

// ---------------------------------------------------------------------------
// Non-stream (pure core, fake provider)
// ---------------------------------------------------------------------------

test('non-stream returns one assembled message with text deltas concatenated', async () => {
  const { status, body } = await assembleNonStream(
    fake([chunk.reasoning('thinking'), chunk.text('he'), chunk.text('llo')]),
    reqFor('m', false),
  );
  assert.equal(status, 200);
  assert.equal(body.object, 'chat.completion');
  assert.equal(body.choices[0].message.content, 'hello');
  assert.equal(body.choices[0].message.reasoning_content, 'thinking');
  assert.equal(body.choices[0].finish_reason, 'stop');
});

test('non-stream: a generation error becomes a 500, not a 200 with half an answer', async () => {
  const { status, body } = await assembleNonStream(boom([chunk.text('half')], 'db gone'), reqFor('m', false));
  assert.equal(status, 500);
  assert.equal(body.error.message, 'db gone');
});

// ---------------------------------------------------------------------------
// Dispatch: unknown model, models list
// ---------------------------------------------------------------------------

test('an unknown model is 404, not a silent default', () => {
  const r = handleChatCompletion(fake([]), { model: 'nope', messages: [{ role: 'user' }] });
  assert.equal(r.kind, 'error');
  assert.equal(r.status, 404);
});

test('a bad body is 400 before the provider is touched', () => {
  const r = handleChatCompletion(fake([]), { model: 'm', messages: [] });
  assert.equal(r.kind, 'error');
  assert.equal(r.status, 400);
});

test('the models endpoint carries the capability fields', () => {
  const body = renderModels([modelCard('m', 4096, 512, false)]);
  assert.equal(body.object, 'list');
  const m = body.data[0];
  assert.equal(m.id, 'm');
  assert.equal(m.context_length, 4096);
  assert.equal(m.max_output_tokens, 512);
  // `vision` decides real image blocks vs OCR, so it must survive the hop.
  assert.equal(m.vision, false);
  assert.equal(m.owned_by, 'senclaw-space-app');
  // `tools` defaults true when a card omits it.
  assert.equal(renderModels([{ id: 'x', contextLength: 1, maxOutputTokens: 1, vision: true }]).data[0].tools, true);
});

// ---------------------------------------------------------------------------
// ChunkSink
// ---------------------------------------------------------------------------

test('ChunkSink delivers events in order and starts open', async () => {
  const seen = [];
  const provider = {
    models: () => [modelCard('m', 4096, 512, false)],
    async chat(_req, sink) {
      assert.equal(sink.isClosed(), false);
      sink.text('a');
      sink.send(chunk.text('b'));
    },
  };
  const payloads = ssePayloads(await collect(streamSse(provider, reqFor('m', true))));
  for (const p of payloads) if (p.choices?.[0]?.delta?.content) seen.push(p.choices[0].delta.content);
  assert.deepEqual(seen, ['a', 'b']);
});

// ---------------------------------------------------------------------------
// Model cache (publishModels)
// ---------------------------------------------------------------------------

test('publishModels refuses an empty list and never clobbers a good cache', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'senclaw-llm-'));
  await publishModels(dir, [modelCard('m', 4096, 512, false)]);
  const good = readFileSync(join(dir, MODELS_CACHE_PATH), 'utf8');

  await assert.rejects(() => publishModels(dir, []), /empty/);
  const after = readFileSync(join(dir, MODELS_CACHE_PATH), 'utf8');
  assert.equal(after, good, 'a failed publish must leave the cache intact');
});

test('a published card round-trips in the daemon wire shape (snake_case, tools defaults true)', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'senclaw-llm-'));
  // The factory defaults tools to true.
  assert.equal(modelCard('x', 1, 1, false).tools, true);

  await publishModels(dir, [modelCard('gemma', 128_000, 8192, true, { displayName: 'Gemma 4' })]);
  const cache = JSON.parse(readFileSync(join(dir, MODELS_CACHE_PATH), 'utf8'));
  const m = cache.models[0];
  // snake_case keys — this is what the daemon deserialises the card from.
  assert.equal(m.id, 'gemma');
  assert.equal(m.display_name, 'Gemma 4');
  assert.equal(m.context_length, 128_000);
  assert.equal(m.max_output_tokens, 8192);
  assert.equal(m.vision, true);
  assert.equal(m.tools, true);
});

test('publishModels omits display_name when absent, matching the Rust skip', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'senclaw-llm-'));
  await publishModels(dir, [modelCard('m', 1, 1, false)]);
  const m = JSON.parse(readFileSync(join(dir, MODELS_CACHE_PATH), 'utf8')).models[0];
  assert.equal('display_name' in m, false);
});

// ---------------------------------------------------------------------------
// End-to-end through the real Express router — the bytes are the contract
// ---------------------------------------------------------------------------

/** Spin up an express app mounting `openaiRouter`, run `fn(baseUrl)`, tear down. */
async function withRouter(provider, fn) {
  const express = (await import('express')).default;
  const { openaiRouter } = await import('../dist/llm.js');
  const app = express();
  app.use(await openaiRouter(provider));
  const server = await new Promise((resolve) => {
    const s = app.listen(0, '127.0.0.1', () => resolve(s));
  });
  const port = server.address().port;
  try {
    return await fn(`http://127.0.0.1:${port}`);
  } finally {
    await new Promise((resolve) => server.close(resolve));
  }
}

test('e2e: GET /v1/models exposes the capability fields over the wire', async () => {
  const body = await withRouter(fake([]), (base) =>
    fetch(`${base}/v1/models`).then((r) => r.json()),
  );
  assert.equal(body.data[0].id, 'm');
  assert.equal(body.data[0].context_length, 4096);
  assert.equal(body.data[0].vision, false);
});

test('e2e: an unknown model is answered 404', async () => {
  const status = await withRouter(fake([]), (base) =>
    fetch(`${base}/v1/chat/completions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ model: 'nope', messages: [{ role: 'user', content: 'x' }] }),
    }).then((r) => r.status),
  );
  assert.equal(status, 404);
});

test('e2e: two tool calls stream at distinct indices through the router', async () => {
  const raw = await withRouter(
    fake([
      chunk.toolCall('call_a', 'get_weather', '{"city":"Hanoi"}'),
      chunk.toolCall('call_b', 'get_time', '{}'),
    ]),
    (base) =>
      fetch(`${base}/v1/chat/completions`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ model: 'm', messages: [{ role: 'user', content: 'hi' }], stream: true }),
      }).then((r) => r.text()),
  );
  const calls = ssePayloads([raw])
    .map((p) => p.choices?.[0]?.delta?.tool_calls?.[0])
    .filter(Boolean);
  assert.equal(calls.length, 2);
  assert.equal(calls[0].index, 0);
  assert.equal(calls[1].index, 1);
  assert.equal(calls[1].function.name, 'get_time');
  assert.equal(raw.trimEnd().endsWith('data: [DONE]'), true);
});

test('e2e: a non-stream turn returns one assembled chat.completion', async () => {
  const body = await withRouter(fake([chunk.text('he'), chunk.text('llo')]), (base) =>
    fetch(`${base}/v1/chat/completions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ model: 'm', messages: [{ role: 'user', content: 'hi' }] }),
    }).then((r) => r.json()),
  );
  assert.equal(body.object, 'chat.completion');
  assert.equal(body.choices[0].message.content, 'hello');
  assert.equal(body.choices[0].finish_reason, 'stop');
});
