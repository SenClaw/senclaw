// A real local model through the new agent loop: a code session in a git repo with a one-character bug.
import fs from 'node:fs';
import path from 'node:path';
import { execSync } from 'node:child_process';
const HERE = path.dirname(new URL(import.meta.url).pathname);
const S = path.join(HERE, 'real');
const UI = 'http://127.0.0.1:48788', WSURL = 'ws://127.0.0.1:48789';
for (const u of [UI, WSURL]) if (/:(18788|18789)\b/.test(u)) throw new Error('refusing the real daemon ports');
const PROJ = path.join(S, 'proj');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const t0 = Date.now();
const log = (...a) => console.log(`[${String(Math.round((Date.now() - t0) / 1000)).padStart(4)}s]`, ...a);
const api = async (p, init) => { const r = await fetch(UI + p, init); const t = await r.text(); try { return JSON.parse(t); } catch { return t; } };

const cards = [], tools = [], replies = [];
const ws = new WebSocket(WSURL);
let idle;
await new Promise((res, rej) => { ws.onopen = () => { ws.send(JSON.stringify({ type: 'connect' })); setTimeout(res, 800); }; ws.onerror = () => rej(new Error('no ws')); });
let busy = false;
ws.onmessage = (e) => {
  const m = JSON.parse(e.data);
  if (m.type === 'permission:request') { cards.push({ tool: m.toolName, title: m.title }); log('CARD', m.toolName, '|', m.title); ws.send(JSON.stringify({ type: 'permission:response', requestId: m.requestId, optionKey: 'refuse' })); }
  if (m.type === 'question:request') { log('QUESTION asked of the user'); cards.push({ tool: 'AskUserQuestion' }); ws.send(JSON.stringify({ type: 'question:response', requestId: m.requestId, answers: { 0: 0 } })); }
  if (m.type === 'tool:execution') { const c = m.content ?? {}; tools.push(m.toolName); log('TOOL', m.toolName, '|', (m.title ?? '').slice(0, 90), '|', (m.summary ?? '').slice(0, 60)); }
  if (m.type === 'agent:reply') { replies.push(m.text ?? m.content ?? ''); }
  if (m.type === 'agent:state') { const s = JSON.stringify(m.state ?? ''); if (/processing|thinking|running/i.test(s)) busy = true; if (/idle/i.test(s) && busy) { busy = false; idle?.(); } }
  if (m.type === 'session:error' || m.type === 'error') log('ERROR EVENT', JSON.stringify(m).slice(0, 200));
};

const created = await api('/api/code/sessions', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ name: 'real', workspace: PROJ }) });
const jid = created.id;
log('session', jid);
const prompt = process.argv[2] ?? 'The function add in calc.py returns the wrong result. Fix it so that `python3 check.py` prints OK, and run that command to confirm.';
const done = new Promise((res, rej) => { idle = res; setTimeout(() => rej(new Error('timeout')), 25 * 60 * 1000); });
ws.send(JSON.stringify({ type: 'message', groupJid: jid, text: prompt }));
try { await done; } catch (e) { log('TIMEOUT'); }
await sleep(1500);

const fixed = (() => { try { return execSync('python3 check.py', { cwd: PROJ, stdio: ['ignore', 'pipe', 'pipe'] }).toString().trim(); } catch (e) { return 'FAILS: ' + (e.stderr?.toString() ?? e.message).trim().split('\n').pop(); } })();
const stats = await api('/api/code/autonomy');
log('--- RESULT');
log('check.py now:', fixed);
log('calc.py now:', JSON.stringify(fs.readFileSync(`${PROJ}/calc.py`, 'utf8')));
log('tools used:', tools.join(' → '));
log('approval cards:', cards.length, JSON.stringify(cards));
log('autonomy stats:', JSON.stringify(stats));
log('final reply:', (replies.at(-1) ?? '').slice(0, 600).replace(/\n/g, ' '));
fs.writeFileSync(`${S}/result.json`, JSON.stringify({ fixed, tools, cards, stats, reply: replies.at(-1) }, null, 1));
ws.close();
process.exit(0);
