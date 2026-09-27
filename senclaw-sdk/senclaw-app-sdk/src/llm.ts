/**
 * Serving an LLM **from** a Space App, so SenClaw can route turns to it.
 *
 * This is the reverse of the AI bridge in the root export. There, an app asks
 * the daemon for a completion. Here, the app *is* the model: it declares an
 * `llm` block in its `senclaw-manifest.json`, the daemon registers the models it
 * advertises into the same picker as every remote provider, and agent turns
 * arrive over HTTP.
 *
 * ```ts
 * import express from 'express';
 * import { openaiRouter, publishModels, modelCard, chunk } from '@senclaw/space-sdk/llm';
 * import type { LlmProvider } from '@senclaw/space-sdk/llm';
 *
 * const mlx: LlmProvider = {
 *   models: () => [modelCard('gemma-4-e2b-it-4bit', 128_000, 8192, true)],
 *   async chat(req, sink) {
 *     sink.text('hello');
 *     sink.send(chunk.usage(12, 3));
 *   },
 * };
 *
 * await publishModels(process.cwd(), mlx.models());   // so a stopped app still shows in the picker
 * app.use(await openaiRouter(mlx));                    // mounts /v1/models + /v1/chat/completions
 * ```
 *
 * ## Why the app owns the wire format and not the provider
 *
 * The provider emits **semantic** events — visible text, reasoning, a tool call
 * — and this module renders them as OpenAI `chat.completion.chunk` SSE. That
 * split is the whole point: the daemon's OpenAI adapter is a real parser with
 * real expectations (`delta.content`, `delta.reasoning_content`, indexed
 * `delta.tool_calls` whose `name` and `arguments` *accumulate* across chunks),
 * and every app that hand-rolled that JSON would get a different corner of it
 * wrong. An app that implements {@link LlmProvider} cannot get it wrong at all.
 *
 * It also decides where parsing lives. A local model emits its tool calls as
 * *text* in whatever dialect its chat template uses; something has to turn that
 * into `tool_calls`. That something is the app, because the app holds the
 * model's own parser config. By the time bytes reach the daemon they are
 * ordinary OpenAI, which is what lets this reuse `adapt: "openai"` instead of
 * adding an adapter.
 *
 * ## The manifest block
 *
 * ```jsonc
 * "llm": { "autoRegister": true, "path": "/v1", "adapt": "openai", "displayName": "MLX" }
 * ```
 *
 * `adapt` must be `openai` (or `anthropic`) — the daemon routes the turn through
 * that adapter, and a value it does not route means every turn gets an OpenAI
 * body and fails upstream with an error naming neither the app nor the field.
 *
 * Node-only: {@link openaiRouter} reaches for `express` (lazily) and
 * {@link publishModels} for `node:fs`. Import from a server process, never from
 * browser app code. The types and the pure rendering functions are safe to
 * import anywhere.
 */

// Types only — erased at compile time, so this drags no `express` into a bundle
// that merely imports the types or the pure rendering helpers below.
import type { Router, Request, Response } from 'express';

/**
 * Where the daemon looks for an app's model list while the app is **stopped**.
 *
 * Relative to the app's own directory. A session app is stopped most of the
 * time — that is its resting state — and a model nobody can see in the picker is
 * a model nobody selects, calls, or ever starts the app for. So the list is
 * cached on disk at startup and read from there when the process is gone.
 */
export const MODELS_CACHE_PATH = '.senclaw/llm-models.json';

// ============================================================================
// What a provider advertises
// ============================================================================

/** One model this app can serve. */
export type ModelCard = {
  /**
   * Wire id. This is what arrives in {@link ChatRequest.model}, and what the
   * user sees in the picker unless `displayName` says otherwise.
   */
  id: string;
  /** Human label for the picker. Defaults to {@link ModelCard.id}. */
  displayName?: string;
  /** Total context window, in tokens. */
  contextLength: number;
  /** Ceiling on one response, in tokens. */
  maxOutputTokens: number;
  /**
   * **Required, never inferred.** SenClaw decides whether to send image blocks
   * or fall back to OCR from this field, and the consequences are asymmetric: a
   * text-only endpoint answers an image block with a hard 400 that fails the
   * entire turn, while OCR merely degrades it. Inference from the model id
   * cannot be trusted here — a local checkpoint is named things like
   * `mlx-community__Qwen3.5-2B-OptiQ-4bit`, which matches no vendor pattern, so
   * a guess lands on `false` by accident today and `true` by accident the day
   * someone widens a regex. The app has the model's `config.json` open; it knows.
   */
  vision: boolean;
  /**
   * Whether the model can be given tools. `false` makes it a chat-only model in
   * the picker. Defaults to `true` — see {@link modelCard}.
   */
  tools: boolean;
};

/**
 * Build a {@link ModelCard}. `tools` defaults to `true`; pass `opts.tools:
 * false` for a chat-only model, and `opts.displayName` for a picker label that
 * differs from the id.
 */
export function modelCard(
  id: string,
  contextLength: number,
  maxOutputTokens: number,
  vision: boolean,
  opts: { displayName?: string; tools?: boolean } = {},
): ModelCard {
  return {
    id,
    contextLength,
    maxOutputTokens,
    vision,
    tools: opts.tools ?? true,
    ...(opts.displayName ? { displayName: opts.displayName } : {}),
  };
}

// ============================================================================
// One turn
// ============================================================================

/**
 * An incoming turn, in OpenAI `chat/completions` shape.
 *
 * The modelled fields are the ones every provider needs. `raw` carries the whole
 * body besides, because SenClaw sends more than this type names — HF-style
 * `tools`, `stream_options`, provider-specific extras — and a provider that
 * understands one of them should not have to fork the SDK to read it.
 */
export type ChatRequest = {
  /** Which {@link ModelCard.id} this turn is for. */
  model: string;
  /**
   * OpenAI-shaped messages, **untouched**. Kept as raw JSON rather than a typed
   * shape: `content` is a string on some turns and an array of parts on others
   * (that is how images arrive), and re-typing here would flatten that array and
   * drop exactly the parts a vision model needs.
   */
  messages: unknown[];
  /** Tool definitions, or empty. OpenAI function shape. */
  tools: unknown[];
  /**
   * Did the caller ask for SSE? {@link openaiRouter} handles both, so a provider
   * normally ignores this — it is here for one that can genuinely go faster when
   * nothing is watching.
   */
  stream: boolean;
  /** Output ceiling for this turn, when the caller set one. */
  maxTokens?: number;
  /** Sampling temperature, when the caller set one. */
  temperature?: number;
  /** The complete request body. */
  raw: unknown;
};

function asRecord(v: unknown): Record<string, unknown> {
  return v && typeof v === 'object' ? (v as Record<string, unknown>) : {};
}

/** A non-negative integer token count, or `undefined` for anything else. */
function readUint(v: unknown): number | undefined {
  return typeof v === 'number' && Number.isFinite(v) && v >= 0 ? Math.floor(v) : undefined;
}

/**
 * Parse a `chat/completions` body into a {@link ChatRequest}, or return
 * `{ error }` describing why it was rejected.
 *
 * An absent/empty `messages` is refused rather than run: an empty turn is a
 * caller mistake, and answering it with a hallucinated reply is worse than a
 * clear 400.
 */
export function parseChatRequest(body: unknown): ChatRequest | { error: string } {
  const b = asRecord(body);
  const model = typeof b.model === 'string' ? b.model : '';
  if (!model) return { error: '`model` is required' };
  if (!Array.isArray(b.messages)) return { error: '`messages` must be an array' };
  if (b.messages.length === 0) return { error: '`messages` must not be empty' };
  return {
    model,
    messages: b.messages,
    tools: Array.isArray(b.tools) ? b.tools : [],
    stream: b.stream === true,
    // `max_completion_tokens` is the current spelling; `max_tokens` is what
    // older clients (and SenClaw) still send. The newer wins when both appear.
    maxTokens: readUint(b.max_completion_tokens) ?? readUint(b.max_tokens),
    temperature:
      typeof b.temperature === 'number' && Number.isFinite(b.temperature) ? b.temperature : undefined,
    raw: b,
  };
}

/** One semantic event from a running generation. */
export type Chunk =
  /** Visible assistant text, already stripped of any chat-template markers. */
  | { kind: 'text'; text: string }
  /**
   * Chain-of-thought, shown separately by SenClaw and echoed back on the next
   * request as `reasoning_content`.
   */
  | { kind: 'reasoning'; text: string }
  /**
   * A completed tool call. Emit it whole: the SDK renders the accumulating
   * `delta.tool_calls` shape the OpenAI wire requires, so a provider never has
   * to stream partial JSON arguments and hope they reassemble.
   */
  | { kind: 'toolCall'; id: string; name: string; arguments: string }
  /**
   * Token counts for this turn. Emit at most once, at the end. SenClaw reads it
   * into its usage tracking; omitting it costs only the statistics.
   */
  | { kind: 'usage'; promptTokens: number; completionTokens: number };

/** Constructors, so a provider never hand-writes the tagged union. */
export const chunk = {
  text: (text: string): Chunk => ({ kind: 'text', text }),
  reasoning: (text: string): Chunk => ({ kind: 'reasoning', text }),
  toolCall: (id: string, name: string, args: string): Chunk => ({
    kind: 'toolCall',
    id,
    name,
    arguments: args,
  }),
  usage: (promptTokens: number, completionTokens: number): Chunk => ({
    kind: 'usage',
    promptTokens,
    completionTokens,
  }),
};

/**
 * The handle a provider writes generation events to.
 *
 * Backed by an async queue: {@link ChunkSink.send} hands each event to the
 * router, which renders it the instant it arrives rather than waiting for the
 * turn to finish. Sending after the client has disconnected is not an error — it
 * is a no-op, so a provider does not need to check. {@link ChunkSink.isClosed}
 * is there for one that would rather stop generating than finish into a void.
 */
export class ChunkSink {
  private queue: Chunk[] = [];
  private waiting: ((r: IteratorResult<Chunk>) => void) | null = null;
  /** Generation finished — `_drain` ends once the queue empties. */
  private ended = false;
  /** Consumer gave up (client disconnected) — sends become no-ops. */
  private consumerGone = false;

  /** Emit one event. A no-op once the consumer has gone. */
  send(c: Chunk): void {
    if (this.ended || this.consumerGone) return;
    if (this.waiting) {
      const resolve = this.waiting;
      this.waiting = null;
      resolve({ value: c, done: false });
    } else {
      this.queue.push(c);
    }
  }

  /** Convenience for the common case. */
  text(s: string): void {
    this.send({ kind: 'text', text: s });
  }

  /**
   * Has the receiving end gone away? A provider generating a long answer can
   * poll this to abandon a turn whose client is no longer listening.
   */
  isClosed(): boolean {
    return this.consumerGone;
  }

  // -- internal: driven by the router runtime, not by app code ---------------

  /** Generation finished; the drain loop ends once the queue drains. */
  _end(): void {
    this.ended = true;
    this._release();
  }

  /** The consumer stopped reading (client disconnected). */
  _closeFromConsumer(): void {
    this.consumerGone = true;
    this._release();
  }

  private _release(): void {
    if (this.waiting) {
      const resolve = this.waiting;
      this.waiting = null;
      resolve({ value: undefined as unknown as Chunk, done: true });
    }
  }

  /** Consume events until generation ends (or the consumer gives up). */
  async *_drain(): AsyncGenerator<Chunk> {
    for (;;) {
      // Queued items first, so anything already produced is delivered before we
      // notice the stream ended — a late send can race `_end`.
      if (this.queue.length > 0) {
        yield this.queue.shift() as Chunk;
        continue;
      }
      if (this.ended || this.consumerGone) return;
      const r = await new Promise<IteratorResult<Chunk>>((resolve) => {
        this.waiting = resolve;
      });
      if (r.done) continue; // loop: drain leftovers, then the flags return us
      yield r.value;
    }
  }
}

/** What an app implements to become a model. */
export interface LlmProvider {
  /** Every model this app can serve, right now. */
  models(): ModelCard[];

  /**
   * Run one turn, writing events to `sink` as they happen.
   *
   * Throwing after events have already been sent ends the stream early; the
   * client keeps what it received. Weights should be loaded here, **lazily** —
   * not during startup. The daemon health-gates a newly spawned app on a
   * 30-second budget with a 5-second probe timeout, so an app that loads
   * gigabytes before it binds its port is reported as failing to start, with
   * nothing in the error to say that loading was the reason.
   */
  chat(req: ChatRequest, sink: ChunkSink): Promise<void>;
}

// ============================================================================
// Rendering core — pure functions, no express, no fs
// ============================================================================

/** A mutable tool-call index counter (see {@link renderChunk}). */
export type IndexCounter = { value: number };

function errorBody(message: string, type: string): { error: { message: string; type: string } } {
  return { error: { message, type } };
}

/**
 * The `/v1/models` response body for a set of cards.
 *
 * The daemon reads the capability fields (`context_length`, `max_output_tokens`,
 * `vision`, `tools`) to build the picker entry; a plain OpenAI client ignores
 * them. `vision` in particular is **load-bearing** — the daemon decides between
 * real image blocks and the OCR fallback from it, and drops any entry that omits
 * it rather than guessing. It is always emitted here because {@link ModelCard}
 * requires it.
 */
export function renderModels(models: ModelCard[]): { object: 'list'; data: Record<string, unknown>[] } {
  return {
    object: 'list',
    data: models.map((m) => ({
      id: m.id,
      object: 'model',
      owned_by: 'senclaw-space-app',
      // Not OpenAI fields, and snake_case because that is the shape the daemon's
      // parser reads (`m["context_length"]`, ...).
      display_name: m.displayName ?? null,
      context_length: m.contextLength,
      max_output_tokens: m.maxOutputTokens,
      vision: m.vision,
      tools: m.tools ?? true,
    })),
  };
}

function baseChunkObject(id: string, model: string, delta: Record<string, unknown>): Record<string, unknown> {
  return {
    id,
    object: 'chat.completion.chunk',
    model,
    choices: [{ index: 0, delta, finish_reason: null }],
  };
}

/**
 * Render one {@link Chunk} to the JSON object of a `chat.completion.chunk`, or
 * `null` when there is nothing to send (an empty text/reasoning delta).
 *
 * The tool-call shape is the fiddly part and the reason this is not left to
 * apps: the consumer accumulates `function.name` and `function.arguments` by
 * **concatenation** across chunks at a given `index`, so a whole call must go
 * out as a single delta at a **fresh, incrementing** index. Reusing an index —
 * or sending the name twice — silently welds two calls into `get_weatherget_time`
 * with both argument objects glued together. `counter` is bumped here so each
 * call lands at its own index.
 */
export function renderChunkObject(
  id: string,
  model: string,
  c: Chunk,
  counter: IndexCounter,
): Record<string, unknown> | null {
  switch (c.kind) {
    case 'text':
      return c.text === '' ? null : baseChunkObject(id, model, { content: c.text });
    case 'reasoning':
      return c.text === '' ? null : baseChunkObject(id, model, { reasoning_content: c.text });
    case 'toolCall': {
      const index = counter.value;
      counter.value += 1;
      return baseChunkObject(id, model, {
        tool_calls: [
          {
            index,
            id: c.id,
            type: 'function',
            function: { name: c.name, arguments: c.arguments },
          },
        ],
      });
    }
    case 'usage':
      // Usage rides its OWN chunk with an empty `choices` array — the shape
      // `stream_options.include_usage` produces, and the only place the consumer
      // looks for it.
      return {
        id,
        object: 'chat.completion.chunk',
        model,
        choices: [],
        usage: {
          prompt_tokens: c.promptTokens,
          completion_tokens: c.completionTokens,
          total_tokens: c.promptTokens + c.completionTokens,
        },
      };
  }
}

/**
 * Render one {@link Chunk} to the `data:` payload string of a
 * `chat.completion.chunk` (the JSON after `data: `), or `null` when the chunk
 * emits nothing. See {@link renderChunkObject} for the tool-call index contract.
 */
export function renderChunk(id: string, model: string, c: Chunk, counter: IndexCounter): string | null {
  const obj = renderChunkObject(id, model, c, counter);
  return obj === null ? null : JSON.stringify(obj);
}

/**
 * The assembled `chat.completion` body for a non-streaming turn.
 *
 * `finish_reason` is `tool_calls` when there are calls, else `stop`. Reasoning is
 * omitted when empty rather than sent blank, and usage is attached only when the
 * provider reported it.
 */
export function renderNonStreamBody(
  model: string,
  text: string,
  reasoning: string,
  calls: unknown[],
  usage: Record<string, number> | null,
): Record<string, unknown> {
  const message: Record<string, unknown> = { role: 'assistant', content: text };
  if (reasoning !== '') message.reasoning_content = reasoning;
  if (calls.length > 0) message.tool_calls = calls;
  const out: Record<string, unknown> = {
    id: completionId(),
    object: 'chat.completion',
    model,
    choices: [
      {
        index: 0,
        message,
        finish_reason: calls.length > 0 ? 'tool_calls' : 'stop',
      },
    ],
  };
  if (usage) out.usage = usage;
  return out;
}

/**
 * `chatcmpl-<hex>`. Uniqueness only has to hold within one client's stream, so
 * a process id plus a monotonic counter is enough and pulls in no dependency.
 */
let idCounter = 0;
function completionId(): string {
  const pid = typeof process !== 'undefined' && process.pid ? process.pid : 0;
  return `chatcmpl-${pid.toString(16)}${(idCounter++).toString(16)}`;
}

// ============================================================================
// Running a provider for one request
// ============================================================================

/**
 * Drive `provider.chat` into `sink`, returning the failure message if it threw
 * (else `null`). Always ends the sink, so the drain loop terminates whether the
 * generation succeeded, threw, or produced nothing.
 */
function runGeneration(provider: LlmProvider, req: ChatRequest, sink: ChunkSink): Promise<string | null> {
  return (async () => {
    try {
      await provider.chat(req, sink);
      return null;
    } catch (err) {
      return err instanceof Error ? err.message : String(err);
    } finally {
      sink._end();
    }
  })();
}

/**
 * Run a provider for one turn and yield the SSE `data:` lines, in order, ending
 * with `data: [DONE]`.
 *
 * A generation that throws mid-stream is emitted as an **error chunk**
 * (`finish_reason: "error"`), never a silent truncation: the status line already
 * went out with the first byte, so a failure here cannot become a 5xx, and
 * ending the stream silently would make a crashed generation look like a short
 * answer — the one reading a caller cannot recover from. `[DONE]` is sent even
 * after a failure, because an unterminated stream turns a failed turn into a
 * hung one that only the client's read timeout ends.
 */
export async function* streamSse(
  provider: LlmProvider,
  req: ChatRequest,
  signal?: AbortSignal,
): AsyncGenerator<string> {
  const sink = new ChunkSink();
  const onAbort = () => sink._closeFromConsumer();
  if (signal) {
    if (signal.aborted) sink._closeFromConsumer();
    else signal.addEventListener('abort', onAbort, { once: true });
  }

  const id = completionId();
  const counter: IndexCounter = { value: 0 };
  const generation = runGeneration(provider, req, sink);

  try {
    for await (const c of sink._drain()) {
      const payload = renderChunk(id, req.model, c, counter);
      if (payload !== null) yield `data: ${payload}\n\n`;
    }
    const failure = await generation;
    if (failure !== null) {
      const errChunk = {
        id,
        object: 'chat.completion.chunk',
        model: req.model,
        choices: [{ index: 0, delta: {}, finish_reason: 'error' }],
        error: { message: failure, type: 'server_error' },
      };
      yield `data: ${JSON.stringify(errChunk)}\n\n`;
    }
    yield 'data: [DONE]\n\n';
  } finally {
    if (signal) signal.removeEventListener('abort', onAbort);
  }
}

/**
 * Run a provider for one turn and return the assembled non-streaming response as
 * `{ status, body }`.
 *
 * A generation error becomes a **500** rather than a 200 with half an answer: a
 * client that gets a truncated body and no explanation cannot tell a short reply
 * from a crash.
 */
export async function assembleNonStream(
  provider: LlmProvider,
  req: ChatRequest,
): Promise<{ status: number; body: unknown }> {
  const sink = new ChunkSink();
  const generation = runGeneration(provider, req, sink);

  let text = '';
  let reasoning = '';
  const calls: unknown[] = [];
  let usage: Record<string, number> | null = null;

  for await (const c of sink._drain()) {
    switch (c.kind) {
      case 'text':
        text += c.text;
        break;
      case 'reasoning':
        reasoning += c.text;
        break;
      case 'toolCall':
        calls.push({ id: c.id, type: 'function', function: { name: c.name, arguments: c.arguments } });
        break;
      case 'usage':
        usage = {
          prompt_tokens: c.promptTokens,
          completion_tokens: c.completionTokens,
          total_tokens: c.promptTokens + c.completionTokens,
        };
        break;
    }
  }

  const failure = await generation;
  if (failure !== null) {
    return { status: 500, body: errorBody(failure, 'server_error') };
  }
  return { status: 200, body: renderNonStreamBody(req.model, text, reasoning, calls, usage) };
}

/**
 * Framework-agnostic dispatch for one `chat/completions` request. Parses, checks
 * the model exists, and hands back one of three outcomes:
 *
 * - `error` — a parse failure (400) or an unknown model (404). An unknown model
 *   is a hard miss, never a silent default: the daemon asked for a specific id,
 *   and answering with the wrong model is worse than a clear 404.
 * - `stream` — an async generator of SSE `data:` lines (the caller asked for
 *   streaming).
 * - `json` — a promise of `{ status, body }` for a non-streaming turn.
 *
 * {@link openaiRouter} wraps this for Express; anything else can route to it
 * directly.
 */
export type ChatCompletionResult =
  | { kind: 'error'; status: number; body: unknown }
  | { kind: 'stream'; sse: AsyncGenerator<string> }
  | { kind: 'json'; json: Promise<{ status: number; body: unknown }> };

export function handleChatCompletion(
  provider: LlmProvider,
  body: unknown,
  opts: { signal?: AbortSignal } = {},
): ChatCompletionResult {
  const parsed = parseChatRequest(body);
  if ('error' in parsed) {
    return { kind: 'error', status: 400, body: errorBody(parsed.error, 'invalid_request_error') };
  }
  if (!provider.models().some((m) => m.id === parsed.model)) {
    return {
      kind: 'error',
      status: 404,
      body: errorBody(`unknown model \`${parsed.model}\``, 'invalid_request_error'),
    };
  }
  if (parsed.stream) {
    return { kind: 'stream', sse: streamSse(provider, parsed, opts.signal) };
  }
  return { kind: 'json', json: assembleNonStream(provider, parsed) };
}

// ============================================================================
// The Express router
// ============================================================================

/**
 * `GET /v1/models` + `POST /v1/chat/completions` for a provider.
 *
 * Mount it wherever the manifest's `llm.path` says — at the root when that is
 * `/v1`, or under a prefix otherwise. Handles both streaming and non-streaming
 * requests.
 *
 * `express` is a dependency already, but it is imported **lazily** — exactly as
 * `dispatchRouter` does — so importing this module from a browser bundle (for
 * the types, or the pure rendering functions) does not drag a server framework
 * in with it.
 */
export async function openaiRouter(provider: LlmProvider): Promise<Router> {
  const { Router: makeRouter, json } = await import('express');
  const router = makeRouter();
  // A vision turn carries base64 image data URLs, which are large; a stingy
  // limit rejects exactly the multipart requests a vision model exists to serve.
  router.use(json({ limit: '32mb' }));

  router.get('/v1/models', (_req: Request, res: Response) => {
    res.json(renderModels(provider.models()));
  });

  router.post('/v1/chat/completions', async (req: Request, res: Response) => {
    // Abort the generation if the client hangs up: a provider polling
    // `sink.isClosed()` can then stop rather than finish into a void.
    const controller = new AbortController();
    res.on('close', () => {
      if (!res.writableEnded) controller.abort();
    });

    const result = handleChatCompletion(provider, req.body, { signal: controller.signal });

    if (result.kind === 'error') {
      res.status(result.status).json(result.body);
      return;
    }
    if (result.kind === 'json') {
      const { status, body } = await result.json;
      res.status(status).json(body);
      return;
    }

    // Streaming. Once the status line is out nothing after it can be a 4xx/5xx —
    // a mid-stream failure rides an error chunk, terminated by `[DONE]`.
    res.status(200);
    res.setHeader('Content-Type', 'text/event-stream');
    res.setHeader('Cache-Control', 'no-cache');
    res.setHeader('Connection', 'keep-alive');
    res.flushHeaders();
    try {
      for await (const line of result.sse) {
        if (res.writableEnded || res.destroyed) break;
        res.write(line);
        // Defeat any buffering middleware (compression) so the client sees each
        // event as it is produced; a plain `res` has no `flush`, hence optional.
        (res as unknown as { flush?: () => void }).flush?.();
      }
    } finally {
      if (!res.writableEnded) res.end();
    }
  });

  return router;
}

// ============================================================================
// Model cache
// ============================================================================

/** The snake_case wire shape the daemon deserializes each card from. */
function modelCardWire(m: ModelCard): Record<string, unknown> {
  return {
    id: m.id,
    // Omitted when absent, matching the Rust SDK's `skip_serializing_if`.
    ...(m.displayName ? { display_name: m.displayName } : {}),
    context_length: m.contextLength,
    max_output_tokens: m.maxOutputTokens,
    vision: m.vision,
    tools: m.tools ?? true,
  };
}

/**
 * Write the model list to {@link MODELS_CACHE_PATH} under `appDir`, for the
 * daemon to read while this app is stopped. Call it once at startup, after the
 * models are known.
 *
 * An **empty list is refused** rather than written. The daemon treats a missing
 * cache as "not known yet" and a present one as authoritative, so clobbering a
 * good list with an empty one during a failed startup would remove the app's
 * models from the picker until someone noticed — the same rule the MCP tool
 * cache follows, for the same reason.
 *
 * Async because it imports `node:fs` lazily, keeping this module free of any
 * top-level Node import so the types and pure helpers stay safe to import
 * anywhere. The refusal throws before touching the disk.
 */
export async function publishModels(appDir: string, models: ModelCard[]): Promise<void> {
  if (!models || models.length === 0) {
    throw new Error('refusing to publish an empty model list');
  }
  const { mkdir, writeFile, rename } = await import('node:fs/promises');
  const { join, dirname } = await import('node:path');
  const path = join(appDir, MODELS_CACHE_PATH);
  await mkdir(dirname(path), { recursive: true });
  const bodyText = JSON.stringify({ models: models.map(modelCardWire) }, null, 2);
  // Write-then-rename: a daemon reading this file concurrently sees either the
  // old list or the new one, never a half-written truncation.
  const tmp = `${path}.tmp`;
  await writeFile(tmp, bodyText);
  await rename(tmp, path);
}
