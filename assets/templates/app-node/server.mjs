/**
 * {{title_name}} — a SenClaw Space App in one file, with no dependencies.
 *
 * What the daemon does with this, in order:
 *
 * 1. Reads `senclaw-manifest.json`. `requires.node` is checked before this file
 *    is ever executed.
 * 2. Runs `runtime.install` once after install or update. The stamp is keyed on
 *    the *content* of package.json and the lockfile, so an update that changes
 *    nothing does not reinstall.
 * 3. `runtime.mode: "session"` — nothing starts at boot. The app starts when the
 *    user opens it or an agent calls one of the tools below, and stops 60
 *    seconds after the last request.
 *
 * The tools stay in every agent's roster while this is stopped: the tool list is
 * cached and the MCP URL points at the daemon's proxy, which starts the app
 * before forwarding the call.
 *
 * Run it by hand during development:
 *
 *   SENCLAW_SPACE_APP_ID={{id}} PORT={{port}} node server.mjs
 */

import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { dirname, join, normalize, resolve, sep } from 'node:path';

const HERE = dirname(fileURLToPath(import.meta.url));
const WEB = join(HERE, 'web');
const APP_ID = process.env.SENCLAW_SPACE_APP_ID || '{{id}}';
const BASE = (process.env.SENCLAW_BASE_URL || 'http://127.0.0.1:18788').replace(/\/$/, '');
// This app's access token, injected by the daemon. Sent on every call to it:
// under the default strict mode a tokenless call to an app's data routes is
// refused, and a token presented against another app's id is refused always.
const TOKEN = process.env.SENCLAW_TOKEN_ACCESS_APP || '';
const API_VERSION = process.env.SENCLAW_API_VERSION || '{{api_version}}';
// Loopback by default. A Space App authenticates nothing of its own — the
// daemon reaches it over 127.0.0.1 and the UI is same-origin — so binding
// 0.0.0.0 hands the whole REST + MCP surface to anyone on the LAN. Set
// SENCLAW_BIND_HOST=0.0.0.0 to opt in to that explicitly.
const HOST = process.env.SENCLAW_BIND_HOST || '127.0.0.1';
const PORT = Number(process.env.PORT || {{port}});
const STARTED = Date.now();

// ---------------------------------------------------------------------------
// Talking to the daemon
// ---------------------------------------------------------------------------

// `missingOk` only for routes where 404 genuinely means "not set" — the config
// KV and nothing else. Treating 404 as null everywhere turns a bridge that has
// moved (an older daemon, a proxy path change, a typo in the app id) into an
// empty *successful* summary the agent cannot tell from a real one.
async function daemon(method, suffix, body, { missingOk = false } = {}) {
  const headers = { 'x-senclaw-api-version': API_VERSION };
  if (TOKEN) headers['x-senclaw-app-token'] = TOKEN;
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  const r = await fetch(`${BASE}/api/space/apps/${encodeURIComponent(APP_ID)}${suffix}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (r.status === 404 && missingOk) return null;
  const payload = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(payload?.error ?? `HTTP ${r.status}`);
  return payload;
}

/** Ask the daemon's model. The app never holds a provider API key. */
async function llm(prompt, maxTokens = 600) {
  const body = await daemon('POST', '/bridge', {
    // The wire field is `action`, not `capability`. The daemon's request struct
    // requires it, and a body without it is rejected by the JSON extractor with
    // a 422 before any handler runs.
    action: 'llm.request',
    // Only these fields are honoured — temperature and friends are not part of
    // the bridge contract and are silently dropped.
    payload: { prompt, maxTokens },
  });
  // A failed completion comes back as HTTP **200** with status "error".
  // Checking only the HTTP status turns a provider outage into a successful
  // empty summary, which the agent has no way to notice.
  if (body?.status === 'error') {
    throw new Error(body.message ?? 'model trả về lỗi không rõ');
  }
  if (body?.finish === 'length') {
    throw new Error('câu trả lời bị cắt ở maxTokens — chia nhỏ công việc ra');
  }
  return body?.text ?? body?.content ?? '';
}

/** The config KV, shared with the app's own settings UI. */
const config = {
  get: async (key, fallback = null) =>
    (await daemon('GET', `/config/${encodeURIComponent(key)}`, undefined, { missingOk: true }))
      ?.value ?? fallback,
  set: async (key, value) => daemon('PUT', `/config/${encodeURIComponent(key)}`, { value }),
};

// ---------------------------------------------------------------------------
// MCP: what agents can do with this app.
//
// The description is the only thing the model sees when choosing a tool — say
// what it does *and when to reach for it*. An error that reads like a sentence
// tells the agent what to do differently; a transport error tells it nothing.
// ---------------------------------------------------------------------------

const TOOLS = {
  '{{snake_name}}_status': {
    description:
      'Xem {{title_name}} đang chạy ra sao: thời gian hoạt động và số lần mở. Dùng khi người dùng hỏi app còn sống không.',
    inputSchema: { type: 'object', properties: {} },
    async run() {
      return { app: APP_ID, node: process.version, uptimeSecs: Math.round((Date.now() - STARTED) / 1000) };
    },
  },
  '{{snake_name}}_summarise': {
    description:
      'Tóm tắt một đoạn văn bản thành đúng ba câu. Dùng khi người dùng đưa một đoạn dài và muốn ý chính.',
    inputSchema: {
      type: 'object',
      properties: { text: { type: 'string', description: 'Đoạn văn bản cần tóm tắt.' } },
      required: ['text'],
    },
    async run({ text }) {
      if (!text?.trim()) {
        return { isError: true, content: [{ type: 'text', text: '`text` đang rỗng — truyền đoạn văn bản cần tóm tắt.' }] };
      }
      return llm(`Tóm tắt đoạn sau thành đúng ba câu:\n\n${text}`);
    },
  },
};

async function handleMcp(req) {
  const id = req?.id ?? null;
  const ok = (result) => ({ jsonrpc: '2.0', id, result });
  const err = (code, message) => ({ jsonrpc: '2.0', id, error: { code, message } });
  try {
    switch (req?.method) {
      case 'initialize':
        return ok({
          protocolVersion: '2024-11-05',
          capabilities: { tools: {} },
          serverInfo: { name: '{{mcp_name}}', version: '0.1.0' },
        });
      // SenClaw sends this as a request with an id, not a notification, and
      // ignores the reply — but erroring on it looks like a broken server.
      case 'notifications/initialized':
      case 'initialized':
      case 'ping':
        return ok({});
      case 'tools/list':
        return ok({
          tools: Object.entries(TOOLS).map(([name, t]) => ({
            name,
            description: t.description,
            inputSchema: t.inputSchema,
          })),
        });
      case 'tools/call': {
        const { name, arguments: args = {} } = req.params ?? {};
        const tool = TOOLS[name];
        if (!tool) return err(-32602, `không có tool tên ${name} (đang có: ${Object.keys(TOOLS).join(', ')})`);
        const out = await tool.run(args);
        if (out && typeof out === 'object' && 'content' in out) return ok(out);
        return ok({ content: [{ type: 'text', text: typeof out === 'string' ? out : JSON.stringify(out, null, 2) }] });
      }
      default:
        return err(-32601, `method not found: ${req?.method}`);
    }
  } catch (e) {
    return err(-32603, String(e?.message ?? e));
  }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

const MIME = {
  '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css',
  '.json': 'application/json', '.svg': 'image/svg+xml', '.png': 'image/png',
};

const server = createServer(async (req, res) => {
  const url = new URL(req.url, `http://${req.headers.host}`);
  const send = (status, body, type = 'application/json') => {
    const buf = Buffer.isBuffer(body) ? body : Buffer.from(typeof body === 'string' ? body : JSON.stringify(body));
    res.writeHead(status, { 'Content-Type': type, 'Content-Length': buf.length });
    res.end(buf);
  };

  // runtime.healthPath. The daemon waits on this before it calls the app
  // started and polls it afterwards, so it must stay cheap and never block.
  if (url.pathname === '/api/status') {
    return send(200, { ok: true, app: APP_ID, uptimeSecs: Math.round((Date.now() - STARTED) / 1000) });
  }

  if (url.pathname === '/api/visit' && req.method === 'POST') {
    try {
      const visits = Number(await config.get('visits', 0)) + 1;
      await config.set('visits', visits);
      return send(200, { visits });
    } catch (e) {
      return send(502, { error: String(e?.message ?? e) });
    }
  }

  if (url.pathname === '/api/mcp/sse' && req.method === 'POST') {
    const chunks = [];
    for await (const c of req) chunks.push(c);
    let payload;
    try {
      payload = JSON.parse(Buffer.concat(chunks).toString() || '{}');
    } catch {
      return send(400, { jsonrpc: '2.0', id: null, error: { code: -32700, message: 'parse error' } });
    }
    return send(200, await handleMcp(payload));
  }

  // Static UI. Resolve first, then confirm the result is still inside the web
  // root — the check a hand-rolled static handler usually forgets, and the one
  // that stops `../../etc/passwd` from being served.
  const rel = url.pathname === '/' ? 'index.html' : normalize(url.pathname).replace(/^[/\\]+/, '');
  const target = resolve(WEB, rel);
  if (!target.startsWith(WEB + sep) && target !== WEB) return send(403, { error: 'forbidden' });
  try {
    return send(200, await readFile(target), MIME[target.slice(target.lastIndexOf('.'))] ?? 'application/octet-stream');
  } catch {
    // Unknown paths are client-side routes in a single-page app, not 404s.
    try {
      return send(200, await readFile(join(WEB, 'index.html')), 'text/html');
    } catch {
      return send(404, { error: 'not found', path: url.pathname });
    }
  }
});

// A session app is stopped when it goes idle: SIGTERM to the process group,
// SIGKILL about two seconds later. Close and flush; do not start new work.
for (const sig of ['SIGTERM', 'SIGINT']) {
  process.on(sig, () => {
    console.log(`[{{id}}] ${sig} — shutting down`);
    server.close(() => process.exit(0));
    setTimeout(() => process.exit(0), 1500).unref();
  });
}

server.listen(PORT, HOST, () => console.log(`[{{id}}] listening on http://${HOST}:${PORT}`));
