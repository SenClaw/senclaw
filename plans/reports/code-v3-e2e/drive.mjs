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
const lastReply = new Map(); // jid -> text of the newest agent:reply
const replyLog = [];
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
  if (m.type === 'agent:reply' && jid) { lastReply.set(jid, m.text ?? m.content ?? ''); replyLog.push([jid, m.text ?? m.content ?? '']); }
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
const sawText = (tag, needle) => mockReqs().filter((r) => r.tag === tag).some((r) => (r.lastText ?? '').includes(needle));

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
const reply3 = lastReply.get(jid) ?? '';
const r3 = results('S3').join('\n');
check('S3 syntax error is reported', /Syntax check: this edit leaves/.test(r3), r3.slice(0, 400));
check('S3 and asked nothing', since(n0).length === 0);

// ---- S4: a check after the edit means no nudge; none means one nudge
await say(jid, steps('S4', [
  { t: 'Write', a: { file_path: `${PROJ}/t.py`, content: 'print("t")\n' } },
  { t: 'Bash', a: { command: 'python3 -m pytest -q t.py', description: 'e2e step' } },
]));
const nudgeText = 'You edited source files';
const s4nudged = mockReqs().filter((r) => r.tag === 'S4').some((r) => (r.lastText ?? '').includes(nudgeText));
check('S4 verified edit is not nudged', !s4nudged);
check('S4 and its reply carries no warning', !/⚠️ Not verified/.test(lastReply.get(jid) ?? ''), lastReply.get(jid));
const s3nudged = mockReqs().filter((r) => r.tag === 'S3').some((r) => (r.lastText ?? '').includes(nudgeText));
check('S3 an edit that nothing ran was sent back', s3nudged);
check('S3 the note names the check this conversation already ran (S1: python3 hello.py)', sawText('S3', 'Run `python3 hello.py` with the Bash tool'));
check('S3 an answer that still skips it is sent back a second time, naming the call', sawText('S3', 'call the Bash tool now with `python3 hello.py`'));
check('S3 and then ends with the warning on the reply', /⚠️ Not verified/.test(reply3) && reply3.includes('`python3 hello.py`') && reply3.includes('MOCK-DONE S3'), JSON.stringify(replyLog.filter(([j]) => j === jid).slice(-4)).slice(0, 700));
const s1nudged = mockReqs().filter((r) => r.tag === 'S1').some((r) => (r.lastText ?? '').includes(nudgeText));
check('S1 an edit followed by running the program is not nudged', !s1nudged);

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


// ============ run guards: stuck detection, file safety, limits, finish checks ============
const lastResults = (tag) => {
  const rs = mockReqs().filter((r) => r.tag === tag);
  return rs.length ? rs[rs.length - 1].prevResults ?? [] : [];
};
const bash = (command) => ({ t: 'Bash', a: { command, description: 'e2e step' } });
const setLimits = (o) => api('/api/agent-behavior', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(o) });

const lim = await api('/api/agent-behavior');
check('G0 limits default to 500 turns / 10M tokens / no judge', lim.body.codeMaxTurns === 500 && lim.body.maxRunTokens === 10000000 && lim.body.finishJudge === false, JSON.stringify(lim.body));
check('G0 a silly turn cap is a 400', (await setLimits({ codeMaxTurns: 3 })).status === 400);

// ---- S10: test-fix-retest. An identical command after an edit must run; with nothing changed it is answered from memory.
n0 = cards.length;
await say(jid, steps('S10', [
  { t: 'Write', a: { file_path: `${PROJ}/calc.py`, content: 'print(1)\n' } },
  bash('python3 calc.py'),
  { t: 'Edit', a: { file_path: `${PROJ}/calc.py`, old_string: 'print(1)', new_string: 'print(2)' } },
  bash('python3 calc.py'),
  { t: 'Read', a: { file_path: `${PROJ}/calc.py` } },
  { t: 'Read', a: { file_path: `${PROJ}/calc.py` } },
]));
const r10 = lastResults('S10');
check('S10 first run prints 1', (r10[1] ?? '').trim() === '1', JSON.stringify(r10[1]));
check('S10 the identical retest after the edit RAN (prints 2)', (r10[3] ?? '').trim() === '2', JSON.stringify(r10[3]));
check('S10 a read-only call repeated with nothing changed is intercepted', /Duplicate call/.test(r10[5] ?? '') && !/Duplicate call/.test(r10[4] ?? ''), JSON.stringify(r10.slice(4)));
check('S10 and asked nothing', since(n0).length === 0);

// ---- S11: the same result again and again is stuck: one nudge, then the run is stopped
await say(jid, steps('S11', Array.from({ length: 10 }, () => bash('python3 calc.py'))), 120000);
check('S11 nudged after 4 identical results', sawText('S11', 'exactly the same result'));
check('S11 stopped for good after 4 more', sawText('S11', 'tool access is now disabled'));

// ---- S12: exit codes are visible, and "done" after a red check is sent back once
await say(jid, steps('S12', [
  { t: 'Write', a: { file_path: `${PROJ}/t2.py`, content: 'print("t2")\n' } },
  bash('python3 -m pytest -q nonexistent_file_xyz.py'),
]));
const r12 = lastResults('S12');
check('S12 a failing command shows its exit code', /\[exit code [1-9]\d*\]/.test(r12[1] ?? ''), JSON.stringify(r12[1]));
check('S12 finishing after a red check is sent back', sawText('S12', 'Your last build/test/lint run'));

// ---- S13: undo takes back the last edit; the write is atomic (no temp files left)
await say(jid, steps('S13', [
  { t: 'Edit', a: { file_path: `${PROJ}/hello.py`, old_string: 'print("v2")', new_string: 'print("v3")' } },
  { t: 'Edit', a: { file_path: `${PROJ}/hello.py`, undo: true } },
]));
const r13 = lastResults('S13');
check('S13 undo reverts', /Reverted the last edit/.test(r13[1] ?? ''), JSON.stringify(r13[1]));
const hello = fs.readFileSync(`${PROJ}/hello.py`, 'utf8');
check('S13 file is back to v2', hello.includes('print("v2")') && !hello.includes('v3'), hello);
check('S13 no temp file left behind', !fs.readdirSync(PROJ).some((f) => f.endsWith('.tmp')), fs.readdirSync(PROJ).join(','));

// ---- S14: the turn cap of a coding request is configurable
await setLimits({ codeMaxTurns: 10 });
await say(jid, steps('S14', Array.from({ length: 14 }, (_, i) => bash(`python3 -c "print(${i + 100})"`))), 120000);
check('S14 a 10-turn cap forces the final answer', sawText('S14', 'tool budget for this task is exhausted'));
await setLimits({ codeMaxTurns: 500 });

// ---- S15: the token budget stops a run
await setLimits({ maxRunTokens: 40 });
await say(jid, steps('S15', Array.from({ length: 8 }, (_, i) => bash(`python3 -c "print(${i + 200})"`))), 120000);
check('S15 a 40-token budget forces the final answer', sawText('S15', 'tool budget for this task is exhausted'));
const ran15 = mockReqs().filter((r) => r.tag === 'S15').length;
check('S15 and after only a few calls', ran15 <= 6, String(ran15));
await setLimits({ maxRunTokens: 10000000 });

// ---- S16: a user PreStop hook can refuse to let the agent stop
check('S16 the PreStop hook sent the agent back once', sawText('S1', 'PRESTOP-GATE'));


// ---- S17: the optional finish judge sends an unfinished request back, then lets it end
await setLimits({ finishJudge: true });
await say(jid, steps('S17', [
  { t: 'Write', a: { file_path: `${PROJ}/j.py`, content: 'print("j")\n' } },
  bash('python3 -m pytest -q nonexistent_j.py'),
]), 120000);
check('S17 the judge found something missing and the agent was sent back', sawText('S17', 'JUDGE-MISSING-ITEM'));
check('S17 and the judge was asked twice, no more', mockReqs().filter((r) => r.judge).length === 2, String(mockReqs().filter((r) => r.judge).length));
await setLimits({ finishJudge: false });

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
