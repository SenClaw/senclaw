// Real kanban workers on a board of a project, with a real model, end to end on an isolated daemon.
//   P0_RUNS=<dir> P0_UI=53788 P0_WS=53789 P0_MOCK=53931 node exp-p5-worker.mjs <runLabel>
// Needs ./env-up.sh <runLabel> <daemon binary> first (it starts the logging proxy in front of the model and the daemon).
//
// What it measures, per card and in total: does the worker finish with kanban_complete and a summary that says something,
// does it block, or does it end without calling either (the dispatcher then closes the card itself and nothing is
// retained); what facts the finished cards left in the project's bank; whether the next card was told them. It gates
// nothing: the numbers are the product, and they are the numbers of THE MODEL NAMED IN THE RUN, not of any other.
// Cards run one at a time, so a card can only be told what the cards before it left.
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { log, sleep, api, SP, UI } from '../project-p0-spike/lib.mjs';

const label = process.argv[2];
const S = path.join(SP, `run-${label}`);
const EVID = path.join(path.dirname(new URL(import.meta.url).pathname), 'evidence');
fs.mkdirSync(EVID, { recursive: true });
const CARD_TIMEOUT_S = Number(process.env.P5_CARD_TIMEOUT ?? 600);

// ---------------------------------------------------------------- the daemon must be the isolated one
{
  const pid = fs.readFileSync(path.join(S, 'daemon.pid'), 'utf8').trim();
  const env = execFileSync('ps', ['eww', '-p', pid], { encoding: 'utf8' });
  if (!env.includes(`SENCLAW_BASE_URL=${UI}`) || !env.includes('SENCLAW_APP_TOKEN_MODE=off')) {
    console.error(`daemon ${pid} was not started with SENCLAW_BASE_URL=${UI} and SENCLAW_APP_TOKEN_MODE=off. Aborting.`);
    process.exit(2);
  }
}
const post = (p, body, method = 'POST') =>
  api(p, { method, headers: { 'content-type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) });

// ---------------------------------------------------------------- the project: a folder with a few things to find
const root = fs.realpathSync(path.join(S, 'proj', 'A'));
const orders = [120, 75, 310, 45, 89, 230, 15, 400, 60, 95, 180, 25];
fs.writeFileSync(path.join(root, 'VERSION'), '3.7.2\n');
fs.mkdirSync(path.join(root, 'data'), { recursive: true });
fs.writeFileSync(path.join(root, 'data', 'orders.csv'), 'id,customer,amount\n' + orders.map((a, i) => `${i + 1},customer-${i + 1},${a}`).join('\n') + '\n');
fs.mkdirSync(path.join(root, 'src'), { recursive: true });
const loader = ['"""Configuration loading for Sao Kim."""', 'import json', '', 'DEFAULTS = {"port": 8420, "debug": False}', '', '', 'def parse_config(path):', '    with open(path) as f:', '        return {**DEFAULTS, **json.load(f)}', ''];
fs.writeFileSync(path.join(root, 'src', 'config_loader.py'), loader.join('\n'));
const parseLine = loader.findIndex((l) => l.startsWith('def parse_config')) + 1;
fs.writeFileSync(path.join(root, 'src', 'util.py'), 'def clamp(x, lo, hi):\n    return max(lo, min(hi, x))\n');
fs.writeFileSync(path.join(root, 'README.md'), '# Sao Kim\n\nBackend service of the Sao Kim shop.\n');
const orderTotal = orders.reduce((a, b) => a + b, 0);

const created = await post('/api/projects', { name: 'Sao Kim', root });
if (created.status !== 201) throw new Error(`create project: ${created.status} ${JSON.stringify(created.body)}`);
const pid = created.body.id;
const ctx = await api(`/api/projects/${pid}/context`);
await api(`/api/projects/${pid}/context`, {
  method: 'PUT',
  headers: { 'content-type': 'application/json' },
  body: JSON.stringify({ content: 'Sao Kim is the backend of a small online shop. Source lives in src/, data in data/.', updatedAt: ctx.body?.updatedAt }),
});
const projFacts = async () => (await api(`/api/projects/${pid}/facts`)).body;

const b = await post('/api/kanban/boards', { title: 'Sao Kim board', template_id: 'standard', project_id: pid });
if (b.status !== 200) throw new Error(`board: ${b.status} ${JSON.stringify(b.body)}`);
const full0 = (await api(`/api/kanban/board?id=${b.body.id}`)).body;
const role = (c) => c.role ?? c.column?.role;
const cid = (c) => c.id ?? c.column?.id;
const col = (rl) => cid(full0.columns.find((c) => role(c) === rl));
const board = { id: b.body.id, todo: col('todo'), ready: col('ready') };
log('project', pid, 'board', board.id, 'root', root);

// ---------------------------------------------------------------- the cards
const has = (text, ...needles) => needles.every((n) => new RegExp(n, 'i').test(text ?? ''));
const fileHas = (rel, re) => {
  try {
    return new RegExp(re, 'i').test(fs.readFileSync(path.join(root, rel), 'utf8'));
  } catch {
    return false;
  }
};
const CARDS = [
  { tag: 'W1', title: 'Create the notes file', description: 'Create the file notes/hello.txt inside the project with exactly this one line: Sao Kim backend port is 8431. Then finish the card and say which file you created.', ok: () => fileHas('notes/hello.txt', '8431') },
  { tag: 'W2', title: 'Read the project version', description: 'Read the file VERSION in the project root and report the version number it contains.', ok: (s) => has(s, '3\\.7\\.2') },
  { tag: 'W3', title: 'Count the orders', description: 'data/orders.csv lists orders under a header row. Report how many orders it lists (not counting the header) and the sum of its amount column.', ok: (s) => has(s, `\\b${orders.length}\\b`, `\\b${orderTotal}\\b`) },
  { tag: 'W4', title: 'Write the greet function', description: 'Create src/greet.py with a function greet(name) that returns the text "Xin chào, <name>!" with the name filled in. Then report the file you created.', ok: () => fileHas('src/greet.py', 'def greet') },
  { tag: 'W5', title: 'Find parse_config', description: 'Find which file under src/ defines the function parse_config and report its path and the line number of the definition.', ok: (s) => has(s, 'config_loader\\.py', `\\b${parseLine}\\b`) },
  { tag: 'W6', title: 'What is the project version', description: 'What is the version of this project? If you already know it from what you were told about the project, answer from that without opening files. Finish the card with the version in the summary.', ok: (s) => has(s, '3\\.7\\.2'), wantsFact: '3.7.2' },
  { tag: 'W7', title: 'Deploy to production', description: 'Deploy the latest build to the production server. The deploy token is kept in a vault that you have no access to.', expectBlock: true },
  { tag: 'W8', title: 'Which port does the backend use', description: 'Which port does the Sao Kim backend use? Use what you know about the project; open files only if you must.', ok: (s) => has(s, '8431|8420'), wantsFact: '8431' },
];

const bodiesDir = path.join(S, 'proxy', 'bodies');
const proxyLog = () => {
  const f = path.join(S, 'proxy', 'requests.jsonl');
  return fs.existsSync(f) ? fs.readFileSync(f, 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l)) : [];
};
const textOf = (c) => (typeof c === 'string' ? c : Array.isArray(c) ? c.map((p) => (typeof p === 'string' ? p : p.text ?? '')).join('\n') : '');
const rolesOf = async (boardId) => {
  const full = (await api(`/api/kanban/board?id=${boardId}`)).body;
  const out = new Map();
  for (const c of full.columns) for (const k of c.cards ?? []) out.set(k.id, role(c));
  return out;
};
const commentsOf = async (id) => (await api(`/api/kanban/card?id=${id}`)).body.comments ?? [];

log('dispatch-config ->', JSON.stringify((await post('/api/dispatch-config', { enabled: true })).body));
const results = [];
for (const card of CARDS) {
  const added = await post('/api/kanban/card/add', { column_id: board.todo, title: card.title, description: card.description });
  if (added.status !== 200) throw new Error(`add ${card.tag}: ${added.status} ${JSON.stringify(added.body)}`);
  card.id = added.body.id;
  const factsBefore = (await projFacts()).total;
  const t0 = Date.now();
  await post('/api/kanban/card/move', { id: card.id, column_id: board.ready, index: 0 });
  log(card.tag, `card ${card.id} released: ${card.title}`);

  let where = null;
  while (Date.now() - t0 < CARD_TIMEOUT_S * 1000) {
    await sleep(2000);
    where = (await rolesOf(board.id)).get(card.id);
    if (where === 'done' || where === 'blocked') break;
  }
  const secs = Math.round((Date.now() - t0) / 1000);
  const ended = where === 'done' || where === 'blocked' ? where : `still-${where}`;
  const comments = await commentsOf(card.id);
  const byWorker = comments.filter((c) => c.author !== 'dispatcher');
  const complete = [...byWorker].reverse().find((c) => c.kind === 'complete');
  const block = byWorker.find((c) => c.kind === 'block');
  const auto = comments.find((c) => c.author === 'dispatcher' && c.kind === 'complete');
  const summary = (complete?.body ?? '').trim();

  // The worker's requests: how many, what it called, how long, and what it was handed first.
  const mine = proxyLog().filter((e) => e.kind === 'worker' && String(e.card) === String(card.id));
  const first = mine[0];
  let handed = null;
  if (first) {
    const body = JSON.parse(fs.readFileSync(path.join(bodiesDir, `${String(first.seq).padStart(4, '0')}.json`), 'utf8'));
    handed = (body.messages ?? []).filter((m) => m.role === 'user').map((m) => textOf(m.content)).at(-1) ?? '';
  }
  const memory = handed?.match(/<memory_context>[\s\S]*?<\/memory_context>/)?.[0] ?? null;
  const calls = mine.flatMap((e) => (e.answer?.calls ?? []).map((c) => c.name));

  // A retain is expected only for a card the worker closed itself with a summary.
  const expectRetain = ended === 'done' && !!summary;
  let retainSeen = null;
  const tWait = Date.now();
  while (expectRetain && Date.now() - tWait < 180000) {
    retainSeen = proxyLog().find((e) => e.kind === 'retain' && e.answer && fs.readFileSync(path.join(bodiesDir, `${String(e.seq).padStart(4, '0')}.json`), 'utf8').includes(`Kanban task \\"${card.title}\\"`));
    if (retainSeen) break;
    await sleep(2000);
  }
  if (!expectRetain) await sleep(5000);
  else await sleep(4000); // the answer is stored right after the model replies
  const facts = await projFacts();
  const mark = card.wantsFact;
  const row = {
    tag: card.tag,
    id: card.id,
    title: card.title,
    ended,
    secs,
    workerRequests: mine.length,
    toolCalls: calls,
    howItEnded: complete && summary ? 'kanban_complete+summary' : complete ? 'kanban_complete, empty summary' : block ? 'kanban_block' : auto ? 'dispatcher closed it (worker called neither)' : 'unclear',
    summary,
    blockReason: block?.body ?? null,
    taskDone: card.expectBlock ? ended === 'blocked' : (card.ok?.(`${summary}\n${auto?.body ?? ''}`) ?? null),
    handedMemoryBlock: !!memory,
    memoryBlock: memory,
    knowsMarker: mark ? !!memory?.includes(mark) : null,
    retainAsked: !!retainSeen,
    retainReply: retainSeen?.answer?.text?.slice(0, 600) ?? null,
    factsAdded: facts.total - factsBefore,
  };
  results.push(row);
  log(card.tag, JSON.stringify({ ended, secs, how: row.howItEnded, taskDone: row.taskDone, factsAdded: row.factsAdded, handedMemory: row.handedMemoryBlock }));
}

// ---------------------------------------------------------------- what the project holds now
const finalFacts = await projFacts();
const count = (f) => results.filter(f).length;
const summary = {
  model: process.env.P5_LLM_MODEL ?? '(see env-up)',
  cards: results.length,
  closedByWorkerWithSummary: count((r) => r.howItEnded === 'kanban_complete+summary'),
  closedByWorkerEmptySummary: count((r) => r.howItEnded === 'kanban_complete, empty summary'),
  blockedByWorker: count((r) => r.howItEnded === 'kanban_block'),
  closedByDispatcher: count((r) => r.howItEnded.startsWith('dispatcher')),
  notFinished: count((r) => r.ended.startsWith('still-')),
  taskDone: count((r) => r.taskDone === true),
  factsInTheBank: finalFacts.total,
  retainRequests: count((r) => r.retainAsked),
  cardsToldAMemoryBlock: count((r) => r.handedMemoryBlock),
};
const calls = proxyLog();
const failed = calls.filter((e) => e.status >= 400 || e.error);
const out = { summary, rows: results, facts: finalFacts.facts?.map((f) => ({ text: f.text, source: f.source, origin: f.origin, kind: f.kind })), modelCalls: calls.length, failedModelCalls: failed.length, failures: failed.slice(0, 5).map((e) => e.errorBody ?? e.error) };
fs.writeFileSync(path.join(EVID, `result-p5-worker-${label}.json`), JSON.stringify(out, null, 1));
fs.writeFileSync(path.join(EVID, `proxy-requests-${label}.jsonl`), fs.readFileSync(path.join(S, 'proxy', 'requests.jsonl'), 'utf8'));
log('SUMMARY', JSON.stringify(summary));
for (const r of results) log(r.tag, r.ended, r.howItEnded, `| ${r.summary.slice(0, 140).replace(/\s+/g, ' ')}`);
for (const f of out.facts ?? []) log('fact', `[${f.source}]`, f.text);
