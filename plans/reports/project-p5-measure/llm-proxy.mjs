#!/usr/bin/env node
// A logging pass-through in front of the real model, so a measurement can see what the daemon sent and what the model
// answered without the daemon knowing. It changes nothing: method, path, body and streaming go through as they came.
//   node llm-proxy.mjs --target http://<host>:<port> --port 53931 --out <dir>
// Logs <out>/requests.jsonl (one line per call: seq, card, kind, ms, tools offered, tool calls made, text) and
// <out>/bodies/NNNN.json (the request body). The card is read from the worker's own instructions ("Kanban task #N").
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';

const args = Object.fromEntries(process.argv.slice(2).flatMap((a, i, all) => (a.startsWith('--') ? [[a.slice(2), all[i + 1]]] : [])));
const TARGET = (args.target ?? '').replace(/\/$/, '');
const PORT = Number(args.port ?? 53931);
const OUT = args.out ?? '.';
if (!TARGET) throw new Error('--target is required');
if (/:(18788|18789|18790)\b/.test(TARGET)) throw new Error('refusing the real daemon ports');
fs.mkdirSync(path.join(OUT, 'bodies'), { recursive: true });
const LOG = path.join(OUT, 'requests.jsonl');
let seq = 0;

const textOf = (c) => (typeof c === 'string' ? c : Array.isArray(c) ? c.map((p) => (typeof p === 'string' ? p : p.text ?? '')).join('\n') : '');

/** Assistant text and tool calls out of a response body, streamed (SSE) or not. */
function digest(raw, contentType) {
  const out = { text: '', reasoning: '', calls: [], finish: null };
  const add = (choice) => {
    const d = choice.delta ?? choice.message ?? {};
    if (typeof d.content === 'string') out.text += d.content;
    if (typeof d.reasoning_content === 'string') out.reasoning += d.reasoning_content;
    if (typeof d.reasoning === 'string') out.reasoning += d.reasoning;
    for (const tc of d.tool_calls ?? []) {
      const i = tc.index ?? out.calls.length;
      out.calls[i] ??= { name: '', arguments: '' };
      if (tc.function?.name) out.calls[i].name += tc.function.name;
      if (tc.function?.arguments) out.calls[i].arguments += tc.function.arguments;
    }
    if (choice.finish_reason) out.finish = choice.finish_reason;
  };
  try {
    if (/event-stream/.test(contentType) || raw.trimStart().startsWith('data:')) {
      for (const line of raw.split('\n')) {
        if (!line.startsWith('data:')) continue;
        const payload = line.slice(5).trim();
        if (!payload || payload === '[DONE]') continue;
        for (const ch of JSON.parse(payload).choices ?? []) add(ch);
      }
    } else {
      for (const ch of JSON.parse(raw).choices ?? []) add(ch);
    }
  } catch (e) {
    out.parseError = String(e).slice(0, 120);
  }
  return out;
}

http
  .createServer(async (req, res) => {
    const parts = [];
    for await (const c of req) parts.push(c);
    const body = Buffer.concat(parts);
    const n = ++seq;
    const t0 = Date.now();
    let json = null;
    try {
      json = JSON.parse(body.toString('utf8'));
    } catch {
      /* not JSON (a GET) */
    }
    const messages = json?.messages ?? [];
    const whole = JSON.stringify(messages);
    const card = whole.match(/Kanban task #(\d+)/)?.[1] ?? null;
    const tools = (json?.tools ?? []).length;
    const lastUser = messages.filter((m) => m.role === 'user').map((m) => textOf(m.content)).at(-1) ?? '';
    const kind = !json ? 'other' : tools > 0 ? (card ? 'worker' : 'chat') : /Kanban task "/.test(lastUser) ? 'retain' : 'other';
    if (json) fs.writeFileSync(path.join(OUT, 'bodies', `${String(n).padStart(4, '0')}.json`), body);
    const record = { seq: n, t: new Date().toISOString(), method: req.method, url: req.url, kind, card, tools, nMessages: messages.length, stream: !!json?.stream };
    try {
      const headers = { 'content-type': req.headers['content-type'] ?? 'application/json' };
      if (req.headers.authorization) headers.authorization = req.headers.authorization;
      const up = await fetch(TARGET + req.url, { method: req.method, headers, body: ['GET', 'HEAD'].includes(req.method) ? undefined : body });
      const out = {};
      for (const [k, v] of up.headers) if (!['content-encoding', 'content-length', 'transfer-encoding', 'connection'].includes(k)) out[k] = v;
      res.writeHead(up.status, out);
      const acc = [];
      for await (const chunk of up.body) {
        acc.push(Buffer.from(chunk));
        res.write(chunk);
      }
      res.end();
      const raw = Buffer.concat(acc).toString('utf8');
      Object.assign(record, { status: up.status, ms: Date.now() - t0 }, kind === 'other' ? {} : { answer: digest(raw, up.headers.get('content-type') ?? '') });
      if (up.status >= 400) record.errorBody = raw.slice(0, 400);
    } catch (e) {
      Object.assign(record, { status: 502, ms: Date.now() - t0, error: String(e).slice(0, 200) });
      if (!res.headersSent) res.writeHead(502, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ error: { message: `proxy: ${String(e).slice(0, 200)}` } }));
    }
    fs.appendFileSync(LOG, JSON.stringify(record) + '\n');
  })
  .listen(PORT, '127.0.0.1', () => console.log(`llm proxy on ${PORT} -> ${TARGET}`));
