// Shared helpers for the Project P0 spike drivers (Node 22+: global WebSocket + fetch).
import fs from 'node:fs';
import path from 'node:path';

// Where run-<label> directories live (override with P0_RUNS).
export const SP = process.env.P0_RUNS ?? '/tmp/senclaw-p0-runs';
// Ports of the scratch daemon (override with P0_UI / P0_WS when another session holds these).
export const UI = `http://127.0.0.1:${process.env.P0_UI ?? 38788}`;
export const WSURL = `ws://127.0.0.1:${process.env.P0_WS ?? 38789}`;

// Never let a driver reach the user's real daemon.
for (const u of [UI, WSURL]) if (/:(18788|18789)\b/.test(u)) throw new Error('refusing the real daemon ports');

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
export const stamp = () => new Date().toISOString().slice(11, 23);
export const log = (...a) => console.log(stamp(), ...a);

/** Build the directive the mock LLM replays. */
export const steps = (tag, list) => `run script ${tag} §§STEPS${JSON.stringify({ tag, steps: list })}§§`;
/** Same, for a prompt that itself travels inside another directive (a DAG task for a worker). */
export const wsteps = (tag, list) => `run script ${tag} ¶¶STEPS${JSON.stringify({ tag, steps: list })}¶¶`;

export async function connect(runDir) {
  const events = fs.createWriteStream(path.join(runDir, 'log', 'ws-events.jsonl'), { flags: 'a' });
  const ws = new WebSocket(WSURL);
  const state = { busy: new Set(), idleWaiters: new Map(), last: new Map(), errors: [] };
  await new Promise((resolve, reject) => {
    ws.onopen = () => {
      ws.send(JSON.stringify({ type: 'connect' })); // admin
      setTimeout(resolve, 800);
    };
    ws.onerror = () => reject(new Error(`cannot reach ${WSURL}`));
  });
  ws.onmessage = (e) => {
    const m = JSON.parse(e.data);
    events.write(JSON.stringify({ t: Date.now(), m }) + '\n');
    if (m.type === 'error') {
      state.errors.push(m.message);
      log('gateway error:', m.message);
    }
    if (m.type === 'permission:request') {
      ws.send(JSON.stringify({ type: 'permission:response', requestId: m.requestId, optionKey: 'allow' }));
    }
    if (m.type === 'question:request') {
      ws.send(JSON.stringify({ type: 'question:response', requestId: m.requestId, answers: { 0: 0 } }));
    }
    const jid = m.groupJid ?? m.jid ?? m.chatJid;
    if (m.type === 'agent:state' && jid) {
      const s = JSON.stringify(m.state ?? '');
      state.last.set(jid, s);
      if (/processing|thinking|running/i.test(s)) state.busy.add(jid);
      if (/idle/i.test(s) && state.busy.has(jid)) {
        state.busy.delete(jid);
        state.idleWaiters.get(jid)?.();
      }
    }
  };
  return {
    ws,
    state,
    send: (o) => ws.send(JSON.stringify(o)),
    register(o) {
      ws.send(JSON.stringify({ type: 'register:group', channel: '', ...o }));
    },
    /** Send a chat message and resolve when that chat goes processing → idle. */
    async say(jid, text, timeoutMs = 90000) {
      const done = new Promise((resolve, reject) => {
        state.idleWaiters.set(jid, resolve);
        setTimeout(() => reject(new Error(`no idle for ${jid} within ${timeoutMs / 1000}s (last state ${state.last.get(jid)})`)), timeoutMs);
      });
      ws.send(JSON.stringify({ type: 'message', groupJid: jid, text }));
      await done;
    },
    close: () => ws.close(),
  };
}

export async function api(pathname, init) {
  const r = await fetch(`${UI}${pathname}`, init);
  const text = await r.text();
  let body;
  try {
    body = JSON.parse(text);
  } catch {
    body = text;
  }
  return { status: r.status, body };
}

/** Read the mock's request log. */
export function mockLog(runDir) {
  const f = path.join(runDir, 'mock', 'requests.jsonl');
  if (!fs.existsSync(f)) return [];
  return fs
    .readFileSync(f, 'utf8')
    .split('\n')
    .filter(Boolean)
    .map((l) => JSON.parse(l));
}
