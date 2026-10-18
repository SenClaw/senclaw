// Kanban workers read and write the facts of their project, end to end on an isolated daemon + scripted mock LLM.
//   P0_RUNS=<dir> P0_UI=58788 P0_WS=58789 P0_MOCK=58931 node exp-p3b.mjs <runLabel>
// Needs ./env-up.sh <runLabel> <daemon binary> to have been run first (it starts the mock and the daemon).
//
// THE EXTRACTOR HERE IS A SCRIPT, and so is every worker. The mock reads the facts out of ¤¤FACTS{...}¤¤ blocks that this
// driver puts in the summaries its scripted workers hand to kanban_complete; it understands nothing. So this proves the
// plumbing — which bank a worker's prompt is built from, what a finished card sends to the extractor and where the
// result lands — and says NOTHING about how well a real model extracts a fact from a card's summary, nor how noisy
// those facts are.
//
// What it checks, against the real REST routes, the real dispatcher, real worker agents (kanban-server stdio MCP) and
// the real fact store (the request body the worker's model receives is read from the mock's log):
//   read    a card of a project board is started with a <memory_context> block of THAT project's facts ahead of its title;
//           never another project's, never the agent-wide facts; a project that keeps no facts of its workers
//           (facts_write chat/off) and an archived project are still read; a plain board's card and a project with
//           nothing to tell are started exactly as before; the block points at no tool the worker lacks
//   write   a card the worker closes with kanban_complete(summary) lands in bank project:<id> as source 'kanban',
//           no chat; the next card of the project is told it; facts_write chat/off, a plain board, a blocked card,
//           a card left to the dispatcher, a card closed with no summary and a failing extractor all retain nothing
//           (and never fail the card)
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { log, sleep, mockLog, api, wsteps, SP, UI } from '../project-p0-spike/lib.mjs';

const label = process.argv[2];
const S = path.join(SP, `run-${label}`);
const DB = path.join(S, 'home', '.senclaw', 'senclaw.db');
const EVID = path.join(path.dirname(new URL(import.meta.url).pathname), 'evidence');
fs.mkdirSync(EVID, { recursive: true });

const checks = [];
function check(name, ok, detail) {
  checks.push({ name, ok: !!ok, detail: detail ?? null });
  log(ok ? 'PASS' : 'FAIL', name, ok ? '' : JSON.stringify(detail));
}
const sql = (q) => execFileSync('sqlite3', ['-cmd', '.timeout 5000', DB, q], { encoding: 'utf8' }).trim();
const post = (p, body, method = 'POST') =>
  api(p, { method, headers: { 'content-type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) });
const del = (p) => api(p, { method: 'DELETE' });
const textOf = (c) =>
  typeof c === 'string' ? c : Array.isArray(c) ? c.map((p) => (typeof p === 'string' ? p : p.text ?? '')).join('\n') : '';
const facts = (arr) => `¤¤FACTS${JSON.stringify({ facts: arr })}¤¤`;
const fact = (text, extra = {}) => ({ text, kind: 'world', when: null, entities: [], ...extra });
async function until(what, fn, ms = 45000) {
  const t0 = Date.now();
  while (Date.now() - t0 < ms) {
    const v = await fn();
    if (v) return v;
    await sleep(500);
  }
  log('TIMEOUT waiting for', what);
  return null;
}

// ---------------------------------------------------------------- the daemon must be the isolated one
// kanban's AI routes go through app-space-sdk's SpaceClient, whose base URL defaults to http://127.0.0.1:18788 — the
// user's REAL daemon. Refuse to run unless the daemon's own environment points it at itself and token mode is off.
{
  const pid = fs.readFileSync(path.join(S, 'daemon.pid'), 'utf8').trim();
  const env = execFileSync('ps', ['eww', '-p', pid], { encoding: 'utf8' });
  if (!env.includes(`SENCLAW_BASE_URL=${UI}`) || !env.includes('SENCLAW_APP_TOKEN_MODE=off')) {
    console.error(`daemon ${pid} was not started with SENCLAW_BASE_URL=${UI} and SENCLAW_APP_TOKEN_MODE=off. Aborting.`);
    process.exit(2);
  }
}

// ---------------------------------------------------------------- projects
const dir = (k) => fs.realpathSync(path.join(S, 'proj', k));
async function project(name, key, extra = {}) {
  const r = await post('/api/projects', { name, root: dir(key) });
  if (r.status !== 201) throw new Error(`create project ${name}: ${r.status} ${JSON.stringify(r.body)}`);
  if (Object.keys(extra).length) {
    const p = await post(`/api/projects/${r.body.id}`, extra, 'PATCH');
    if (p.status !== 200) throw new Error(`patch project ${name}: ${p.status} ${JSON.stringify(p.body)}`);
  }
  return r.body.id;
}
const pA = await project('Alpha', 'A');
const pB = await project('Beta', 'B');
const pC = await project('Gamma', 'C', { factsWrite: 'off' });
const pD = await project('Delta', 'D', { factsWrite: 'chat' });
const pE = await project('Epsilon', 'E');
const bank = (id) => `project:${id}`;
log('projects', JSON.stringify({ pA, pB, pC, pD, pE }));
const projFacts = async (id) => (await api(`/api/projects/${id}/facts`)).body;

// What a person typed into each project (manual: no model involved), and one fact of the agent as a whole.
async function seed(id, text) {
  const r = await post(`/api/projects/${id}/facts`, { text });
  if (r.status !== 201) throw new Error(`seed ${text}: ${r.status} ${JSON.stringify(r.body)}`);
}
await seed(pA, 'The Alpha payment gateway listens on port 7421.');
await seed(pB, 'The Beta payment gateway listens on port 9090.');
await seed(pC, 'The Gamma archive is kept in vault 12.');
await seed(pD, 'The Delta ledger sits in table 55.');
// An agent-wide fact (bank = a profile folder, no chat), put in the table directly as the P2 driver does.
sql(`insert into mem_facts (id,bank,kind,text,mentioned_at,created_at,content_hash,source,origin)
     values ('common-1','shared','world','The office payment gateway lives in quokkaville.',strftime('%s','now'),strftime('%s','now'),'common-1','upload','')`);
sql(`insert into mem_facts_fts (fact_id,text) values ('common-1','the office payment gateway lives in quokkaville')`);
const MARK = {
  A: ['7421', 'Falcon'],
  B: ['9090', '03:15', '14 days'],
  C: ['vault 12', 'vault 99'],
  D: ['table 55', 'table 77'],
  common: ['quokkaville'],
};
const others = (key) => Object.entries(MARK).filter(([k]) => k !== key).flatMap(([, v]) => v);
const before = Object.fromEntries(await Promise.all([pA, pB, pC, pD, pE].map(async (id) => [id, (await projFacts(id)).total])));
log('facts before', JSON.stringify(before));

// ---------------------------------------------------------------- boards and cards
async function board(title, extra = {}) {
  const r = await post('/api/kanban/boards', { title, template_id: 'standard', ...extra });
  if (r.status !== 200) throw new Error(`board ${title}: ${r.status} ${JSON.stringify(r.body)}`);
  const full = await api(`/api/kanban/board?id=${r.body.id}`);
  const role = (c) => c.role ?? c.column?.role;
  const cid = (c) => c.id ?? c.column?.id;
  const col = (rl) => cid(full.body.columns.find((c) => role(c) === rl));
  return { id: r.body.id, todo: col('todo'), ready: col('ready') };
}
const bA = await board('Alpha board', { project_id: pA });
const bB = await board('Beta board', { project_id: pB });
const bC = await board('Gamma board', { project_id: pC });
const bD = await board('Delta board', { project_id: pD });
const bE = await board('Epsilon board', { project_id: pE });
const bX = await board('Plain board');

const kanbanTools = {
  t: 'ToolSearch',
  a: { query: 'select:mcp__senclaw-kanban__kanban_show,mcp__senclaw-kanban__kanban_complete,mcp__senclaw-kanban__kanban_block', max_results: 5 },
};
/** A card whose worker is scripted: `mode` says how it ends. It stays in Todo until `release`. */
async function addCard(b, tag, title, mode, { summary = '', reason = 'needs the staging key' } = {}) {
  const added = await post('/api/kanban/card/add', { column_id: b.todo, title, description: 'placeholder' });
  const id = added.body.id;
  const list = {
    complete: [kanbanTools, { t: 'mcp__senclaw-kanban__kanban_complete', a: { card_id: id, summary } }],
    block: [kanbanTools, { t: 'mcp__senclaw-kanban__kanban_block', a: { card_id: id, reason } }],
    unresolved: [{ t: 'Bash', a: { command: 'echo working', description: 'work' } }],
  }[mode];
  await post('/api/kanban/card/update', { id, description: wsteps(tag, list) });
  return { id, tag, title, b, mode };
}
const release = (card) => post('/api/kanban/card/move', { id: card.id, column_id: card.b.ready, index: 0 });
const cardRole = async (card) => {
  const full = (await api(`/api/kanban/board?id=${card.b.id}`)).body;
  for (const c of full.columns) if ((c.cards ?? []).some((k) => k.id === card.id)) return c.role ?? c.column?.role;
  return null;
};
const commentsOf = async (card) => (await api(`/api/kanban/card?id=${card.id}`)).body.comments ?? [];

// ---------------------------------------------------------------- reading the mock's log
const reqs = () => mockLog(S).filter((e) => !e.extraction);
const extractions = () => mockLog(S).filter((e) => e.extraction);
const firstRequest = (tag) => reqs().find((e) => e.tag === tag && e.step === 0);
function bodyOf(tag) {
  const e = firstRequest(tag);
  if (!e) return null;
  return JSON.parse(fs.readFileSync(path.join(S, 'mock', 'bodies', `${String(e.seq).padStart(4, '0')}.json`), 'utf8'));
}
/** What the worker's model was handed as its task: the last user message of its first request. */
function promptOf(tag) {
  const body = bodyOf(tag);
  if (!body) return null;
  const text = body.messages.filter((m) => m.role === 'user').map((m) => textOf(m.content)).at(-1) ?? '';
  fs.writeFileSync(path.join(EVID, `prompt-${tag}.txt`), text);
  return text;
}
const toolNames = (tag) => (bodyOf(tag)?.tools ?? []).map((t) => t.function?.name ?? t.name);
/** The engine puts its own <system-reminder> notes (the date, a skill hint) in front of every prompt; the task starts after them. */
const taskOf = (prompt) => (prompt ?? '').replace(/^(?:\s*<system-reminder>[\s\S]*?<\/system-reminder>)+\s*/, '');
const memoryBlock = (prompt) => prompt?.match(/<memory_context>[\s\S]*?<\/memory_context>/)?.[0] ?? null;
const askedExtractor = (title) => extractions().some((e) => (e.extractionText ?? '').includes(`Kanban task "${title}"`));
async function finished(tag, secs = 120) {
  const t0 = Date.now();
  while (Date.now() - t0 < secs * 1000) {
    const rec = reqs().filter((e) => e.tag === tag);
    if (rec.at(-1)?.answered?.text) return true;
    await sleep(1000);
  }
  return false;
}
/** The prompt of the card's worker, once it ran; checks the same things for every card. */
async function ran(card) {
  const ok = await finished(card.tag);
  check(`${card.tag}: the worker ran to its end`, ok, reqs().filter((e) => e.tag === card.tag).length);
  return promptOf(card.tag);
}

// ---------------------------------------------------------------- wave 1: one card per kind of board
const sum = (text, f) => `${text} ${f ? facts([f]) : ''}`.trim();
// A card's fact is about something the project's typed facts are not (not the same sentence with another number: that is
// a near-duplicate, and recall folds near-duplicates into one result).
const kA = await addCard(bA, 'KA1', 'Where does the Alpha payment gateway listen', 'complete', {
  summary: sum('Set up the Falcon blue-green deploy pipeline for Alpha.', fact('Alpha deploys through the Falcon blue-green pipeline.', { entities: ['Alpha', 'Falcon pipeline'] })),
});
const kB = await addCard(bB, 'KB1', 'Where does the Beta payment gateway listen', 'complete', {
  summary: sum('Scheduled the Beta nightly backup.', fact('The Beta nightly backup runs at 03:15.', { entities: ['Beta', 'backup'] })),
});
const kC = await addCard(bC, 'KC1', 'Where is the Gamma archive kept', 'complete', {
  summary: sum('Moved the Gamma archive to vault 99.', fact('The Gamma archive moved to vault 99.', { entities: ['Gamma archive'] })),
});
const kD = await addCard(bD, 'KD1', 'Where does the Delta ledger sit', 'complete', {
  summary: sum('Moved the Delta ledger to table 77.', fact('The Delta ledger moved to table 77.', { entities: ['Delta ledger'] })),
});
const kE = await addCard(bE, 'KE1', 'Which port does the Epsilon cache use', 'complete', {
  summary: sum('Pointed the Epsilon cache at port 6379.', fact('The Epsilon cache runs on port 6379.', { entities: ['Epsilon cache'] })),
});
const kX = await addCard(bX, 'KX1', 'Where does the plain payment gateway listen', 'complete', {
  summary: sum('Moved the plain gateway to port 5150.', fact('The plain gateway listens on port 5150.', { entities: ['plain gateway'] })),
});
const wave1 = [kA, kB, kC, kD, kE, kX];
for (const k of wave1) await release(k);
log('dispatch-config ->', JSON.stringify((await post('/api/dispatch-config', { enabled: true })).body));

const prompts = {};
for (const k of wave1) prompts[k.tag] = await ran(k);

// ---- read
const blockA = memoryBlock(prompts.KA1);
check('A: the worker was handed a <memory_context> block', !!blockA, prompts.KA1?.slice(0, 600));
check('A: …with the Alpha fact a person typed', blockA?.includes('7421'), blockA);
check('A: …labelled a fact', /\(fact\)[^\n]*\n[^\n]*7421/.test(blockA ?? ''), blockA);
check('A: …the first thing of the task, ahead of the card title', taskOf(prompts.KA1).startsWith('<memory_context>') && taskOf(prompts.KA1).indexOf('</memory_context>\n\nWhere does the Alpha payment gateway listen') > 0, taskOf(prompts.KA1).slice(0, 400));
check('A: …and no other project’s fact, no agent-wide fact', others('A').every((m) => !(prompts.KA1 ?? '').includes(m)), others('A').filter((m) => (prompts.KA1 ?? '').includes(m)));
check('A: …and the block points at no memory tool (the worker has none in its roster)', !/memory_recall|memory_get/.test(blockA ?? '') && !toolNames('KA1').some((n) => /memory_recall|memory_get/.test(n)), toolNames('KA1').filter((n) => /memory/.test(n)));
const blockB = memoryBlock(prompts.KB1);
check('B: the Beta worker is told the Beta fact and not Alpha’s', blockB?.includes('9090') && others('B').every((m) => !(prompts.KB1 ?? '').includes(m)), blockB);
check('C (facts_write=off): the worker is still told the project’s facts', memoryBlock(prompts.KC1)?.includes('vault 12') && others('C').every((m) => !(prompts.KC1 ?? '').includes(m)), memoryBlock(prompts.KC1));
check('D (facts_write=chat): the worker is still told the project’s facts', memoryBlock(prompts.KD1)?.includes('table 55') && others('D').every((m) => !(prompts.KD1 ?? '').includes(m)), memoryBlock(prompts.KD1));
check('E (no facts yet): the worker prompt is the card and nothing else', memoryBlock(prompts.KE1) === null && !(prompts.KE1 ?? '').includes('<memory_context'), prompts.KE1?.slice(0, 300));
check('E: its task starts with the card title, as it always did', taskOf(prompts.KE1).startsWith('Which port does the Epsilon cache use\n\nrun script'), taskOf(prompts.KE1).slice(0, 120));
check('X (plain board): no block, no fact of any project or of the agent', memoryBlock(prompts.KX1) === null && Object.values(MARK).flat().every((m) => !(prompts.KX1 ?? '').includes(m)), prompts.KX1?.slice(0, 300));
check('X: its task starts with the card title, as it always did', taskOf(prompts.KX1).startsWith('Where does the plain payment gateway listen\n\nrun script'), taskOf(prompts.KX1).slice(0, 120));

// ---- write
const newIn = async (id) => (await projFacts(id)).facts.filter((f) => f.source === 'kanban');
const nA = await until('Alpha to hold a fact from the card', async () => ((await newIn(pA)).length ? true : null));
const factsA = await projFacts(pA);
const fromCardA = factsA.facts.filter((f) => f.source === 'kanban');
check('A: the closed card left a fact in the project bank', fromCardA.length === 1 && fromCardA[0].text.includes('Falcon'), factsA);
check('A: …tagged with no chat, from the kanban worker, in bank project:<id>', fromCardA[0]?.origin === '' && factsA.bank === bank(pA), fromCardA[0]);
check('A: …next to the fact a person typed, which it did not replace', factsA.facts.some((f) => f.source === 'manual' && f.text.includes('7421')), factsA.facts);
const exA = extractions().find((e) => (e.extractionText ?? '').includes('Kanban task "Where does the Alpha'));
check('A: the extractor was handed the title and the worker’s own summary, not the first line of its last answer',
  exA?.extractionText?.includes('Kanban task "Where does the Alpha payment gateway listen" completed: Set up the Falcon blue-green deploy pipeline for Alpha.') && !exA.extractionText.includes('MOCK-DONE'), exA?.extractionText);
await until('Beta to hold a fact from the card', async () => ((await newIn(pB)).length ? true : null));
const fromCardB = await newIn(pB);
check('B: the Beta card left its fact in Beta’s bank only', fromCardB.length === 1 && fromCardB[0].text.includes('03:15') && !(await newIn(pA)).some((f) => f.text.includes('03:15')), fromCardB);
await until('Epsilon to hold a fact from the card', async () => ((await newIn(pE)).length ? true : null));
check('E: a project that had no facts keeps what its first card finished', (await newIn(pE)).some((f) => f.text.includes('6379')), await projFacts(pE));
await sleep(8000); // a retain that was going to happen has happened by now
const afterC = await projFacts(pC);
const afterD = await projFacts(pD);
check('C (facts_write=off): nothing was retained, nothing was sent to the extractor', afterC.total === before[pC] && !askedExtractor(kC.title), { afterC: afterC.total, asked: askedExtractor(kC.title) });
check('D (facts_write=chat): nothing in the project bank, nothing sent to the extractor', afterD.total === before[pD] && !askedExtractor(kD.title), { afterD: afterD.total, asked: askedExtractor(kD.title) });
const banksNow = (await api('/api/memory/facts/banks')).body.banks.map((b) => b.bank).sort();
check('…and no bank appeared for a worker that has no profile or chat', banksNow.every((b) => b === 'shared' || b.startsWith('project:')) && !banksNow.includes(''), banksNow);
const xFacts = Number(sql(`select count(*) from mem_facts where text like '%5150%'`));
check('X (plain board): nothing was sent to the extractor and no fact of it exists', !askedExtractor(kX.title) && xFacts === 0, { asked: askedExtractor(kX.title), xFacts });
for (const k of [kC, kD, kX]) check(`${k.tag}: the card still reached Done`, (await cardRole(k)) === 'done', await cardRole(k));

// ---------------------------------------------------------------- wave 2: the next card hears of it; the ways a card does NOT retain
const kA2 = await addCard(bA, 'KA2', 'How does Alpha deploy and which port does the payment gateway listen on', 'complete', { summary: 'Checked the deploy and the port, nothing to change.' });
const kA3 = await addCard(bA, 'KA3', 'Alpha card the worker blocks', 'block', { reason: 'needs the staging key' });
const kA4 = await addCard(bA, 'KA4', 'Alpha card left to the dispatcher', 'unresolved');
const kA5 = await addCard(bA, 'KA5', 'Alpha card closed with no summary', 'complete', { summary: '' });
const kA6 = await addCard(bA, 'KA6', 'Alpha card whose extractor is down', 'complete', {
  summary: sum('Rotated the Alpha keys EXTRACTOR-DOWN.', fact('The Alpha keys were rotated.', { entities: ['Alpha'] })),
});
const wave2 = [kA2, kA3, kA4, kA5, kA6];
for (const k of wave2) await release(k);
for (const k of wave2) prompts[k.tag] = await ran(k);

const blockA2 = memoryBlock(prompts.KA2);
check('A2: the next card of Alpha is told what the first one left (kanban fact) and what a person typed', blockA2?.includes('Falcon') && blockA2.includes('7421'), blockA2);
check('A2: …and nothing of any other project or of the agent', others('A').every((m) => !(prompts.KA2 ?? '').includes(m)), others('A').filter((m) => (prompts.KA2 ?? '').includes(m)));
await sleep(10000);
check('A3 (blocked by the worker): nothing sent to the extractor', !askedExtractor(kA3.title), extractions().length);
check('A4 (left to the dispatcher): nothing sent to the extractor', !askedExtractor(kA4.title), null);
const c4 = await commentsOf(kA4);
check('A4: the dispatcher closed it with its own line (so this really is the auto-close path)', c4.some((c) => c.author === 'dispatcher' && c.kind === 'complete'), c4);
check('A5 (closed with no summary): nothing sent to the extractor', !askedExtractor(kA5.title), null);
check('A6 (extractor down): the extractor was asked and refused, and the card is Done all the same', extractions().some((e) => e.refused && (e.extractionText ?? '').includes(kA6.title)) && (await cardRole(kA6)) === 'done', { role: await cardRole(kA6) });
check('A6: …and no fact was kept', !(await projFacts(pA)).facts.some((f) => f.text.includes('keys were rotated')), null);
check('A2/A3/A4/A5/A6: Alpha holds exactly one fact from a card (the first)', (await newIn(pA)).length === 1, await newIn(pA));
log('where the blocked / left cards ended', JSON.stringify({ blocked: await cardRole(kA3), left: await cardRole(kA4), emptySummary: await cardRole(kA5) }));

// ---------------------------------------------------------------- archived project: still read, still retained
const arch = await del(`/api/projects/${pB}`);
const kB2 = await addCard(bB, 'KB2', 'When does the Beta nightly backup run and where does the payment gateway listen', 'complete', {
  summary: sum('Set the Beta backup retention.', fact('The Beta backup retention is 14 days.', { entities: ['Beta', 'backup'] })),
});
await release(kB2);
prompts.KB2 = await ran(kB2);
check('B archived: the worker is still told the project’s facts (typed and from the first card)', arch.status === 200 && memoryBlock(prompts.KB2)?.includes('9090') && memoryBlock(prompts.KB2)?.includes('03:15'), memoryBlock(prompts.KB2));
await until('archived Beta to keep the audit fact', async () => ((await newIn(pB)).length >= 2 ? true : null));
check('B archived: what its card finished is kept all the same', (await newIn(pB)).some((f) => f.text.includes('14 days')), await newIn(pB));

// ---------------------------------------------------------------- tidy summary
const out = {
  projects: { pA, pB, pC, pD, pE },
  boards: { bA, bB, bC, bD, bE, bX },
  before,
  after: Object.fromEntries(await Promise.all([pA, pB, pC, pD, pE].map(async (id) => [id, (await projFacts(id)).total]))),
  extractions: extractions().map((e) => ({ seq: e.seq, refused: e.refused ?? false, text: (e.extractionText ?? '').slice(0, 300) })),
  factsOfAlpha: (await projFacts(pA)).facts.map((f) => ({ text: f.text, source: f.source, origin: f.origin })),
  checks,
};
fs.writeFileSync(path.join(EVID, `result-p3b-${label}.json`), JSON.stringify(out, null, 1));
const failed = checks.filter((k) => !k.ok);
log(`SUMMARY ${checks.length - failed.length}/${checks.length} checks passed`);
for (const f of failed) log('FAILED:', f.name);
process.exit(failed.length ? 1 : 0);
