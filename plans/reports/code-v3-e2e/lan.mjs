// A model on the network through the new agent loop: a code session in a bare git folder, several requests in a row.
// node lan.mjs '["first request", "second request"]'   (after up-lan.sh)
import fs from 'node:fs';
import path from 'node:path';
import { execSync } from 'node:child_process';
const HERE = path.dirname(new URL(import.meta.url).pathname);
const S = path.join(HERE, 'lan');
const UI = 'http://127.0.0.1:48788', WSURL = 'ws://127.0.0.1:48789';
for (const u of [UI, WSURL]) if (/:(18788|18789)\b/.test(u)) throw new Error('refusing the real daemon ports');
const PROJ = path.join(S, 'proj');
const PROMPTS = JSON.parse(process.argv[2]);
const t0 = Date.now();
const log = (...a) => console.log(`[${String(Math.round((Date.now() - t0) / 1000)).padStart(4)}s]`, ...a);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const api = async (p, init) => { const r = await fetch(UI + p, init); const t = await r.text(); try { return JSON.parse(t); } catch { return t; } };

const cards = [], replies = [];
let turnTools = [], busy = false, idle;
const ws = new WebSocket(WSURL);
await new Promise((res, rej) => { ws.onopen = () => { ws.send(JSON.stringify({ type: 'connect' })); setTimeout(res, 800); }; ws.onerror = () => rej(new Error('no ws')); });
ws.onmessage = (e) => {
  const m = JSON.parse(e.data);
  if (m.type === 'permission:request') { cards.push({ tool: m.toolName, title: m.title }); log('CARD', m.toolName, '|', m.title); ws.send(JSON.stringify({ type: 'permission:response', requestId: m.requestId, optionKey: 'refuse' })); }
  if (m.type === 'tool:execution') { turnTools.push(m.toolName); log('TOOL', m.toolName, '|', (m.title ?? '').slice(0, 100), '|', String(m.summary ?? m.result ?? m.output ?? '').replace(/\n/g, ' ').slice(0, 160)); }
  if (m.type === 'agent:reply') replies.push(m.text ?? m.content ?? '');
  if (m.type === 'agent:state') { const s = JSON.stringify(m.state ?? ''); if (/processing|thinking|running/i.test(s)) busy = true; if (/idle/i.test(s) && busy) { busy = false; idle?.(); } }
  if (m.type === 'session:error' || m.type === 'error') log('ERROR EVENT', JSON.stringify(m).slice(0, 200));
};

const runTests = () => {
  // however the model laid its tests out: top-level discovery, then a tests/ folder with or without __init__.py
  const tries = ['python3 -m unittest discover', 'python3 -m unittest discover -s tests -t .', 'python3 -m unittest discover -s tests'];
  let last = '';
  for (const c of tries) {
    try { const out = execSync(c + ' 2>&1', { cwd: PROJ, stdio: ['ignore', 'pipe', 'pipe'], timeout: 60000 }).toString().trim(); if (!/NO TESTS RAN/.test(out)) return `${c} -> ` + out.split('\n').slice(-3).join(' | '); last = out; }
    catch (e) { last = ((e.stdout?.toString() ?? '') + (e.stderr?.toString() ?? '')).trim(); }
  }
  return 'FAILS or none ran: ' + last.split('\n').slice(-4).join(' | ');
};
const created = await api('/api/code/sessions', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ name: 'lan', workspace: PROJ }) });
const jid = created.id;
log('session', jid);
const results = [];
for (const [i, prompt] of PROMPTS.entries()) {
  turnTools = []; replies.length = 0; busy = false;
  const t = Date.now();
  const done = new Promise((res, rej) => { idle = res; setTimeout(() => rej(new Error('timeout')), Number(process.env.TURN_MINUTES ?? 45) * 60 * 1000); });
  log(`--- request ${i + 1}: ${prompt.slice(0, 90)}`);
  ws.send(JSON.stringify({ type: 'message', groupJid: jid, text: prompt }));
  try { await done; } catch { log('TIMEOUT'); }
  await sleep(1500);
  results.push({ request: i + 1, seconds: Math.round((Date.now() - t) / 1000), tools: turnTools, replies: [...replies] });
  log(`request ${i + 1}: ${results.at(-1).seconds}s, tools: ${turnTools.join(' -> ')}`);
  log('final reply:', (replies.at(-1) ?? '').slice(0, 900).replace(/\n/g, ' '));
  log('tests after this request:', runTests());
}
const tests = runTests();
log('--- RESULT');
log('python3 -m unittest in the project now:', tests);
log('approval cards:', cards.length, JSON.stringify(cards));
log('autonomy stats:', JSON.stringify(await api('/api/code/autonomy')));
fs.writeFileSync(`${S}/result.json`, JSON.stringify({ results, tests, cards }, null, 1));
ws.close();
process.exit(0);
