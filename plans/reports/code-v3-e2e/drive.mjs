// Code v3 e2e: a real daemon, a scripted LLM, a real git project. Counts the approval cards the agent causes.
// node drive.mjs   (after up.sh)
import fs from 'node:fs';
import path from 'node:path';
import { execSync } from 'node:child_process';

const HERE = path.dirname(new URL(import.meta.url).pathname);
const S = path.join(HERE, 'run');
const UI = 'http://127.0.0.1:48788';
const WSURL = 'ws://127.0.0.1:48789';
for (const u of [UI, WSURL]) if (/:(18788|18789)\b/.test(u)) throw new Error('refusing the real daemon ports');
const PROJ = path.join(S, 'proj');
const OUTSIDE = path.join(S, 'outside');

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const log = (...a) => console.log(new Date().toISOString().slice(11, 23), ...a);
const steps = (tag, list) => `run script ${tag} §§STEPS${JSON.stringify({ tag, steps: list })}§§`;

async function api(p, init) {
  const r = await fetch(`${UI}${p}`, init);
  const t = await r.text();
  try { return { status: r.status, body: JSON.parse(t) }; } catch { return { status: r.status, body: t }; }
}

const cards = []; // every permission:request the agent caused
let answerWith = 'refuse';
const ws = new WebSocket(WSURL);
const idle = new Map();
const busy = new Set();
await new Promise((res, rej) => { ws.onopen = () => { ws.send(JSON.stringify({ type: 'connect' })); setTimeout(res, 800); }; ws.onerror = () => rej(new Error('no ws')); });
ws.onmessage = (e) => {
  const m = JSON.parse(e.data);
  if (m.type === 'permission:request') {
    cards.push({ tool: m.toolName ?? m.tool_name, title: m.title, content: m.content, jid: m.groupJid ?? m.jid });
    ws.send(JSON.stringify({ type: 'permission:response', requestId: m.requestId, optionKey: answerWith }));
  }
  if (m.type === 'question:request') ws.send(JSON.stringify({ type: 'question:response', requestId: m.requestId, answers: { 0: 0 } }));
  const jid = m.groupJid ?? m.jid ?? m.chatJid;
  if (m.type === 'agent:state' && jid) {
    const s = JSON.stringify(m.state ?? '');
    if (/processing|thinking|running/i.test(s)) busy.add(jid);
    if (/idle/i.test(s) && busy.has(jid)) { busy.delete(jid); idle.get(jid)?.(); }
  }
};
async function say(jid, text, timeoutMs = 90000) {
  const done = new Promise((res, rej) => { idle.set(jid, res); setTimeout(() => rej(new Error(`no idle for ${jid}`)), timeoutMs); });
  ws.send(JSON.stringify({ type: 'message', groupJid: jid, text }));
  await done;
  await sleep(600);
}
const mockReqs = () => (fs.existsSync(`${S}/mock/requests.jsonl`) ? fs.readFileSync(`${S}/mock/requests.jsonl`, 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l)) : []);
const results = (tag) => mockReqs().filter((r) => r.tag === tag).flatMap((r) => r.prevResults ?? []);

let pass = 0, fail = 0;
const check = (name, ok, detail = '') => { (ok ? pass++ : fail++); log(ok ? 'PASS' : 'FAIL', name, ok ? '' : detail); };
const since = (n) => cards.slice(n);

// ---- a code session pinned to the project (init_git false: it already is a repo)
const created = await api('/api/code/sessions', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ name: 'e2e', workspace: PROJ }) });
log('created', created.status, JSON.stringify(created.body).slice(0, 200));
const jid = created.body.id ?? created.body.jid ?? created.body.session_id;
if (!jid) throw new Error('no session id');

const lvl = await api('/api/agent-behavior');
check('default level is auto', lvl.body.codeAutonomy === 'auto', JSON.stringify(lvl.body));

// ---- S1: the dev loop runs with no card
let n0 = cards.length;
await say(jid, steps('S1', [
  { t: 'Write', a: { file_path: `${PROJ}/hello.py`, content: 'def hello():\n    return "hi"\n\nprint(hello())\nprint("v2")\n' } },
  { t: 'Bash', a: { command: 'python3 hello.py', description: 'e2e step' } },
  { t: 'Bash', a: { command: 'git add -A', description: 'e2e step' } },
  { t: 'Bash', a: { command: 'git commit -q -m "update hello"', description: 'e2e step' } },
  { t: 'Bash', a: { command: 'mkdir -p build && touch build/x.txt', description: 'e2e step' } },
  { t: 'Bash', a: { command: 'rm -rf build', description: 'e2e step' } },
]));
check('S1 dev loop asked nothing', since(n0).length === 0, JSON.stringify(since(n0)));
const log1 = execSync('git log --oneline', { cwd: PROJ }).toString();
check('S1 the commit really happened', /update hello/.test(log1), log1);
check('S1 build/ was created and removed', !fs.existsSync(`${PROJ}/build`));

// ---- S2: an Edit miss shows the file's own lines
n0 = cards.length;
await say(jid, steps('S2', [
  { t: 'Edit', a: { file_path: `${PROJ}/hello.py`, old_string: 'def hello():\n    return "hello"', new_string: 'def hello():\n    return "hey"' } },
]));
check('S2 edit miss asked nothing', since(n0).length === 0);
const r2 = results('S2').join('\n');
check('S2 miss names the closest lines', /closest text in hello\.py is lines 1-2/.test(r2) && /return "hi"/.test(r2), r2.slice(0, 400));

// ---- S3: a syntax error is reported by the edit itself
n0 = cards.length;
await say(jid, steps('S3', [
  { t: 'Write', a: { file_path: `${PROJ}/bad.py`, content: 'def f(:\n    pass\n' } },
]));
const r3 = results('S3').join('\n');
check('S3 syntax error is reported', /Syntax check: this edit leaves/.test(r3), r3.slice(0, 400));
check('S3 and asked nothing', since(n0).length === 0);

// ---- S4: a check after the edit means no nudge; none means one nudge
await say(jid, steps('S4', [
  { t: 'Write', a: { file_path: `${PROJ}/t.py`, content: 'print("t")\n' } },
  { t: 'Bash', a: { command: 'python3 -m pytest -q t.py', description: 'e2e step' } },
]));
const nudgeText = 'You edited source files but have not run anything that checks them';
const s4nudged = mockReqs().filter((r) => r.tag === 'S4').some((r) => (r.lastText ?? '').includes(nudgeText));
check('S4 verified edit is not nudged', !s4nudged);
const s1nudged = mockReqs().filter((r) => r.tag === 'S1').some((r) => (r.lastText ?? '').includes(nudgeText));
check('S1 unverified edit was nudged once', s1nudged);

// ---- S5: what cannot be taken back asks, with the reason
n0 = cards.length;
await say(jid, steps('S5', [
  { t: 'Bash', a: { command: 'git push origin main', description: 'e2e step' } },
  { t: 'Bash', a: { command: 'rm -rf ~/e2e-nope-dir', description: 'e2e step' } },
  { t: 'Write', a: { file_path: `${OUTSIDE}/x.txt`, content: 'nope' } },
]));
const c5 = since(n0);
check('S5 three cards', c5.length === 3, JSON.stringify(c5.map((c) => c.title)));
check('S5 push card says why', c5.some((c) => /publishes commits/.test(c.title ?? '')), JSON.stringify(c5.map((c) => c.title)));
check('S5 outside write card says why', c5.some((c) => /outside the project folder/.test(c.title ?? '')), JSON.stringify(c5.map((c) => c.title)));
check('S5 refused file was not written', !fs.existsSync(`${OUTSIDE}/x.txt`));

// ---- S6: ask mode prompts for the same dev command
await api('/api/agent-behavior', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ codeAutonomy: 'ask' }) });
n0 = cards.length;
await say(jid, steps('S6', [{ t: 'Bash', a: { command: 'python3 hello.py', description: 'e2e step' } }]));
check('S6 ask mode prompts for python3 hello.py', since(n0).length === 1, JSON.stringify(since(n0)));
await api('/api/agent-behavior', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ codeAutonomy: 'auto' }) });
const bad = await api('/api/agent-behavior', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ codeAutonomy: 'sometimes' }) });
check('S6 junk level is a 400', bad.status === 400, JSON.stringify(bad));

// ---- S7: back to auto, applies on the next turn without a restart
n0 = cards.length;
await say(jid, steps('S7', [{ t: 'Bash', a: { command: 'python3 hello.py', description: 'e2e step' } }]));
check('S7 auto again, no card', since(n0).length === 0, JSON.stringify(since(n0)));

// ---- S8: an ordinary chat in a non-repository keeps asking
ws.send(JSON.stringify({ type: 'register:group', channel: '', folder: 'plain', name: 'plain', jid: 'web:plain' }));
await sleep(800);
n0 = cards.length;
await say('web:plain', steps('S8', [{ t: 'Bash', a: { command: 'python3 -c "print(1)"', description: 'e2e step' } }]));
check('S8 a plain chat still asks', since(n0).length === 1, JSON.stringify(since(n0)));

// ---- S9: the prompt the model saw
const bodies = fs.readdirSync(`${S}/mock/bodies`).map((f) => fs.readFileSync(`${S}/mock/bodies/${f}`, 'utf8'));
const s1body = bodies.find((b) => b.includes('S1') && b.includes('AGENTS-MARKER')) ?? '';
check('S9 repo instructions reach the model', s1body.includes('AGENTS-MARKER'));
check('S9 detected commands reach the model', s1body.includes('<project_commands>') && s1body.includes('cargo test'));
const sysDir = `${S}/mock/sys`;
const sysAll = fs.readdirSync(sysDir).map((f) => fs.readFileSync(`${sysDir}/${f}`, 'utf8')).join('\n');
check('S9 working-style reminder in the system prompt', sysAll.includes('# Working in this project') && sysAll.includes('run on their own'));

// ---- counters
const st = await api('/api/code/autonomy');
log('autonomy stats', JSON.stringify(st.body));
check('counters: ran unattended > 10, asked anyway counted', st.body.ranUnattended >= 10 && st.body.askedAnyway >= 3, JSON.stringify(st.body));

log(`done: ${pass} passed, ${fail} failed; cards total ${cards.length}`);
fs.writeFileSync(`${S}/result.json`, JSON.stringify({ pass, fail, cards, stats: st.body }, null, 1));
ws.close();
process.exit(fail ? 1 : 0);
