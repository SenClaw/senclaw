// Kanban boards in projects, end to end on an isolated daemon + scripted mock LLM.
//   P0_RUNS=<dir> P0_UI=58788 P0_WS=58789 P0_MOCK=58931 node exp-p3a.mjs <runLabel>
// Needs ../project-p0-spike/env-up.sh to have been run for <runLabel> first (it starts the mock and the daemon;
// pass SENCLAW_DISPATCH_INTERVAL_SECS=3 to it so a card is picked up within seconds).
//
// What it checks, against the real REST routes, the real dispatcher and real worker agents (kanban-server stdio MCP):
//   REST     board create with project_id (+ refusals), list/get show project_id and effective_workspace_dir,
//            GET /api/projects/:id/boards
//   Worker   a card of a project board runs in the project's root and its SYSTEM PROMPT carries the <project> block
//            right after the card's own instructions; a plain board's worker is what it always was
//   AI       /generate and /breakdown put the project's context in the model prompt (and a plain board's not)
//   Lifecycle archive keeps the board running in the project; purge releases the board (pinned to the old root);
//            a root that vanished blocks the card instead of running it elsewhere
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { log, sleep, mockLog, api, wsteps, SP, UI } from '../project-p0-spike/lib.mjs';

const label = process.argv[2];
const S = path.join(SP, `run-${label}`);
const EVID = path.join(path.dirname(new URL(import.meta.url).pathname), 'evidence');
fs.mkdirSync(EVID, { recursive: true });
const real = (p) => fs.realpathSync(p);

const checks = [];
function check(name, ok, detail) {
  checks.push({ name, ok: !!ok, detail: detail ?? null });
  log(ok ? 'PASS' : 'FAIL', name, ok ? '' : JSON.stringify(detail));
}
const post = (p, body, method = 'POST') =>
  api(p, { method, headers: { 'content-type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) });
const put = (p, body) => post(p, body, 'PUT');
const del = (p) => api(p, { method: 'DELETE' });
const rel = (s) => (typeof s === 'string' ? s.replaceAll(real(S) + '/', '').replaceAll(S + '/', '') : s);

// ---------------------------------------------------------------- the AI endpoints call the daemon's own base URL
// kanban's /generate, /breakdown, /chat go through app-space-sdk's SpaceClient, whose base URL defaults to
// http://127.0.0.1:18788 — the user's REAL daemon. An isolated daemon must be started with SENCLAW_BASE_URL pointing
// at itself, or those calls leave the sandbox. Refuse to run unless the daemon's own environment says so.
{
  const pid = fs.readFileSync(path.join(S, 'daemon.pid'), 'utf8').trim();
  const env = execFileSync('ps', ['eww', '-p', pid], { encoding: 'utf8' });
  if (!env.includes(`SENCLAW_BASE_URL=${UI}`)) {
    console.error(`daemon ${pid} was not started with SENCLAW_BASE_URL=${UI}; its kanban AI calls would reach another daemon. Aborting.`);
    process.exit(2);
  }
}

// ---------------------------------------------------------------- folders
const A = path.join(S, 'proj', 'A');
const B = path.join(S, 'proj', 'B');
const BSUB = path.join(B, 'sub');
const C = path.join(S, 'proj', 'C');
for (const d of [BSUB, C]) fs.mkdirSync(d, { recursive: true });
const RA = real(A), RB = real(B), RBSUB = real(BSUB), RC = real(C);

// ---------------------------------------------------------------- projects
async function project(name, root, marker) {
  const r = await post('/api/projects', { name, root });
  if (r.status !== 201) throw new Error(`create project ${name}: ${r.status} ${JSON.stringify(r.body)}`);
  const w = await put(`/api/projects/${r.body.id}/context`, { content: `${marker}: ${name} is a test project.\n`, updatedAt: 0 });
  if (w.status !== 200) throw new Error(`context ${name}: ${w.status} ${JSON.stringify(w.body)}`);
  return r.body.id;
}
const MARK = { A: 'ALPHA-CONTEXT-MARKER', B: 'BETA-CONTEXT-MARKER', G: 'GAMMA-CONTEXT-MARKER' };
const pA = await project('Alpha', A, MARK.A);
const pB = await project('Beta', B, MARK.B);
const pG = await project('Gamma', C, MARK.G);
log('projects', JSON.stringify({ pA, pB, pG }));

// ---------------------------------------------------------------- boards
async function board(title, extra = {}) {
  const r = await post('/api/kanban/boards', { title, template_id: 'standard', ...extra });
  if (r.status !== 200) return { status: r.status, body: r.body };
  const id = r.body.id;
  const full = await api(`/api/kanban/board?id=${id}`);
  const role = (c) => c.role ?? c.column?.role;
  const cid = (c) => c.id ?? c.column?.id;
  const col = (rl) => cid(full.body.columns.find((c) => role(c) === rl));
  return { id, todo: col('todo'), ready: col('ready'), status: 200 };
}
const bA = await board('Alpha board', { project_id: pA });
const bB = await board('Beta board', { project_id: pB, workspace_dir: BSUB });
const bX = await board('Plain board');
const bG = await board('Gamma board', { project_id: pG });
check('boards are created in projects and plain', [bA, bB, bX, bG].every((b) => b.status === 200), { bA, bB, bX, bG });

// refusals: nothing is created
const before = (await api('/api/kanban/boards')).body.length;
const outside = await post('/api/kanban/boards', { title: 'x', template_id: 'standard', project_id: pA, workspace_dir: B });
const dotdot = await post('/api/kanban/boards', { title: 'x', template_id: 'standard', project_id: pA, workspace_dir: `${A}/../B` });
const unknown = await post('/api/kanban/boards', { title: 'x', template_id: 'standard', project_id: 'prj_ffffffff' });
const afterRefusals = (await api('/api/kanban/boards')).body.length;
check('a folder outside the root is refused (400)', outside.status === 400 && /inside the project root/.test(outside.body.error ?? ''), outside);
check('a folder with .. is refused (400)', dotdot.status === 400, dotdot);
check('an unknown project is refused (404)', unknown.status === 404, unknown);
check('refusals create nothing', afterRefusals === before, { before, afterRefusals });

// ---------------------------------------------------------------- REST views
const list = (await api('/api/kanban/boards')).body;
const byId = (id) => list.find((b) => b.id === id);
check('list: project board shows project_id and the root as effective dir',
  byId(bA.id)?.project_id === pA && byId(bA.id)?.workspace_dir === null && byId(bA.id)?.effective_workspace_dir === RA, byId(bA.id));
check('list: project board with a folder inside the root keeps it',
  byId(bB.id)?.project_id === pB && byId(bB.id)?.effective_workspace_dir === BSUB, byId(bB.id));
check('list: plain board has no project and no effective dir',
  byId(bX.id)?.project_id === null && byId(bX.id)?.effective_workspace_dir === null && byId(bX.id)?.workspace_dir === null, byId(bX.id));
const one = (await api(`/api/kanban/board?id=${bA.id}`)).body;
check('get board: meta carries project_id and effective_workspace_dir', one.meta?.project_id === pA && one.meta?.effective_workspace_dir === RA, one.meta);
const ofA = await api(`/api/projects/${pA}/boards`);
check('GET /api/projects/:id/boards lists only its boards',
  ofA.status === 200 && ofA.body.boards?.length === 1 && ofA.body.boards[0].id === bA.id && ofA.body.boards[0].effective_workspace_dir === RA, ofA);
const nobody = await api('/api/projects/prj_ffffffff/boards');
check('GET /api/projects/<unknown>/boards is 404', nobody.status === 404, nobody);

// ---------------------------------------------------------------- AI endpoints see the project context
const goalA = await post('/api/kanban/generate', { goal: 'Ship invoicing', project_id: pA });
const cardForAi = await post('/api/kanban/card/add', { column_id: bA.todo, title: 'Write the invoice export', description: 'csv' });
const cardForAiX = await post('/api/kanban/card/add', { column_id: bX.todo, title: 'Write the plain export', description: 'csv' });
const brA = await post('/api/kanban/breakdown', { card_id: cardForAi.body.id });
const brX = await post('/api/kanban/breakdown', { card_id: cardForAiX.body.id });
const goalX = await post('/api/kanban/generate', { goal: 'Ship plain things' });
await sleep(1000);
const reqs = mockLog(S).filter((r) => !r.tag && !r.extraction);
function headOf(r) {
  const seq = String(r.seq).padStart(4, '0');
  try {
    return fs.readFileSync(path.join(S, 'mock', 'sys', `${seq}.txt`), 'utf8').slice(0, 30);
  } catch {
    return '';
  }
}
const aiRows = reqs.map((r) => ({ seq: r.seq, head: headOf(r), carries: (r.lastText ?? '').includes('About the project this board belongs to'), alpha: (r.lastText ?? '').includes(MARK.A) }));
log('ai rows', JSON.stringify(aiRows));
check('/generate with project_id puts the project context in the board-planner prompt',
  aiRows.some((r) => r.head.startsWith('You are a Kanban board planner') && r.carries && r.alpha), aiRows);
check('/breakdown of a card on a project board puts the context in the prompt',
  aiRows.some((r) => r.head.startsWith('You break a single task card') && r.carries && r.alpha), aiRows);
check('/generate and /breakdown without a project carry no project section',
  aiRows.filter((r) => /^You (break|are a Kanban board planner)/.test(r.head)).length === 4 &&
    aiRows.filter((r) => /^You (break|are a Kanban board planner)/.test(r.head) && r.carries).length === 2, aiRows);
log('ai statuses (the mock answers [] so the model output is rejected)', goalA.status, brA.status, brX.status, goalX.status);

// ---------------------------------------------------------------- workers
async function card(b, tag, title) {
  const added = await post('/api/kanban/card/add', { column_id: b.todo, title, description: 'placeholder' });
  const id = added.body.id;
  const desc = wsteps(tag, [
    { t: 'ToolSearch', a: { query: 'select:mcp__senclaw-kanban__kanban_show,mcp__senclaw-kanban__kanban_complete', max_results: 5 } },
    { t: 'Bash', a: { command: 'pwd', description: 'print cwd' } },
    { t: 'Write', a: { file_path: `kanban-${tag}.txt`, content: tag } },
    { t: 'mcp__senclaw-kanban__kanban_complete', a: { card_id: id, summary: `done ${tag}` } },
  ]);
  await post('/api/kanban/card/update', { id, description: desc });
  await post('/api/kanban/card/move', { id, column_id: b.ready, index: 0 });
  return id;
}
async function waitWorker(tag, secs = 180) {
  for (let i = 0; i < secs / 2; i++) {
    await sleep(2000);
    const rec = mockLog(S).filter((r) => r.tag === tag && !r.extraction);
    const fin = rec[rec.length - 1];
    if (fin?.answered?.text) return rec;
  }
  return mockLog(S).filter((r) => r.tag === tag && !r.extraction);
}
const sysOf = (rec) => fs.readFileSync(path.join(S, 'mock', 'sys', `${String(rec[0].seq).padStart(4, '0')}.txt`), 'utf8');
const guidanceOf = (id) =>
  `You are running Kanban task #${id}. First call the tool \`mcp__senclaw-kanban__kanban_show\` with card_id=${id} to read its description, comments, and the summaries of the tasks it depends on. Do the work using your tools. You MUST finish with exactly one of: \`mcp__senclaw-kanban__kanban_complete(card_id=${id}, summary=…)\` when the task is done, or \`mcp__senclaw-kanban__kanban_block(card_id=${id}, reason=…)\` when you need human input or an external dependency. Never stop without calling one of them. For code-changing work, block with reason 'review-required: …' and attach details via \`kanban_comment\`.`;
const cardRole = async (b, id) => {
  const full = (await api(`/api/kanban/board?id=${b.id}`)).body;
  for (const c of full.columns) if ((c.cards ?? []).some((k) => k.id === id)) return c.role ?? c.column?.role;
  return null;
};

const wave = {};
async function runCard(name, b, tag, title) {
  const id = await card(b, tag, title);
  wave[name] = { id, tag };
  return id;
}
// Cards go to Ready first; the dispatcher is global and off until now.
const kA = await runCard('A', bA, 'KA', 'Alpha card');
const kB = await runCard('B', bB, 'KB', 'Beta card');
const kX = await runCard('X', bX, 'KX', 'Plain card');
log('cards', JSON.stringify(wave));
log('dispatch-config ->', JSON.stringify((await post('/api/dispatch-config', { enabled: true })).body));

const out = { projects: { pA, pB, pG }, boards: { bA, bB, bX, bG }, cards: wave };
async function judge(name, b, id, tag, expectDir, expectBlock, otherMarkers) {
  const rec = await waitWorker(tag);
  if (!rec.length) {
    check(`${name}: a worker request reached the mock`, false, 'none');
    return null;
  }
  const sys = sysOf(rec);
  fs.writeFileSync(path.join(EVID, `system-prompt-${tag}.txt`), sys);
  const fin = rec[rec.length - 1];
  const pwd = (fin.prevResults[1] ?? '').trim();
  const wrote = (fin.prevResults[2] ?? '').split('\n')[0];
  const filePlace = expectDir && fs.existsSync(path.join(expectDir, `kanban-${tag}.txt`));
  const g = guidanceOf(id);
  const r = {
    systemPromptChars: sys.length,
    startsWithGuidance: sys.startsWith(g),
    hasProjectBlock: sys.includes('<project name='),
    blockRightAfterGuidance: expectBlock ? sys.startsWith(`${g}\n\n<project name="${expectBlock.name}" root="${expectBlock.root}">`) : null,
    markerPresent: expectBlock ? sys.includes(expectBlock.marker) : null,
    leakedMarkers: otherMarkers.filter((m) => sys.includes(m)),
    hasDefaultAssistantPrompt: sys.includes('You are a helpful AI assistant'),
    bashPwd: rel(pwd),
    writeResult: rel(wrote),
    cardRoleAfter: await cardRole(b, id),
  };
  out[tag] = r;
  log(tag, JSON.stringify(r));
  return { sys, pwd, r, filePlace };
}

const jA = await judge('Alpha worker', bA, kA, 'KA', RA, { name: 'Alpha', root: RA, marker: MARK.A }, [MARK.B, MARK.G]);
check('Alpha worker: cwd is the project root and the file landed there', jA && real(jA.pwd) === RA && jA.filePlace, jA?.r);
check('Alpha worker: system prompt = card guidance, then the <project> block (Alpha), nothing else replaced',
  jA?.r.startsWithGuidance && jA.r.blockRightAfterGuidance && jA.r.markerPresent && !jA.r.hasDefaultAssistantPrompt, jA?.r);
check('Alpha worker: no other project’s context leaked in', jA && jA.r.leakedMarkers.length === 0, jA?.r);
check('Alpha worker: card reached Done', jA?.r.cardRoleAfter === 'done', jA?.r);

const jB = await judge('Beta worker', bB, kB, 'KB', RBSUB, { name: 'Beta', root: RB, marker: MARK.B }, [MARK.A, MARK.G]);
check('Beta worker: cwd is the board’s folder inside the root', jB && real(jB.pwd) === RBSUB && jB.filePlace, jB?.r);
check('Beta worker: system prompt carries the Beta block and not Alpha’s',
  jB?.r.startsWithGuidance && jB.r.blockRightAfterGuidance && jB.r.markerPresent && jB.r.leakedMarkers.length === 0, jB?.r);

const jX = await judge('Plain worker', bX, kX, 'KX', null, null, [MARK.A, MARK.B, MARK.G]);
check('Plain worker: system prompt is only the card guidance (+ the usual env), no project block',
  jX?.r.startsWithGuidance && !jX.r.hasProjectBlock && jX.r.leakedMarkers.length === 0 && !jX.r.hasDefaultAssistantPrompt, jX?.r);
check('Plain worker: still runs in the throwaway folder under mcp-dispatch', jX && /\/\.senclaw\/mcp-dispatch\/\d+$/.test(jX.pwd), jX?.r);

// ---------------------------------------------------------------- archive keeps the board running
const arch = await del(`/api/projects/${pB}`);
const stillB = await api(`/api/projects/${pB}/boards`);
check('archiving Beta keeps its board linked', arch.status === 200 && stillB.body.boards?.length === 1 && stillB.body.boards[0].project_id === pB, { arch: arch.body, stillB: stillB.body });
const toArchived = await post('/api/kanban/boards', { title: 'late', template_id: 'standard', project_id: pB });
check('a new board cannot join an archived project (400)', toArchived.status === 400, toArchived);
const kB2 = await runCard('B2', bB, 'KB2', 'Beta card after archive');
const jB2 = await judge('Beta worker after archive', bB, kB2, 'KB2', RBSUB, { name: 'Beta', root: RB, marker: MARK.B }, [MARK.A]);
check('after archiving, the Beta card still runs in the folder with the block', jB2 && real(jB2.pwd) === RBSUB && jB2.r.blockRightAfterGuidance, jB2?.r);

// ---------------------------------------------------------------- purge releases the board
const purge = await del(`/api/projects/${pA}?purge=true`);
const afterA = (await api('/api/kanban/boards')).body.find((b) => b.id === bA.id);
check('purging Alpha: the board leaves the project and is pinned to the old root',
  purge.status === 200 && afterA.project_id === null && afterA.workspace_dir === RA && afterA.effective_workspace_dir === RA, { purge: purge.body, afterA });
check('the project folder is untouched', fs.existsSync(path.join(A, 'marker-A.txt')) && fs.existsSync(path.join(A, 'kanban-KA.txt')), null);
const kA2 = await runCard('A2', bA, 'KA2', 'Alpha card after purge');
const jA2 = await judge('Alpha worker after purge', bA, kA2, 'KA2', RA, null, [MARK.A, MARK.B]);
check('after the purge, the same board still runs its cards in the old root', jA2 && real(jA2.pwd) === RA && jA2.filePlace && jA2.r.cardRoleAfter === 'done', jA2?.r);
check('after the purge, the worker prompt has no project block (the project is gone)', jA2 && !jA2.r.hasProjectBlock && jA2.r.startsWithGuidance, jA2?.r);

// ---------------------------------------------------------------- a root that vanished blocks the card
fs.renameSync(C, `${C}.gone`);
const kG = await runCard('G', bG, 'KG', 'Gamma card with the folder gone');
let blocked = null;
for (let i = 0; i < 20; i++) {
  await sleep(2000);
  blocked = await cardRole(bG, kG);
  if (blocked === 'blocked') break;
}
const gWorker = mockLog(S).filter((r) => r.tag === 'KG');
const gCard = (await api(`/api/kanban/card?id=${kG}`)).body;
const gComments = (gCard.comments ?? []).map((c) => c.body).find((b) => /^gave_up: project/.test(b)) ?? null;
check('a root that vanished: the card is Blocked, no worker was started', blocked === 'blocked' && gWorker.length === 0, { blocked, workers: gWorker.length });
check('a root that vanished: the card says why', gComments && /cannot be used/.test(gComments), gComments);
fs.renameSync(`${C}.gone`, C);
out.gammaComment = gComments;

fs.writeFileSync(path.join(EVID, `result-p3a-${label}.json`), JSON.stringify({ ...out, checks }, null, 1));
const failed = checks.filter((k) => !k.ok);
log(`SUMMARY ${checks.length - failed.length}/${checks.length} checks passed`);
for (const f of failed) log('FAILED:', f.name);
process.exit(failed.length ? 1 : 0);
