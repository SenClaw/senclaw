// Project P1, end to end on an isolated daemon + scripted mock LLM.
//   P0_RUNS=<dir> P0_UI=48788 P0_WS=48789 P0_MOCK=48931 node exp-p1.mjs <runLabel> <daemon binary>
// Needs ../project-p0-spike/env-up.sh to have been run for <runLabel> first (it starts the mock and the daemon).
//
// What it checks, against the real REST routes, the real WS gateway and real `core-server` children:
//   REST  create / validate-root / duplicates / list / context 409
//   WS    register:group with projectId (and the refusals), update:group refusing a change
//   D6    three sessions of ONE profile (two in projects, one outside) keep their own cwd,
//         workspace_info, relative Write, relative datagen file, and workspace-state file
//   DAG   blocking DAGs from each session, native DAG tool, and a DAG that has to QUEUE behind another
//   Prompt  the <project> block lands in the system prompt, edits apply next turn, AGENTS.md stays last
//   Upgrade restart with projectId stripped from config.json (an old config), purge a project
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync, spawn } from 'node:child_process';
import { connect, steps, wsteps, log, sleep, mockLog, api, SP } from '../project-p0-spike/lib.mjs';

const label = process.argv[2];
const BIN = process.argv[3];
const S = path.join(SP, `run-${label}`);
const HOME_SC = path.join(S, 'home', '.senclaw');
const DATA = path.join(S, 'home', 'senclaw');
const DB = path.join(HOME_SC, 'senclaw.db');
const A = path.join(S, 'proj', 'A');
const B = path.join(S, 'proj', 'B');
const C = path.join(S, 'proj', 'C');
fs.mkdirSync(C, { recursive: true });
fs.writeFileSync(path.join(C, 'marker-C.txt'), 'marker C\n');
fs.writeFileSync(path.join(C, 'seeds.jsonl'), '{"text":"seed-from-C one","label":"c"}\n{"text":"seed-from-C two","label":"c"}\n');
const real = (p) => fs.realpathSync(p);
const RA = real(A), RB = real(B), RC = real(C);
const JID = { A: 'web:shared:a', B: 'web:shared:b', C: 'web:shared:c' };

const checks = [];
function check(name, ok, detail) {
  checks.push({ name, ok: !!ok, detail: detail ?? null });
  log(ok ? 'PASS' : 'FAIL', name, ok ? '' : JSON.stringify(detail));
}
const rel = (s) => (typeof s === 'string' ? s.replaceAll(S + '/', '').replaceAll(real(S) + '/', '') : s);
const sql = (q) => execFileSync('sqlite3', [DB, q], { encoding: 'utf8' }).trim();
const ls = (d) => (fs.existsSync(d) ? fs.readdirSync(d).sort() : []);
const stateFiles = () => ls(HOME_SC).filter((f) => f.startsWith('workspace-state-shared'));
const readCur = (f) => {
  try {
    return JSON.parse(fs.readFileSync(path.join(HOME_SC, f), 'utf8')).currentDir;
  } catch {
    return null;
  }
};
const unwrap = (r) => {
  try {
    return JSON.parse(r).content?.[0]?.text ?? r;
  } catch {
    return r;
  }
};
const tryJson = (t) => {
  try {
    return JSON.parse(t);
  } catch {
    return null;
  }
};
const post = (p, body, method = 'POST') =>
  api(p, { method, headers: { 'content-type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) });

const N = {
  ws: 'mcp__core__workspace_info',
  ds: 'mcp__core__datagen_start',
  dag: 'mcp__core__create_parent_and_run',
  mk: 'mcp__core__create_parent',
};
const LOAD = { t: 'ToolSearch', a: { query: `select:${Object.values(N).join(',')}`, max_results: 20 } };

let c = await connect(S);
const wsErrorsSince = (n) => c.state.errors.slice(n);

// ---------------------------------------------------------------- AGENTS.md (read every turn)
fs.writeFileSync(path.join(HOME_SC, 'AGENTS.md'), 'AGENTS-RULE-MARKER: answer briefly.\n');

// ---------------------------------------------------------------- REST
const bad = async (p) => (await post('/api/projects/validate-root', { path: p })).body;
let v = await bad('/');
check('validate-root refuses /', v.ok === false && v.reasons.some((r) => r.code === 'filesystem_root'), v);
v = await bad(path.join(S, 'home'));
check('validate-root refuses $HOME (and says it holds the state)', v.ok === false && v.reasons.some((r) => r.code === 'home_dir'), v);
v = await bad(HOME_SC);
check('validate-root refuses ~/.senclaw', v.reasons?.some((r) => r.code === 'senclaw_state'), v);
const profiles = path.join(DATA, 'profiles');
v = await bad(profiles);
check('validate-root refuses the profiles folder', ls(profiles).length >= 0 && v.reasons?.some((r) => r.code === 'senclaw_profiles'), v);
v = await bad(path.join(S, 'proj', 'nope'));
check('validate-root says not_found', v.reasons?.[0]?.code === 'not_found', v);
v = await bad(A);
check('validate-root accepts a real folder and canonicalises it', v.ok === true && v.canonical === RA, v);

let r = await post('/api/projects', { name: 'Alpha', root: A, description: 'first' });
check('create project A → 201', r.status === 201 && r.body.id?.startsWith('prj_') && r.body.root === RA, r);
const PA = r.body;
r = await post('/api/projects', { name: 'Beta', root: B });
check('create project B → 201', r.status === 201, r);
const PB = r.body;
for (const spelling of [A + '/', path.join(A, '..', 'A')]) {
  r = await post('/api/projects', { name: 'Dup', root: spelling });
  check(`same root as ${rel(spelling)} → 409 root_in_use`, r.status === 409 && r.body.error === 'root_in_use' && r.body.projectId === PA.id, r);
}
r = await post('/api/projects', { name: 'Root', root: '/' });
check('create with / → 400 invalid_root', r.status === 400 && r.body.error === 'invalid_root', r);
r = await api('/api/projects');
check('list: A and B, nothing else', r.body.projects?.map((p) => p.name).join() === 'Alpha,Beta', r.body);
r = await post(`/api/projects/${PA.id}/context`, undefined, 'GET');
check('context of a new project is empty, updatedAt 0', r.body.content === '' && r.body.updatedAt === 0, r.body);
r = await post(`/api/projects/${PA.id}/context`, { content: 'ALPHA-CTX-MARKER v1: Alpha is the daemon.', updatedAt: 0 }, 'PUT');
check('PUT context → 200 with updatedAt', r.status === 200 && r.body.updatedAt > 0, r);
let ctxT = r.body.updatedAt;
r = await post(`/api/projects/${PA.id}/context`, { content: 'stale', updatedAt: 0 }, 'PUT');
check('PUT context with a stale updatedAt → 409 + current text', r.status === 409 && r.body.content?.includes('v1'), r);
check('context file is on disk under the data home', fs.existsSync(path.join(DATA, 'projects', PA.id, 'CONTEXT.md')));
// B stays without a context on purpose.

// ---------------------------------------------------------------- WS registration
let e0 = c.state.errors.length;
c.register({ folder: 'shared', name: 'A chat', jid: JID.A, groupType: 'chat', projectId: PA.id });
c.register({ folder: 'shared', name: 'B code', jid: JID.B, groupType: 'code', projectId: PB.id, allowedWorkDirs: ['/tmp/ignored-by-the-server'] });
c.register({ folder: 'shared', name: 'C code', jid: JID.C, groupType: 'code', allowedWorkDirs: [C] });
await sleep(1500);
check('registering the three sessions raised no gateway error', wsErrorsSince(e0).length === 0, wsErrorsSince(e0));
const row = (jid) => sql(`select project_id||'|'||allowed_work_dirs from groups where jid='${jid}'`);
check('A is stored in project A with allowed_work_dirs=[root]', row(JID.A) === `${PA.id}|["${RA}"]`, row(JID.A));
check('B is stored in project B; the client-sent folder was replaced by the root', row(JID.B) === `${PB.id}|["${RB}"]`, row(JID.B));
check('C is stored with no project', sql(`select ifnull(project_id,'-') from groups where jid='${JID.C}'`) === '-');

e0 = c.state.errors.length;
c.register({ folder: 'shared', name: 'tg', jid: 'tg:project:1', channel: 'telegram', projectId: PA.id });
c.register({ folder: 'shared', name: 'ghost', jid: 'web:shared:ghost', projectId: 'prj_00000000' });
c.register({ folder: 'shared', name: 'cw', jid: 'web:shared:cw', groupType: 'cowork', projectId: PA.id });
c.send({ type: 'update:group', jid: JID.A, projectId: PB.id });
c.send({ type: 'update:group', jid: JID.A, projectId: null });
c.send({ type: 'update:group', jid: JID.C, projectId: PA.id });
await sleep(1500);
const errs = wsErrorsSince(e0);
check('six refusals reached the client', errs.length === 6, errs);
const saidSoon = (needle, n = 1) => errs.filter((m) => m.includes(needle)).length === n;
check('channel, unknown project, group type, change, drop, late join: each said why',
  saidSoon('web-only') && saidSoon('no such project') && saidSoon('chat and code') && saidSoon('cannot be changed or removed', 3), errs);
check('refused requests created nothing and changed nothing',
  sql(`select count(*) from groups where jid in ('tg:project:1','web:shared:ghost','web:shared:cw')`) === '0' &&
  row(JID.A).startsWith(PA.id) && sql(`select ifnull(project_id,'-') from groups where jid='${JID.C}'`) === '-');
// An ordinary rename (and a client echoing the whole group) is still fine.
e0 = c.state.errors.length;
c.send({ type: 'update:group', jid: JID.A, name: 'A chat (renamed)', projectId: PA.id });
await sleep(700);
check('update:group echoing the same projectId is accepted', wsErrorsSince(e0).length === 0 && sql(`select name from groups where jid='${JID.A}'`).includes('renamed'));

// ---------------------------------------------------------------- turns: the D6 witnesses
const turns = [];
async function turn(who, tag, extra = []) {
  const list = [
    LOAD,
    { t: 'Bash', a: { command: 'pwd', description: 'print cwd' } },
    { t: N.ws, a: {} },
    { t: 'Write', a: { file_path: `rel-${tag}.txt`, content: tag } },
    { t: N.ds, a: { pipeline: 'import', name: `imp-${tag}`, kind: 'labeled_text', license: 'private', source: { file: 'seeds.jsonl' } } },
    ...extra,
  ];
  await c.say(JID[who], steps(tag, list), 120000);
  await sleep(2000);
  const recs = mockLog(S).filter((x) => x.tag === tag && !x.extraction);
  const fin = recs[recs.length - 1];
  const res = fin?.prevResults ?? [];
  const ds = path.join(DATA, 'datasets', `imp-${tag}`, 'data.jsonl');
  const t = {
    who,
    tag,
    pwd: (res[1] ?? '').trim(),
    workspaceInfo: res[2] ? unwrap(res[2]).replace(/\s+/g, ' ') : '',
    wi: tryJson(res[2] ? unwrap(res[2]) : ''),
    wrote: res[3] ?? '',
    dataset: fs.existsSync(ds) ? fs.readFileSync(ds, 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l).text ?? l).join(' | ') : '(no dataset)',
    seq: recs[0]?.seq,
  };
  turns.push(t);
  log(JSON.stringify({ who, tag, pwd: rel(t.pwd), wi: rel(t.workspaceInfo).slice(0, 160), dataset: t.dataset }));
  return t;
}
const where = { A: RA, B: RB, C: RC };
const seed = { A: 'seed-from-A', B: 'seed-from-B', C: 'seed-from-C' };
// C goes between A and B and again at the end: every time a session of this profile built (or would have
// rebuilt) the shared file, the others must not notice.
for (const [who, tag] of [['A', 'A1'], ['B', 'B1'], ['C', 'C1'], ['A', 'A2'], ['B', 'B2'], ['C', 'C2']]) {
  const t = await turn(who, tag);
  const mine = where[who];
  check(`${tag}: Bash pwd is the session's folder`, t.pwd === mine, t.pwd);
  check(`${tag}: workspace_info says ${rel(mine)}`, t.wi?.currentDir === mine, rel(t.workspaceInfo).slice(0, 300));
  if (who !== 'C') check(`${tag}: workspace_info also reports the root as default and only allowed folder`, t.wi?.defaultWorkspace === mine && [].concat(t.wi?.allowedWorkDirs ?? []).join('|') === mine, rel(t.workspaceInfo).slice(0, 300));
  check(`${tag}: a relative Write landed in that folder only`, fs.existsSync(path.join(mine, `rel-${tag}.txt`)) && Object.values(where).filter((x) => x !== mine).every((x) => !fs.existsSync(path.join(x, `rel-${tag}.txt`))));
  check(`${tag}: a relative datagen file was that folder's`, t.dataset.includes(seed[who]) && !Object.entries(seed).some(([k, s]) => k !== who && t.dataset.includes(s)), t.dataset);
}
const files = stateFiles();
const own = (jid) => files.find((f) => f.startsWith('workspace-state-shared@') && readCur(f) === (jid === JID.A ? RA : RB));
check('there are three state files: the profile\'s and one per project session', files.length === 3 && files.includes('workspace-state-shared.json'), files);
check('the profile\'s file is the outside session\'s (C), untouched by A and B', readCur('workspace-state-shared.json') === RC, readCur('workspace-state-shared.json'));
check('A and B each have a file of their own, holding their root', !!own(JID.A) && !!own(JID.B) && own(JID.A) !== own(JID.B), files.map((f) => [f, rel(readCur(f) ?? '')]));

// ---------------------------------------------------------------- the prompt
const sysOf = (tag) => {
  const rec = mockLog(S).find((x) => x.tag === tag && !x.extraction);
  return rec ? fs.readFileSync(path.join(S, 'mock', 'sys', `${String(rec.seq).padStart(4, '0')}.txt`), 'utf8') : '';
};
const sA = sysOf('A1'), sB = sysOf('B1'), sC = sysOf('C1');
const iProj = sA.indexOf('<project name="Alpha"');
const iRules = sA.indexOf('<user_operating_rules>');
check('A: the system prompt carries the <project> block with the context', iProj > 0 && sA.includes('ALPHA-CTX-MARKER v1') && sA.includes(`root="${RA}"`), sA.slice(-400));
check('A: the project block precedes AGENTS.md, which is still last', iRules > iProj && sA.trimEnd().endsWith('</user_operating_rules>'), { iProj, iRules, tail: sA.slice(-200) });
check('B (project without a context) and C (no project): no <project> block at all', !sB.includes('<project') && !sC.includes('<project'));
check('A\'s context is not in B\'s or C\'s prompt', !sB.includes('ALPHA-CTX') && !sC.includes('ALPHA-CTX'));
// An edit applies on the very next turn, with nothing restarted.
r = await post(`/api/projects/${PA.id}/context`, { content: 'ALPHA-CTX-MARKER v2: moved to Go.', updatedAt: ctxT }, 'PUT');
ctxT = r.body.updatedAt;
await turn('A', 'A3');
const sA3 = sysOf('A3');
check('A: a context edit shows on the next turn (v2, not v1)', sA3.includes('ALPHA-CTX-MARKER v2') && !sA3.includes('ALPHA-CTX-MARKER v1'));
fs.writeFileSync(path.join(DATA, 'projects', PA.id, 'CONTEXT.md'), 'ALPHA-CTX-MARKER v3: edited by hand.\n');
await turn('A', 'A4');
check('A: a hand edit of CONTEXT.md shows on the next turn too', sysOf('A4').includes('ALPHA-CTX-MARKER v3'));
r = await post(`/api/projects/${PA.id}/context`, { content: 'stale', updatedAt: ctxT }, 'PUT');
check('and a client holding the old copy is refused after a hand edit', r.status === 409, r);

// ---------------------------------------------------------------- DAGs
const workerSteps = (tag, sleepFor = 0) => [
  ...(sleepFor ? [{ t: 'Bash', a: { command: `sleep ${sleepFor} && pwd`, description: 'wait then print cwd' } }] : [{ t: 'Bash', a: { command: 'pwd', description: 'print cwd' } }]),
  { t: 'Write', a: { file_path: `dag-${tag}.txt`, content: tag } },
];
const dagTasks = (tag, sleepFor) =>
  [1, 2].map((i) => ({ label: `w${i}`, agentName: 'persona:p0-worker', io: 'readOnly', prompt: wsteps(`W-${tag}-${i}`, workerSteps(`${tag}-${i}`, sleepFor)) }));
const workerPwd = (tag) => mockLog(S).filter((x) => x.tag?.startsWith(`W-${tag}-`) && x.step === 2).map((x) => (x.prevResults[0] ?? '').trim());
for (const [who, tag] of [['A', 'DA'], ['B', 'DB'], ['C', 'DC']]) {
  await c.say(JID[who], steps(tag, [LOAD, { t: N.dag, a: { goal: `p1 ${tag}`, timeoutSeconds: 60, tasks: dagTasks(tag, 0) } }]), 150000);
  await sleep(2000);
  const pwds = workerPwd(tag);
  check(`${tag}: both workers ran in ${rel(where[who])}`, pwds.length === 2 && pwds.every((p) => p === where[who]), pwds);
  check(`${tag}: and wrote there only`, [1, 2].every((i) => fs.existsSync(path.join(where[who], `dag-${tag}-${i}.txt`))) && Object.entries(where).filter(([k]) => k !== who).every(([, d]) => ls(d).every((f) => !f.startsWith(`dag-${tag}-`))));
}

// A DAG that has to wait behind another of the same profile (project B's queues behind project A's).
const state = () => JSON.parse(fs.readFileSync(path.join(HOME_SC, 'dispatch-state.json'), 'utf8'));
await c.say(JID.A, steps('QA', [LOAD, { t: N.mk, a: { goal: 'p1 queue A', tasks: dagTasks('QA', 10) } }]), 90000);
await c.say(JID.B, steps('QB', [LOAD, { t: N.mk, a: { goal: 'p1 queue B', tasks: dagTasks('QB', 10) } }]), 90000);
await sleep(500);
const mine = (g) => state().parents.find((p) => p.goal === g);
const pA = mine('p1 queue A'), pB = mine('p1 queue B');
check('QA: A\'s parent is active in A\'s root', pA?.status === 'active' && pA.sharedWorkspace === RA, pA && { s: pA.status, w: pA.sharedWorkspace });
check('QB: B\'s parent waits (queued) and ALREADY knows B\'s root', pB?.status === 'queued' && pB.sharedWorkspace === RB, pB && { s: pB.status, w: pB.sharedWorkspace });
const deadline = Date.now() + 90000;
while (Date.now() < deadline && !(mine('p1 queue B')?.status === 'done')) await sleep(1000);
await sleep(1500);
const pB2 = mine('p1 queue B');
check('QB: promoted and finished', pB2?.status === 'done', pB2 && { s: pB2.status, tasks: pB2.tasks.map((t) => t.status) });
check('QB: the promoted parent still points at B\'s root', pB2?.sharedWorkspace === RB, pB2?.sharedWorkspace);
const qa = workerPwd('QA'), qb = workerPwd('QB');
check('QA workers ran in A\'s root', qa.length === 2 && qa.every((p) => p === RA), qa);
check('QB workers ran in B\'s root (after being promoted inside the daemon)', qb.length === 2 && qb.every((p) => p === RB), qb);
check('QB wrote into B only', [1, 2].every((i) => fs.existsSync(path.join(B, `dag-QB-${i}.txt`)) && !fs.existsSync(path.join(A, `dag-QB-${i}.txt`)) && !fs.existsSync(path.join(C, `dag-QB-${i}.txt`))));

// The built-in (native) DAG tool is a different code path from the MCP one.
await c.say(JID.A, steps('NA', [{ t: 'ToolSearch', a: { query: 'select:DispatchCreateParentAndRun', max_results: 5 } }, { t: 'DispatchCreateParentAndRun', a: { goal: 'p1 native A', timeoutSeconds: 60, tasks: dagTasks('NA', 0) } }]), 150000);
await sleep(2000);
const na = workerPwd('NA');
check('NA: the native DAG tool also ran in A\'s root', na.length === 2 && na.every((p) => p === RA), na);

// ---------------------------------------------------------------- upgrade: an old config.json, a restart
const cfgPath = path.join(HOME_SC, 'config.json');
const cfg = JSON.parse(fs.readFileSync(cfgPath, 'utf8'));
check('config.json mirrors the session with projectId', cfg.groups?.find((g) => g.jid === JID.A)?.projectId === PA.id && cfg.groups?.find((g) => g.jid === JID.C)?.projectId === undefined, cfg.groups);
c.close();
for (const g of cfg.groups ?? []) delete g.projectId; // what an older build's file looks like
fs.writeFileSync(cfgPath, JSON.stringify(cfg, null, 2));
const pid = fs.readFileSync(path.join(S, 'daemon.pid'), 'utf8').trim();
process.kill(Number(pid), 'SIGTERM');
await sleep(5000);
const child = spawn(path.join(S, 'senclaw-bin'), ['start'], {
  cwd: path.join(S, 'root'),
  env: { ...process.env, HOME: path.join(S, 'home'), SENCLAW_HOME: HOME_SC, SENCLAW_UI_PORT: process.env.P0_UI ?? '38788', SENCLAW_WS_PORT: process.env.P0_WS ?? '38789', SENCLAW_COGNITIVE_REFLECT_WINDOW_IDLE_MS: '6000', RUST_LOG: 'info' },
  stdio: ['ignore', fs.openSync(path.join(S, 'log', 'daemon2.log'), 'a'), fs.openSync(path.join(S, 'log', 'daemon2.log'), 'a')],
  detached: true,
});
child.unref();
fs.writeFileSync(path.join(S, 'daemon.pid'), String(child.pid));
for (let i = 0; i < 60; i++) {
  try {
    if ((await fetch(`http://127.0.0.1:${process.env.P0_UI ?? 38788}/api/auth/status`)).ok) break;
  } catch {}
  await sleep(1000);
}
await sleep(3000);
check('after a restart on a config.json with no projectId, the sessions are still in their projects',
  row(JID.A).startsWith(PA.id) && row(JID.B).startsWith(PB.id) && sql(`select ifnull(project_id,'-') from groups where jid='${JID.C}'`) === '-', [row(JID.A), row(JID.B)]);
r = await api('/api/projects');
check('the list survives the restart with chat counts', r.body.projects?.find((p) => p.id === PA.id)?.chatCount === 1 && r.body.projects?.find((p) => p.id === PB.id)?.chatCount === 1, r.body.projects);
c = await connect(S);
const t5 = await turn('A', 'A5');
check('A5 (a fresh core after the restart): still in A\'s root', t5.pwd === RA && t5.wi?.currentDir === RA, t5.pwd);
check('A5: the context is still in the prompt', sysOf('A5').includes('ALPHA-CTX-MARKER v3'));
const t6 = await turn('B', 'B5');
check('B5 (fresh core): in B\'s root', t6.pwd === RB, t6.pwd);
const files2 = stateFiles();
check('the restart made no new state files', files2.length === 3, files2);

// ---------------------------------------------------------------- delete
r = await post(`/api/projects/${PA.id}`, undefined, 'DELETE');
check('DELETE without purge archives', r.status === 200 && r.body.archivedAt > 0, r);
r = await api('/api/projects');
check('archived: gone from the default list', r.body.projects?.map((p) => p.name).join() === 'Beta', r.body);
e0 = c.state.errors.length;
c.register({ folder: 'shared', name: 'late', jid: 'web:shared:late', projectId: PA.id });
await sleep(900);
check('an archived project takes no new session', wsErrorsSince(e0).some((m) => m.includes('archived')), wsErrorsSince(e0));
const ownA = own(JID.A);
fs.writeFileSync(path.join(A, 'keep-me.txt'), 'mine');
r = await post(`/api/projects/${PA.id}?purge=true`, undefined, 'DELETE');
check('purge removes the project and detaches its session', r.status === 200 && r.body.detachedChats?.join() === JID.A, r);
check('the session itself is still there, in no project', sql(`select ifnull(project_id,'-') from groups where jid='${JID.A}'`) === '-');
check('the context folder is gone, the root is untouched', !fs.existsSync(path.join(DATA, 'projects', PA.id)) && fs.existsSync(path.join(A, 'keep-me.txt')) && fs.existsSync(path.join(A, 'marker-A.txt')));
check('the session\'s own state file was removed', ownA && !fs.existsSync(path.join(HOME_SC, ownA)), ownA);
await turn('A', 'A6');
check('after the purge the next turn has no <project> block', !sysOf('A6').includes('<project'));
await turn('B', 'B6');
check('B is untouched by A\'s deletion', sysOf('B6') && row(JID.B).startsWith(PB.id));

// ---------------------------------------------------------------- deleting a session
const ownB = own(JID.B);
check('B still has its own state file before it is deleted', !!ownB);
c.send({ type: 'unregister:group', jid: JID.B });
await sleep(1500);
check('deleting a session in a project removes its own state file', ownB && !fs.existsSync(path.join(HOME_SC, ownB)) && sql(`select count(*) from groups where jid='${JID.B}'`) === '0', ownB);
check('and leaves the profile\'s shared file (C\'s) alone', readCur('workspace-state-shared.json') === RC, readCur('workspace-state-shared.json'));

// ---------------------------------------------------------------- housekeeping
const logText = fs.readFileSync(path.join(S, 'log', 'daemon.log'), 'utf8') + fs.readFileSync(path.join(S, 'log', 'daemon2.log'), 'utf8');
check('no external MCP server was contacted (isolated cwd worked)', (logText.match(/MCP-external HTTP connect/g) ?? []).length === 0);
const failed = checks.filter((x) => !x.ok);
fs.writeFileSync(path.join(S, 'result-p1.json'), JSON.stringify({ label, bin: BIN, passed: checks.length - failed.length, failed: failed.length, checks, turns }, null, 1));
log(`${checks.length - failed.length}/${checks.length} checks passed`);
c.close();
process.exit(failed.length ? 1 : 0);
