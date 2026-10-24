#!/usr/bin/env node
// End-to-end checks of background tasks against an isolated daemon (see up.sh).
//   node e2e.mjs scenarios   — the agent starts servers four ways; the list, output and Stop are checked
//   node e2e.mjs start-two   — leave two tasks running (for the shutdown and crash checks)
//   node e2e.mjs restored    — after a SIGKILL + restart: the leftovers are listed as `restored` and can be stopped
// Env: BG_RUNS (scratch dir), BG_LABEL, BG_UI / BG_WS (default 38788 / 38789).
import fs from 'node:fs';
import net from 'node:net';
import path from 'node:path';

const RUNS = process.env.BG_RUNS;
const LABEL = process.env.BG_LABEL ?? 'a';
const UI = `http://127.0.0.1:${process.env.BG_UI ?? 38788}`;
const WSURL = `ws://127.0.0.1:${process.env.BG_WS ?? 38789}`;
for (const u of [UI, WSURL]) if (/:(18788|18789)\b/.test(u)) throw new Error('refusing the real daemon ports');
const RUN = path.join(RUNS, `run-${LABEL}`);

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const log = (...a) => console.log(new Date().toISOString().slice(11, 23), ...a);
let failures = 0;
const check = (name, ok, detail = '') => {
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${ok || !detail ? '' : `  — ${detail}`}`);
  if (!ok) failures++;
};

const steps = (tag, list) => `run script ${tag} §§STEPS${JSON.stringify({ tag, steps: list })}§§`;

async function api(p, init) {
  const r = await fetch(`${UI}${p}`, init);
  const t = await r.text();
  let body;
  try {
    body = JSON.parse(t);
  } catch {
    body = t;
  }
  return { status: r.status, body };
}

const listening = (port) =>
  new Promise((resolve) => {
    const s = net.connect({ port, host: '127.0.0.1' });
    s.on('connect', () => (s.destroy(), resolve(true)));
    s.on('error', () => resolve(false));
    setTimeout(() => (s.destroy(), resolve(false)), 800);
  });

async function until(what, fn, ms = 15000) {
  const t0 = Date.now();
  while (Date.now() - t0 < ms) {
    const v = await fn();
    if (v) return v;
    await sleep(150);
  }
  throw new Error(`timed out waiting for ${what}`);
}

async function connect() {
  const ws = new WebSocket(WSURL);
  const state = { idle: new Map(), busy: new Set() };
  await new Promise((resolve, reject) => {
    ws.onopen = () => {
      ws.send(JSON.stringify({ type: 'connect' }));
      setTimeout(resolve, 800);
    };
    ws.onerror = () => reject(new Error(`cannot reach ${WSURL}`));
  });
  ws.onmessage = (e) => {
    const m = JSON.parse(e.data);
    if (m.type === 'permission:request') {
      ws.send(JSON.stringify({ type: 'permission:response', requestId: m.requestId, optionKey: 'allow' }));
    }
    const jid = m.groupJid ?? m.jid ?? m.chatJid;
    if (m.type === 'agent:state' && jid) {
      const s = JSON.stringify(m.state ?? '');
      if (/processing|thinking|running/i.test(s)) state.busy.add(jid);
      if (/idle/i.test(s) && state.busy.has(jid)) {
        state.busy.delete(jid);
        state.idle.get(jid)?.();
      }
    }
  };
  return {
    register: (o) => ws.send(JSON.stringify({ type: 'register:group', channel: '', ...o })),
    async say(jid, text, timeoutMs = 90000) {
      const done = new Promise((resolve, reject) => {
        state.idle.set(jid, resolve);
        setTimeout(() => reject(new Error(`no idle for ${jid} within ${timeoutMs / 1000}s`)), timeoutMs);
      });
      ws.send(JSON.stringify({ type: 'message', groupJid: jid, text }));
      await done;
    },
    unregister: (jid) => ws.send(JSON.stringify({ type: 'unregister:group', jid })),
    close: () => ws.close(),
  };
}

/** Everything the mock saw come back from the agent's tools, in order. */
function toolResults(tag) {
  const f = path.join(RUN, 'mock', 'requests.jsonl');
  if (!fs.existsSync(f)) return [];
  const rows = fs
    .readFileSync(f, 'utf8')
    .split('\n')
    .filter(Boolean)
    .map((l) => JSON.parse(l))
    .filter((r) => r.tag === tag);
  return rows.length ? rows[rows.length - 1].prevResults : [];
}

const tasksOf = async (jid, finished = false) =>
  (await api(`/api/bg-tasks?chatJid=${encodeURIComponent(jid)}${finished ? '&includeFinished=true' : ''}`)).body.tasks ?? [];

const PY_GET = (port) =>
  `python3 -c "import urllib.request as u;print('HTTP',u.urlopen('http://127.0.0.1:${port}/').status)"`;

async function scenarios() {
  const c = await connect();
  const jid = 'web:bg1';
  c.register({ jid, folder: 'main', name: 'bg e2e' });
  await sleep(500);

  // S0 — the polite way: run_in_background, then the agent's own PeekBgJob and StopBgJob.
  log('S0 run_in_background + PeekBgJob + StopBgJob');
  await c.say(
    jid,
    steps('S0', [
      { t: 'Bash', a: { command: 'python3 -m http.server 38950 --bind 127.0.0.1', description: 'serve S0', run_in_background: true } },
      { t: 'Bash', a: { command: PY_GET(38950), description: 'probe S0' } },
      { t: 'PeekBgJob', a: { job_id: 'bash-00001', wait: false } },
      { t: 'StopBgJob', a: { job_id: 'bash-00001' } },
    ]),
  );
  const s0 = toolResults('S0');
  check('S0 Bash returned a job id', /Started in the background: bash-00001/.test(s0[0] ?? ''), s0[0]);
  check('S0 start result names the port', /listening on :38950/.test(s0[0] ?? ''), s0[0]);
  check('S0 server answered while running', /HTTP 200/.test(s0[1] ?? ''), s0[1]);
  check('S0 PeekBgJob shows the request log', /GET \//.test(s0[2] ?? '') && /status=running/.test(s0[2] ?? ''), s0[2]);
  check('S0 StopBgJob stopped it', /stopped=true/.test(s0[3] ?? ''), s0[3]);
  check('S0 port is closed after StopBgJob', !(await listening(38950)));
  const s0t = await tasksOf(jid, true);
  check('S0 task is listed as stopped', s0t.some((t) => t.id === 'bash-00001' && t.status === 'stopped'), JSON.stringify(s0t));

  // S1 — what the user's screenshot did: nohup … > log 2>&1 & in a foreground call.
  log('S1 nohup ... > log 2>&1 &');
  await c.say(
    jid,
    steps('S1', [
      { t: 'Bash', a: { command: 'nohup python3 -m http.server 38951 --bind 127.0.0.1 > srv1.log 2>&1 &\nsleep 1\ncat srv1.log; echo started', description: 'Start http server in background and check log' } },
    ]),
  );
  const s1 = toolResults('S1');
  check('S1 the model is told the job id', /Background process left running by this command: bash-\d+/.test(s1[0] ?? ''), s1[0]);
  check('S1 the model is told the port', /listening on :38951/.test(s1[0] ?? ''), s1[0]);
  let list = await tasksOf(jid);
  const t1 = list.find((t) => t.ports.includes(38951));
  check('S1 /api/bg-tasks lists it running with its port', !!t1 && t1.status === 'running' && t1.origin === 'adopted', JSON.stringify(list));
  check('S1 title is the agent\'s description', t1?.description === 'Start http server in background and check log', t1?.description);
  check('S1 the server is reachable', await listening(38951));
  check('S1 another chat does not see it', (await tasksOf('web:other')).length === 0);
  const stop1 = await api(`/api/bg-tasks/${t1.id}/stop`, { method: 'POST' });
  check('S1 POST stop answers 200', stop1.status === 200, JSON.stringify(stop1.body));
  check('S1 port is closed after Stop', !(await listening(38951)));
  check('S1 second stop is a 409, not a fake success', (await api(`/api/bg-tasks/${t1.id}/stop`, { method: 'POST' })).status === 409);

  // S2 — a trailing & that keeps the pipes: the call must return at once and the server must survive.
  log('S2 server & (pipes held)');
  const t0 = Date.now();
  await c.say(jid, steps('S2', [{ t: 'Bash', a: { command: 'python3 -m http.server 38952 --bind 127.0.0.1 &\nsleep 1\necho started', description: 'Start server with &' } }]));
  const took = Date.now() - t0;
  const s2 = toolResults('S2');
  check('S2 the call did not wait for the pipes (took ' + took + ' ms)', took < 20000, String(took));
  check('S2 the server survived the call', await listening(38952));
  list = await tasksOf(jid);
  const t2 = list.find((t) => t.ports.includes(38952));
  check('S2 listed with its port', !!t2 && t2.status === 'running', JSON.stringify(list));
  // Its request log keeps arriving after the call ended.
  await api('/', { method: 'GET' }).catch(() => {});
  await fetch('http://127.0.0.1:38952/probe-from-e2e').catch(() => {});
  await until('server log in the task output', async () => {
    const d = await api(`/api/bg-tasks/${t2.id}`);
    return /probe-from-e2e/.test(d.body.task?.output ?? '');
  }).then(
    () => check('S2 output keeps streaming into the task after the call returned', true),
    () => check('S2 output keeps streaming into the task after the call returned', false),
  );
  const stop2 = await api(`/api/bg-tasks/${t2.id}/stop`, { method: 'POST' });
  check('S2 stop works', stop2.status === 200 && !(await listening(38952)), JSON.stringify(stop2.body));

  // S3 — fails at once: not a background task.
  log('S3 early exit');
  await c.say(jid, steps('S3', [{ t: 'Bash', a: { command: 'echo boom; exit 3', description: 'fail fast', run_in_background: true } }]));
  const s3 = toolResults('S3');
  check('S3 says it exited, with the code', /exited within 2s \(exit code 3\)/.test(s3[0] ?? '') && /boom/.test(s3[0] ?? ''), s3[0]);
  check('S3 nothing is left running', (await tasksOf(jid)).length === 0);
  const fin = await tasksOf(jid, true);
  check('S3 the failed start is still visible as failed', fin.some((t) => t.status === 'failed' && t.exitCode === 3), JSON.stringify(fin));
  const cleared = await api(`/api/bg-tasks/clear-finished?chatJid=${encodeURIComponent(jid)}`, { method: 'POST' });
  check('S3 clear-finished empties the history', cleared.body.removed >= 1 && (await tasksOf(jid, true)).length === 0, JSON.stringify(cleared.body));

  // S4 — a clean foreground command adds nothing to the list.
  log('S4 clean foreground command');
  await c.say(jid, steps('S4', [{ t: 'Bash', a: { command: 'echo plain', description: 'plain' } }]));
  check('S4 nothing was registered', (await tasksOf(jid, true)).length === 0);
  check('S4 no background note in the result', !/Background process left running/.test(toolResults('S4')[0] ?? ''));

  // S5 — unregistering the chat stops its tasks.
  log('S5 deleting the chat stops its tasks');
  await c.say(jid, steps('S5', [{ t: 'Bash', a: { command: 'python3 -m http.server 38954 --bind 127.0.0.1', description: 'serve S5', run_in_background: true } }]));
  check('S5 server is up', await listening(38954));
  c.unregister(jid);
  await until('port 38954 closed after the chat was deleted', async () => !(await listening(38954)), 12000).then(
    () => check('S5 the chat\'s task stopped with the chat', true),
    () => check('S5 the chat\'s task stopped with the chat', false),
  );
  c.close();
}

async function startTwo() {
  const c = await connect();
  const jid = 'web:bg2';
  c.register({ jid, folder: 'main', name: 'bg e2e two' });
  await sleep(500);
  await c.say(
    jid,
    steps('T1', [
      { t: 'Bash', a: { command: 'nohup python3 -m http.server 38961 --bind 127.0.0.1 > t1.log 2>&1 &\nsleep 1', description: 'adopted server' } },
      { t: 'Bash', a: { command: 'python3 -m http.server 38962 --bind 127.0.0.1', description: 'explicit server', run_in_background: true } },
    ]),
  );
  const list = await tasksOf(jid);
  check('two tasks running', list.length === 2 && (await listening(38961)) && (await listening(38962)), JSON.stringify(list));
  c.close();
}

async function restored() {
  const list = await tasksOf('web:bg2');
  check('both leftovers are listed after the restart', list.length === 2, JSON.stringify(list));
  check('they are marked restored', list.every((t) => t.origin === 'restored'), JSON.stringify(list));
  check('they keep the ids they were recorded with', list.map((t) => t.id).sort().join() === 'bash-00001,bash-00002', list.map((t) => t.id).join());
  // A task started now must not collide with them.
  const c = await connect();
  c.register({ jid: 'web:bg3', folder: 'main', name: 'bg e2e three' });
  await sleep(500);
  await c.say('web:bg3', steps('R1', [{ t: 'Bash', a: { command: 'sleep 30', description: 'new task after restore', run_in_background: true } }]));
  const fresh = await tasksOf('web:bg3');
  check('a new task gets a fresh id (bash-00003)', fresh.length === 1 && fresh[0].id === 'bash-00003', JSON.stringify(fresh.map((t) => t.id)));
  for (const t of fresh) await api(`/api/bg-tasks/${t.id}/stop`, { method: 'POST' });
  c.close();
  for (const t of list) {
    const r = await api(`/api/bg-tasks/${t.id}/stop`, { method: 'POST' });
    check(`stop ${t.id} (restored) works`, r.status === 200, JSON.stringify(r.body));
  }
  check('their ports are free', !(await listening(38961)) && !(await listening(38962)));
}

const phase = process.argv[2];
try {
  if (phase === 'scenarios') await scenarios();
  else if (phase === 'start-two') await startTwo();
  else if (phase === 'restored') await restored();
  else throw new Error('usage: e2e.mjs scenarios|start-two|restored');
} catch (e) {
  console.log('ERROR', e.message);
  failures++;
}
console.log(failures ? `\n${failures} FAILED` : '\nALL PASSED');
process.exit(failures ? 1 : 0);
