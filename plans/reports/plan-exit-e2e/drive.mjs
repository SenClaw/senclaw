// Plan approval + skill guard e2e: a real daemon, a scripted LLM, a real git project.
//   ./up.sh <senclaw binary> && node drive.mjs [label] && ./down.sh
// The same script runs against a binary from before the fix (label "before") and after it (label "after").
import fs from 'node:fs';
import path from 'node:path';

const HERE = path.dirname(new URL(import.meta.url).pathname);
const S = path.join(HERE, 'run');
const LABEL = process.argv[2] ?? 'after';
const UI = 'http://127.0.0.1:48790';
const WSURL = 'ws://127.0.0.1:48791';
for (const u of [UI, WSURL]) if (/:(18788|18789|18790)\b/.test(u)) throw new Error('refusing the real daemon ports');
const PROJ = path.join(S, 'proj');

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const log = (...a) => console.log(new Date().toISOString().slice(11, 23), ...a);
const steps = (tag, list, extra = {}) => `run script ${tag} §§STEPS${JSON.stringify({ tag, steps: list, ...extra })}§§`;

async function api(p, init) {
  const r = await fetch(`${UI}${p}`, init);
  const t = await r.text();
  try { return { status: r.status, body: JSON.parse(t) }; } catch { return { status: r.status, body: t }; }
}

const planRequests = []; // every plan:exit:request, in order
const modeChanges = []; // [jid, mode]
const lastReply = new Map();
const replies = [];
let planAnswer = 'startEditing';
const ws = new WebSocket(WSURL);
const idle = new Map();
const busy = new Set();
await new Promise((res, rej) => { ws.onopen = () => { ws.send(JSON.stringify({ type: 'connect' })); setTimeout(res, 800); }; ws.onerror = () => rej(new Error('no ws')); });
ws.onmessage = (e) => {
  const m = JSON.parse(e.data);
  const jid = m.groupJid ?? m.jid ?? m.chatJid;
  if (m.type === 'plan:exit:request') {
    planRequests.push({ jid, plan: m.planContent ?? '' });
    ws.send(JSON.stringify({ type: 'plan:exit:response', groupJid: jid, agentId: m.agentId ?? 'main', selected: planAnswer }));
  }
  if (m.type === 'agent:mode:changed') modeChanges.push([jid, m.mode]);
  if (m.type === 'permission:request') ws.send(JSON.stringify({ type: 'permission:response', requestId: m.requestId, optionKey: 'allow' }));
  if (m.type === 'question:request') ws.send(JSON.stringify({ type: 'question:response', requestId: m.requestId, answers: { 0: 0 } }));
  if (m.type === 'agent:reply' && jid) { lastReply.set(jid, m.text ?? m.content ?? ''); replies.push([jid, m.text ?? m.content ?? '']); }
  if (m.type === 'agent:state' && jid) {
    const s = JSON.stringify(m.state ?? '');
    if (/processing|thinking|running/i.test(s)) busy.add(jid);
    if (/idle/i.test(s) && busy.has(jid)) { busy.delete(jid); idle.get(jid)?.(); }
  }
};
async function say(jid, text, timeoutMs = 60000) {
  const done = new Promise((res, rej) => { idle.set(jid, res); setTimeout(() => rej(new Error(`no idle for ${jid}`)), timeoutMs); });
  ws.send(JSON.stringify({ type: 'message', groupJid: jid, text }));
  await done.catch((e) => log('WARN', e.message));
  await sleep(600);
}
const setMode = async (jid, mode) => { ws.send(JSON.stringify({ type: 'agent:mode', groupJid: jid, mode })); await sleep(500); };
const mockReqs = () => (fs.existsSync(`${S}/mock/requests.jsonl`) ? fs.readFileSync(`${S}/mock/requests.jsonl`, 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l)) : []);
const reqs = (tag) => mockReqs().filter((r) => r.tag === tag);
const results = (tag) => reqs(tag).flatMap((r) => r.prevResults ?? []);
const sawText = (tag, needle) => reqs(tag).filter((r) => (r.lastText ?? '').includes(needle)).length;
const session = async (name) => {
  const c = await api('/api/code/sessions', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ name, workspace: PROJ }) });
  const jid = c.body.id ?? c.body.jid ?? c.body.session_id;
  if (!jid) throw new Error(`no session id: ${JSON.stringify(c.body)}`);
  return jid;
};

let pass = 0, fail = 0;
const check = (name, ok, detail = '') => { (ok ? pass++ : fail++); log(ok ? 'PASS' : 'FAIL', name, ok ? '' : detail); };
const mine = (jid) => planRequests.filter((p) => p.jid === jid);
const exists = (f) => fs.existsSync(path.join(PROJ, f));

// ---- P1: plan in prose, ExitPlanMode, "clear context and start fresh": the run must still know the request
const j1 = await session('p1');
await setMode(j1, 'Plan');
planAnswer = 'clearContextAndStart';
await say(j1, steps('P1', [
  { t: '@text', a: { text: 'P1-TEXT: here is my plan, in prose.' } },
  { t: 'ExitPlanMode', a: { plan: 'P1-PLAN: 1. write todo.py' } },
], { afterRestart: [{ t: 'Write', a: { file_path: `${PROJ}/todo.py`, content: 'print("todo")\n' } }] }));
check('P1 the person was asked once, with the plan', mine(j1).length === 1 && mine(j1)[0].plan.includes('P1-PLAN'), JSON.stringify(mine(j1)));
check('P1 a text-only reply in Plan mode was sent back for ExitPlanMode', sawText('P1', 'You must finish by calling `ExitPlanMode') >= 1);
const restart = reqs('P1').find((r) => r.restarted);
check('P1 the run restarted after the approval', !!restart, JSON.stringify(reqs('P1').map((r) => r.roles)).slice(0, 500));
check('P1 the restart message carries the request', !!restart && restart.lastText.includes('run script P1'), restart?.lastText);
check('P1 and the plan', !!restart && restart.lastText.includes('P1-PLAN: 1. write todo.py'), restart?.lastText);
check('P1 and no Chinese stub', !!restart && !/按照以下计划/.test(restart.lastText), restart?.lastText);
check('P1 the old conversation is gone (no assistant turn in the request)', !!restart && restart.roles.every((r) => !r.startsWith('assistant')), JSON.stringify(restart?.roles));
check('P1 the task carried on after the restart: todo.py was written', exists('todo.py'));
check('P1 the run finished', /MOCK-DONE P1/.test(lastReply.get(j1) ?? ''), lastReply.get(j1));
check('P1 the mode went back to Agent', modeChanges.some(([j, m]) => j === j1 && m === 'Agent'), JSON.stringify(modeChanges));

// ---- P2: an empty plan asks nobody; the run is sent back for the real one
const j2 = await session('p2');
await setMode(j2, 'Plan');
planAnswer = 'startEditing';
await say(j2, steps('P2', [
  { t: 'ExitPlanMode', a: { plan: '' } },
  { t: '@text', a: { text: 'P2-TEXT the plan is above.' } },
  { t: 'ExitPlanMode', a: { plan: 'P2-PLAN: write p2.txt' } },
  { t: 'Write', a: { file_path: `${PROJ}/p2.txt`, content: 'ok\n' } },
]));
check('P2 only the real plan reached the person', mine(j2).length === 1 && mine(j2)[0].plan.includes('P2-PLAN'), JSON.stringify(mine(j2)));
check('P2 the empty call was refused, naming the argument', results('P2').some((t) => t.startsWith('Error: ExitPlanMode was called with an empty `plan`') && t.includes('`plan` argument')), JSON.stringify(results('P2')).slice(0, 400));
check('P2 a text-only reply after the refusal was still sent back for the plan', sawText('P2', 'You must finish by calling `ExitPlanMode') >= 1);
check('P2 after the approval the work was done', exists('p2.txt') && /MOCK-DONE P2/.test(lastReply.get(j2) ?? ''), lastReply.get(j2));

// ---- P3: a skill that works through tools, loaded, and an answer from memory: sent back, then the tool is called
const j3 = await session('p3');
await setMode(j3, 'Agent');
await say(j3, steps('P3', [
  { t: 'Skill', a: { skill: 'web-research' } },
  { t: '@text', a: { text: 'P3-FABRICATED: gold is 1 019 000 dong per luong (made up).' } },
  { t: 'Bash', a: { command: 'echo looked-up', description: 'e2e step' } },
]));
check('P3 the skill loaded', results('P3').some((t) => t.includes('Skill Activated: web-research')), JSON.stringify(results('P3')).slice(0, 300));
check('P3 an answer straight after the load was sent back once, naming the skill', sawText('P3', 'You loaded the skill `web-research`') === 1, String(sawText('P3', 'You loaded the skill')));
check('P3 and the tool was then called', results('P3').some((t) => t.includes('looked-up')));
check('P3 and the final reply carries no warning', /MOCK-DONE P3/.test(lastReply.get(j3) ?? '') && !/Not looked up/.test(lastReply.get(j3) ?? ''), lastReply.get(j3));

// ---- P4: a model that keeps answering from memory: sent back twice, then the reply says so
const j4 = await session('p4');
await setMode(j4, 'Agent');
await say(j4, steps('P4', [
  { t: 'Skill', a: { skill: 'web-research' } },
  { t: '@text', a: { text: 'P4-FABRICATED: gold is 1 019 000 dong per luong (made up).', stubborn: true } },
]));
check('P4 sent back exactly twice', sawText('P4', 'You loaded the skill `web-research`') === 2, String(sawText('P4', 'You loaded the skill')));
const r4 = lastReply.get(j4) ?? '';
check('P4 the reply still carries the made-up text, and the warning', r4.includes('P4-FABRICATED') && r4.includes('⚠️ Not looked up') && r4.includes('`web-research`'), r4);

// ---- P5: the restart brings the first-turn context back: the date and the project's instructions
const j5 = await session('p5');
await setMode(j5, 'Plan');
planAnswer = 'clearContextAndStart';
await say(j5, steps('P5', [
  { t: '@text', a: { text: 'P5-TEXT: here is my plan, in prose.' } },
  { t: 'ExitPlanMode', a: { plan: 'P5-PLAN: 1. write p5.txt' } },
], { afterRestart: [{ t: 'Write', a: { file_path: `${PROJ}/p5.txt`, content: 'ok\n' } }] }));
const first5 = reqs('P5')[0];
const restart5 = reqs('P5').find((r) => r.restarted);
const count = (text, needle) => text.split(needle).length - 1;
check('P5 the first request of the chat carried the date and the project instructions', !!first5 && first5.lastText.includes("Today's date is") && first5.lastText.includes('P5-PROJECT-MARKER'), first5?.lastText?.slice(0, 300));
check('P5 the restart message carries them again', !!restart5 && restart5.lastText.includes("Today's date is") && restart5.lastText.includes('P5-PROJECT-MARKER'), restart5?.lastText?.slice(0, 300));
check('P5 once, and ahead of the request', !!restart5 && count(restart5.lastText, "Today's date is") === 1 && restart5.lastText.indexOf("Today's date is") < restart5.lastText.indexOf('run script P5'), restart5?.lastText?.slice(0, 300));
check('P5 with the request and the plan after it', !!restart5 && restart5.lastText.includes('run script P5') && restart5.lastText.includes('P5-PLAN: 1. write p5.txt'));
check('P5 and the run carried on: p5.txt was written', exists('p5.txt') && /MOCK-DONE P5/.test(lastReply.get(j5) ?? ''), lastReply.get(j5));

log(`${LABEL}: ${pass} passed, ${fail} failed`);
fs.writeFileSync(`${S}/result-${LABEL}.json`, JSON.stringify({ label: LABEL, pass, fail, planRequests, modeChanges, replies }, null, 1));
ws.close();
process.exit(fail ? 1 : 0);
