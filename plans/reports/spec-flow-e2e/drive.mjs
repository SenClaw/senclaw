// Spec-flow e2e: a real daemon, a scripted LLM, a real git project. Walks CUJ 1-4 of
// plans/261027-1505-code-spec-flow/design.md and the edges (the store, REST chat, specFlow off).
// node drive.mjs   (after up.sh)
import fs from 'node:fs';
import path from 'node:path';

const HERE = path.dirname(new URL(import.meta.url).pathname);
const S = path.join(HERE, 'run');
const UI = 'http://127.0.0.1:48798';
const WSURL = 'ws://127.0.0.1:48799';
for (const u of [UI, WSURL]) if (/:(18788|18789)\b/.test(u)) throw new Error('refusing the real daemon ports');
const PROJ = fs.realpathSync(path.join(S, 'proj'));
const MOCK = path.join(S, 'mock');
const STORE = path.join(S, 'home/.senclaw/specs');

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const log = (...a) => console.log(new Date().toISOString().slice(11, 23), ...a);
const steps = (tag, list) => `run script ${tag} §§STEPS${JSON.stringify({ tag, steps: list })}§§`;
const d = new Date();
const yymmdd = `${String(d.getFullYear()).slice(2)}${String(d.getMonth() + 1).padStart(2, '0')}${String(d.getDate()).padStart(2, '0')}`;
const SPEC = `docs/designs/${yymmdd}-add-greeting-feature.md`;
const SPEC_ABS = path.join(PROJ, SPEC);
const HELLO = path.join(PROJ, 'hello.py');

async function api(p, init) {
  const r = await fetch(`${UI}${p}`, init);
  const t = await r.text();
  try { return { status: r.status, body: JSON.parse(t) }; } catch { return { status: r.status, body: t }; }
}
const post = (p, body) => api(p, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });

const replies = []; // [jid, text]
const waiters = new Map(); // jid -> resolve(text) for the next agent:reply
const idle = new Map();
const busy = new Set();
const ws = new WebSocket(WSURL);
await new Promise((res, rej) => { ws.onopen = () => { ws.send(JSON.stringify({ type: 'connect' })); setTimeout(res, 800); }; ws.onerror = () => rej(new Error('no ws')); });
ws.onmessage = (e) => {
  const m = JSON.parse(e.data);
  if (m.type === 'permission:request') ws.send(JSON.stringify({ type: 'permission:response', requestId: m.requestId, optionKey: 'allow_once' }));
  if (m.type === 'question:request') ws.send(JSON.stringify({ type: 'question:response', requestId: m.requestId, answers: { 0: 0 } }));
  const jid = m.groupJid ?? m.jid ?? m.chatJid;
  if (m.type === 'agent:reply' && jid) {
    const text = m.text ?? m.content ?? '';
    replies.push([jid, text]);
    const w = waiters.get(jid);
    if (w) { waiters.delete(jid); w(text); }
  }
  if (m.type === 'agent:state' && jid) {
    const s = JSON.stringify(m.state ?? '');
    if (/processing|thinking|running/i.test(s)) busy.add(jid);
    if (/idle/i.test(s) && busy.has(jid)) { busy.delete(jid); idle.get(jid)?.(); }
  }
};
/** An agent turn: wait until the chat is idle again. */
async function say(jid, text, timeoutMs = 120000) {
  const done = new Promise((res, rej) => { idle.set(jid, res); setTimeout(() => rej(new Error(`no idle for ${jid} after: ${text.slice(0, 60)}`)), timeoutMs); });
  ws.send(JSON.stringify({ type: 'message', groupJid: jid, text }));
  await done;
  await sleep(500);
}
/** A command the gateway answers itself: the next agent:reply of the chat. */
async function command(jid, text, timeoutMs = 15000) {
  const got = new Promise((res, rej) => { waiters.set(jid, res); setTimeout(() => rej(new Error(`no reply to ${text}`)), timeoutMs); });
  ws.send(JSON.stringify({ type: 'message', groupJid: jid, text }));
  return got;
}
const mockReqs = () => (fs.existsSync(`${MOCK}/requests.jsonl`) ? fs.readFileSync(`${MOCK}/requests.jsonl`, 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l)) : []);
const results = (tag) => mockReqs().filter((r) => r.tag === tag).flatMap((r) => r.prevResults ?? []);
// The memory extraction that runs after a turn reads the same conversation and is tagged too: not a step.
const lastResults = (tag) => { const all = mockReqs().filter((r) => r.tag === tag && !r.extraction && !r.lens); return all.length ? all[all.length - 1].prevResults ?? [] : []; };
const bodiesWith = (needle) => fs.readdirSync(`${MOCK}/bodies`).map((f) => fs.readFileSync(`${MOCK}/bodies/${f}`, 'utf8')).filter((b) => b.includes(needle));
const lensCalls = () => mockReqs().filter((r) => r.lens);
const stepsFile = (kind, tag, list) => fs.writeFileSync(`${MOCK}/spec-steps-${kind}.json`, JSON.stringify({ tag, steps: list }));
const lensAnswers = (o) => fs.writeFileSync(`${MOCK}/lens-answers.json`, JSON.stringify(o));
const state = () => {
  if (!fs.existsSync(STORE)) return null;
  for (const k of fs.readdirSync(STORE)) {
    const f = path.join(STORE, k, 'state.json');
    if (fs.existsSync(f)) return JSON.parse(fs.readFileSync(f, 'utf8'));
  }
  return null;
};
const hello = () => fs.readFileSync(HELLO, 'utf8');
const editHello = (marker) => ({ t: 'Edit', a: { file_path: HELLO, old_string: 'return "hi"', new_string: `return "hi"  # ${marker}` } });
const resetHello = () => fs.writeFileSync(HELLO, 'def hello():\n    return "hi"\n\nprint(hello())\n');

let pass = 0, fail = 0;
const check = (name, ok, detail = '') => { (ok ? pass++ : fail++); log(ok ? 'PASS' : 'FAIL', name, ok ? '' : String(detail).slice(0, 600)); };

const BRIEF = `# Add greeting feature\n*   **Status:** Brief\n*   **Date:** 2026-10-27\n## Overview\nGreet by name.\n## The Problem\nhello() ignores the name.\n## Goals\n* hello(name) greets the name\n## Non-Goals\n* i18n\n## Open Questions\n## Anchored Hypothesis\nAdd a name parameter.\n## Options\n* A parameter\n`;
const FULL = `# Add greeting feature\n*   **Status:** Draft\n*   **Date:** 2026-10-27\n## Overview\nGreet by name.\n## The Problem\nhello() ignores the name.\n## Goals\n* hello(name) greets the name. Keep it small.\n## Non-Goals\n* i18n\n## Open Questions\n## Critical User Journeys\n* Given a name, when hello runs, then it greets it.\n## Technical Plan\nA default parameter.\n## Alternatives Considered\n* A new function: more code.\n## Risks\n* None worth a mitigation.\n## Detailed Implementation Plan\n### Target Files\n#### 1. \`hello.py\` (Modify)\n* add name=\"world\"\n### Tasks\n- [ ] T1 add the parameter\n### Verification Plan\n* \`python3 hello.py\`\n## Review Log\n`;

// ---- two Code sessions on the same repository
const mk = async (name) => {
  const c = await post('/api/code/sessions', { name, workspace: PROJ });
  const id = c.body.id ?? c.body.jid ?? c.body.session_id;
  if (!id) throw new Error(`no session id: ${JSON.stringify(c.body)}`);
  return id;
};
const A = await mk('spec-a');
log('session A', A);

// S0: default
const beh = await api('/api/agent-behavior');
check('S0 default specFlow is required', beh.body.specFlow === 'required', JSON.stringify(beh.body));

// S1: required, no spec — the edit is refused before any card
await say(A, steps('S1', [editHello('S1')]));
check('S1 required: edit refused with NO_SPEC', results('S1').some((r) => r.includes('No design spec for this folder yet')), results('S1'));
check('S1 file untouched', !hello().includes('S1'), hello());
check('S1 the answer says nothing was changed', replies.some(([j, t]) => j === A && t.includes('Blocked by the spec flow') && t.includes('hello.py')), replies.slice(-2));

// S2: /spec skip lets this chat edit
const skipped = await command(A, '/spec skip');
check('S2 /spec skip answered by the gateway', skipped.includes('skipped for this chat'), skipped);
await say(A, steps('S2', [editHello('S2')]));
check('S2 after skip the edit lands', hello().includes('S2'), results('S2'));

// S3: reset clears the skip
const reset = await command(A, 'reset');
check('S3 reset answered', /reset/i.test(reset), reset);
resetHello();
await say(A, steps('S3', [editHello('S3')]));
check('S3 after reset the edit is refused again', results('S3').some((r) => r.includes('No design spec')) && !hello().includes('S3'), results('S3'));

// S4: /brainstorm opens the spec; the brief is written, code stays locked
stepsFile('BRAINSTORM', 'B1', [{ t: 'Write', a: { file_path: SPEC_ABS, content: BRIEF } }, editHello('B1')]);
await say(A, '/brainstorm add greeting feature');
let st = state();
check('S4 state opened the spec at the daemon-chosen path', st?.active === SPEC, JSON.stringify(st));
check('S4 brief written', fs.existsSync(SPEC_ABS));
const b1 = lastResults('B1');
check('S4 code edit refused in the design phase', b1.some((r) => r.includes('is brief, not approved')) && !hello().includes('B1'), b1);
check('S4 the agent got the BRAINSTORM instructions', mockReqs().some((r) => r.tag === 'B1' && /\[Spec flow — BRAINSTORM\]/.test(r.lastText ?? '') ) || bodiesWith('[Spec flow — BRAINSTORM]').length > 0);

// S5: /review of a brief runs the four lenses in brainstorm mode
const lens0 = lensCalls().length;
await say(A, '/review');
st = state();
const r5 = st?.specs?.[SPEC]?.review;
check('S5 brief reviewed in brainstorm mode, complete', r5?.mode === 'brainstorm' && r5?.complete === true, JSON.stringify(r5));
const l5 = lensCalls().slice(lens0).map((r) => r.lens).join(',');
check('S5 four lenses, in order', l5 === 'canh,kiem,hieu,phan_bien', l5);
const lensFiles = fs.readdirSync(MOCK).filter((f) => f.startsWith('lens-'));
check('S5 brainstorm roles were asked', lensFiles.some((f) => fs.readFileSync(`${MOCK}/${f}`, 'utf8').includes('contrarian')));
check('S5 the agent got the REVIEW RESULTS turn', bodiesWith('[Spec flow — REVIEW RESULTS]').length > 0);
check('S5 report saved', r5 && fs.existsSync(path.join(STORE, fs.readdirSync(STORE)[0], r5.report)), JSON.stringify(r5));

// S6: a brief cannot be approved, not even with force
const ap6 = await command(A, '/approve force');
check('S6 /approve force refuses a brief', ap6.includes('no "## Detailed Implementation Plan" yet'), ap6);

// S7: /draft writes the full spec
stepsFile('DRAFT', 'D1', [{ t: 'Write', a: { file_path: SPEC_ABS, content: FULL } }]);
await say(A, '/draft');
check('S7 full spec written', fs.readFileSync(SPEC_ABS, 'utf8').includes('## Detailed Implementation Plan'));
check('S7 the agent got the DRAFT instructions', bodiesWith('[Spec flow — DRAFT]').length > 0);

// S8: a blocker from Kiểm blocks /approve
lensAnswers({ kiem: '[BLOCKER] Tasks :: T1 does not say which function :: name hello()' });
await say(A, '/review');
st = state();
check('S8 review recorded 1 blocker, design mode', st?.specs?.[SPEC]?.review?.blocker === 1 && st.specs[SPEC].review.mode === 'design', JSON.stringify(st?.specs?.[SPEC]?.review));
const ap8 = await command(A, '/approve');
check('S8 /approve refused for the blocker', ap8.includes('1 blocker(s)'), ap8);
check('S8 an untouched spec sends the triage back once', mockReqs().filter((r) => (r.lastText ?? '').includes('You have not changed')).length >= 1);
check('S8 the answer says the design is unchanged with its blocker', replies.some(([j, t]) => j === A && t.includes('📋') && t.includes('unchanged') && t.includes('1 blocker(s)')), replies.slice(-2));

// S8b: an unreadable lens leaves the review incomplete
lensAnswers({ hieu: 'Looks good to me overall!' });
await say(A, '/review');
const ap8b = await command(A, '/approve');
check('S8b unreadable lens: review incomplete, /approve refused', ap8b.includes('did not finish (hieu: unparsed)'), ap8b);

// S9: no blockers, some majors: an agent that still edits the design is held off, so the review
// stays the current one and /approve takes it (it used to go stale after every such review)
lensAnswers({ hieu: '[MAJOR] Goals :: the wording is vague :: say what changes' });
const design9 = fs.readFileSync(SPEC_ABS, 'utf8');
stepsFile('REVIEW_RESULTS', 'R9', [{ t: 'Write', a: { file_path: SPEC_ABS, content: `${design9}\nExtra design text.\n` } }]);
await say(A, '/review');
fs.rmSync(`${MOCK}/spec-steps-REVIEW_RESULTS.json`);
check('S9 the design that passed cannot change during the review turn', lastResults('R9').some((r) => r.includes('passed its review with no blockers')) && fs.readFileSync(SPEC_ABS, 'utf8') === design9, lastResults('R9'));
check('S9 the answer says /approve unlocks the code', replies.some(([j, t]) => j === A && t.includes('📋') && t.includes('/approve unlocks the code')), replies.filter(([j]) => j === A).slice(-1));
const ap9 = await command(A, '/approve');
check('S9 /approve after a clean review', ap9.startsWith(`Approved ${SPEC} (version `), ap9);
check('S9 approval recorded via web', state()?.specs?.[SPEC]?.approval?.via === 'web');

// S10: approved, but this chat has not read it: the first change is refused with the spec in the refusal
await say(A, steps('A1', [editHello('A1'), editHello('A1')]));
const a1 = lastResults('A1');
check('S10 first change refused, the approved spec handed over in the refusal', a1[0]?.includes('this chat had not read it') && a1[0]?.includes(`<spec_file path="${SPEC}">`) && a1[0]?.includes('## Detailed Implementation Plan'), a1[0]?.slice(0, 300));
check('S10 the same change made again lands, no Read needed', hello().includes('A1'), a1);

// S11: a second session sees the spec and must read it too
const B = await mk('spec-b');
await say(B, steps('B2', [editHello('B2')]));
const withCapsule = bodiesWith('run script B2').filter((b) => b.includes('<spec>'));
check('S11 the new chat carries the <spec> block', withCapsule.length > 0 && withCapsule[0].includes(SPEC) && withCapsule[0].includes('Stage: approved'), withCapsule.length);
check('S11 the new chat is handed the spec before its first change', results('B2').some((r) => r.includes('<spec_file')), results('B2'));
await say(B, steps('B3', [editHello('B3')]));
check('S11 then the new chat edits', hello().includes('B3'), lastResults('B3'));

// S12: ticking a task does not lock; changing the design does
const ticked = fs.readFileSync(SPEC_ABS, 'utf8').replace('- [ ] T1', '- [x] T1');
await say(A, steps('C0', [{ t: 'Write', a: { file_path: SPEC_ABS, content: ticked } }, editHello('C0')]));
check('S12 ticking a task keeps the spec approved', hello().includes('C0'), lastResults('C0'));
await say(A, steps('C1', [{ t: 'Edit', a: { file_path: SPEC_ABS, old_string: 'Keep it small.', new_string: 'Keep it small and log every call.' } }, editHello('C1')]));
check('S12 a design change after approval locks code', lastResults('C1').some((r) => r.includes('changed after it was approved')) && !hello().includes('C1'), lastResults('C1'));
const status = await command(A, '/spec');
check('S12 /spec shows stage changed and the task count', status.includes('stage: changed') && status.includes('tasks: 1/1'), status);

// S13: the store cannot be written by a tool
await say(A, steps('X1', [{ t: 'Write', a: { file_path: path.join(STORE, 'forged', 'state.json'), content: '{}' } }]));
check('S13 writing the spec store is refused', results('X1').some((r) => r.includes("spec flow's own records")), results('X1'));

// S14: the Code-session REST chat answers spec commands itself
const rest = await post(`/api/code/sessions/${A}/chat`, { prompt: '/spec' });
check('S14 REST chat answers /spec', typeof rest.body.reply === 'string' && rest.body.reply.includes(SPEC), JSON.stringify(rest.body).slice(0, 300));

// S15: /spec done closes it; required then wants a spec again
const done = await command(A, '/spec done');
check('S15 /spec done closes the spec', done.startsWith(`Closed ${SPEC}`) && !state()?.active, done);
await say(A, steps('E1', [editHello('E1')]));
check('S15 required with no open spec refuses again', results('E1').some((r) => r.includes('No design spec')), results('E1'));

// S16: specFlow off turns it all off
const off = await post('/api/agent-behavior', { specFlow: 'off' });
check('S16 specFlow off saved', off.body.specFlow === 'off', JSON.stringify(off.body));
const offReply = await command(A, '/spec');
check('S16 commands say the flow is off', offReply.startsWith('Spec flow is off'), offReply);
await say(A, steps('O1', [editHello('O1')]));
check('S16 edits land with the flow off', hello().includes('O1'), lastResults('O1'));
const bad = await post('/api/agent-behavior', { specFlow: 'sometimes' });
check('S16 an unknown specFlow is a 400', bad.status === 400, bad.status);

log(`DONE pass=${pass} fail=${fail}`);
fs.writeFileSync(path.join(HERE, 'result.json'), JSON.stringify({ pass, fail, at: new Date().toISOString() }, null, 1));
ws.close();
process.exit(fail ? 1 : 0);
