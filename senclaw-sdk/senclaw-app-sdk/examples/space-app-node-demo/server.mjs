/**
 * A complete Space App in Node, in one file, with no dependencies.
 *
 * What it demonstrates, in the order the daemon exercises it:
 *
 * 1. `requires.node` is checked before this file is ever executed.
 * 2. `runtime.install` runs once after install or update — here `npm install`
 *    with an empty dependency list, so it is instant. The stamp is keyed on the
 *    *content* of package.json and the lockfile, so an update that changes
 *    nothing does not reinstall.
 * 3. `runtime.mode: "session"` — the daemon does not start this at boot. It
 *    starts when the user opens the app or an agent calls one of the tools
 *    below, and stops it 60 seconds after the last request.
 * 4. Its tools stay in every agent's roster while it is stopped: the tool list
 *    is cached and the MCP URL points at the daemon's proxy, which starts the
 *    app before forwarding.
 *
 * Run it by hand:  SENCLAW_SPACE_APP_ID=node-demo PORT=4820 node server.mjs
 */

import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { dirname, join, normalize, resolve, sep } from 'node:path';

const HERE = dirname(fileURLToPath(import.meta.url));
const WEB = join(HERE, 'web');
const APP_ID = process.env.SENCLAW_SPACE_APP_ID || 'node-demo';
const BASE = (process.env.SENCLAW_BASE_URL || 'http://127.0.0.1:18788').replace(/\/$/, '');
// Loopback by default. A Space App authenticates nothing of its own — the
// daemon reaches it over 127.0.0.1 and the UI is same-origin — so binding
// 0.0.0.0 hands the whole REST + MCP surface to anyone on the LAN.
const HOST = process.env.SENCLAW_BIND_HOST || '127.0.0.1';
const PORT = Number(process.env.PORT || 4820);
const STARTED = Date.now();

/** Call the daemon's bridge. The app never holds a provider API key. */
async function llm(prompt, maxTokens = 600) {
  const r = await fetch(`${BASE}/api/space/apps/${encodeURIComponent(APP_ID)}/bridge`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ capability: 'llm.request', payload: { prompt, maxTokens } }),
  });
  const body = await r.json();
  if (!r.ok) throw new Error(body?.error ?? `HTTP ${r.status}`);
  if (body?.finish === 'length') {
    throw new Error('the reply was truncated at maxTokens — split the work into chunks');
  }
  return body?.text ?? body?.content ?? '';
}

// ---------------------------------------------------------------------------
// MCP: JSON-RPC over HTTP POST. Three methods is the whole protocol surface
// SenClaw's client uses, so there is no need for the MCP SDK here.
// ---------------------------------------------------------------------------

const TOOLS = {
  node_demo_env: {
    description: 'Report the Node runtime this Space App is running on',
    inputSchema: { type: 'object', properties: {} },
    async run() {
      return {
        node: process.version,
        platform: `${process.platform}-${process.arch}`,
        uptimeSecs: Math.round((Date.now() - STARTED) / 100) / 10,
      };
    },
  },
  node_demo_summarise: {
    description: 'Summarise a piece of text in three sentences',
    inputSchema: {
      type: 'object',
      properties: { text: { type: 'string', description: 'The text to summarise' } },
      required: ['text'],
    },
    async run({ text }) {
      if (!text?.trim()) {
        // A readable sentence, not a stack trace: the agent needs to know what
        // to do differently, and a JSON-RPC error tells it nothing.
        return { content: [{ type: 'text', text: '`text` is empty — pass the text to summarise.' }], isError: true };
      }
      return llm(`Summarise the following in exactly three sentences:\n\n${text}`);
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
          serverInfo: { name: 'node-demo-mcp', version: '1.0.0' },
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
            name, description: t.description, inputSchema: t.inputSchema,
          })),
        });
      case 'tools/call': {
        const { name, arguments: args = {} } = req.params ?? {};
        const tool = TOOLS[name];
        if (!tool) {
          return err(-32602, `unknown tool: ${name} (have: ${Object.keys(TOOLS).join(', ')})`);
        }
        const out = await tool.run(args);
        if (out && typeof out === 'object' && 'content' in out) return ok(out);
        const text = typeof out === 'string' ? out : JSON.stringify(out, null, 2);
        return ok({ content: [{ type: 'text', text }] });
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

const MIME = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css',
               '.json': 'application/json', '.svg': 'image/svg+xml' };

const server = createServer(async (req, res) => {
  const url = new URL(req.url, `http://${req.headers.host}`);
  const send = (status, body, type = 'application/json') => {
    const buf = Buffer.isBuffer(body) ? body : Buffer.from(typeof body === 'string' ? body : JSON.stringify(body));
    res.writeHead(status, { 'Content-Type': type, 'Content-Length': buf.length });
    res.end(buf);
  };

  if (url.pathname === '/api/status') {
    return send(200, { ok: true, app: APP_ID, uptimeSecs: Math.round((Date.now() - STARTED) / 1000) });
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
    const ext = target.slice(target.lastIndexOf('.'));
    return send(200, await readFile(target), MIME[ext] ?? 'application/octet-stream');
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
    console.log(`[node-demo] ${sig} — shutting down`);
    server.close(() => process.exit(0));
    setTimeout(() => process.exit(0), 1500).unref();
  });
}

server.listen(PORT, HOST, () => console.log(`[node-demo] listening on http://${HOST}:${PORT}`));
