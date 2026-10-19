// Project P2 (a project's own facts), end to end on an isolated daemon + a scripted mock LLM.
//   P0_RUNS=<dir> P0_UI=48788 P0_WS=48789 P0_MOCK=48931 node exp-p2.mjs <runLabel>
// Needs ./env-up.sh <runLabel> <daemon binary> to have been run first (it starts the mock and the daemon).
//
// THE EXTRACTOR HERE IS A SCRIPT. The mock reads the facts out of ¤¤FACTS{...}¤¤ blocks that this driver puts in
// the chat messages; it understands nothing. So this proves the plumbing — where a chat's facts are written, what a
// chat's recall reads, the agent tools, the REST routes, the purge — and says NOTHING about how well a real model
// extracts facts from a conversation.
//
// What it checks, against the real REST routes, the real WS gateway, real `core-server` children and the real
// recall path of a chat turn (the request body the model receives is read from the mock's log):
//   write    a fact said in chat A1 of project P lands in bank project:P, tagged with no chat; the profile's bank is untouched
//   read     chat A2 of P is told it; a chat outside any project and a chat of project Q are not; common facts stay visible
//   mind     a change of mind in A2 retires what A1 said; A1 is then told the new value only
//   switches inherit_common (live), facts_write chat / off (live)
//   manual   POST /facts; a conversation that contradicts it does not replace it
//   tools    memory_search_facts / memory_forget_fact from a project chat, and from outside it
//   purge    DELETE ?purge=true removes the bank, its entities, links and text index, and nothing else
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { connect, steps, log, sleep, mockLog, api, SP } from '../project-p0-spike/lib.mjs';

const label = process.argv[2];
const S = path.join(SP, `run-${label}`);
const HOME_SC = path.join(S, 'home', '.senclaw');
const DB = path.join(HOME_SC, 'senclaw.db');
const A = path.join(S, 'proj', 'A');
const B = path.join(S, 'proj', 'B');
const C = path.join(S, 'proj', 'C');

const checks = [];
function check(name, ok, detail) {
  checks.push({ name, ok: !!ok, detail: detail ?? null });
  log(ok ? 'PASS' : 'FAIL', name, ok ? '' : JSON.stringify(detail));
}
const sql = (q) => execFileSync('sqlite3', ['-cmd', '.timeout 5000', DB, q], { encoding: 'utf8' }).trim();
const post = (p, body, method = 'POST') =>
  api(p, { method, headers: { 'content-type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) });
const textOf = (c) =>
  typeof c === 'string' ? c : Array.isArray(c) ? c.map((p) => (typeof p === 'string' ? p : p.text ?? '')).join('\n') : '';
const facts = (arr) => `¤¤FACTS${JSON.stringify({ facts: arr })}¤¤`;
const fact = (text, extra = {}) => ({ text, kind: 'world', when: null, entities: [], ...extra });

const c = await connect(S);
let n = 0;
/** Say something in a chat: `prose` is what a person would type, `list` what the scripted extractor will read out of it. */
async function say(jid, prose, list, toolSteps = []) {
  const tag = `${jid.split(':').pop()}-${++n}`;
  await c.say(jid, `${prose} ${steps(tag, toolSteps)} ${list ? facts(list) : ''}`);
  return tag;
}
/** The `<memory_context>` block the model was shown on the first request of a turn. */
function recallOf(tag) {
  const first = mockLog(S).find((e) => e.tag === tag && e.step === 0);
  if (!first) return null;
  const f = path.join(S, 'mock', 'bodies', `${String(first.seq).padStart(4, '0')}.json`);
  const body = JSON.parse(fs.readFileSync(f, 'utf8'));
  const last = body.messages.filter((m) => m.role === 'user').map((m) => textOf(m.content)).at(-1) ?? '';
  return last.match(/<memory_context>([\s\S]*?)<\/memory_context>/)?.[1] ?? '';
}
/** What the tool the turn called answered (the result arrives in the next request). */
function toolResult(tag, stepNo) {
  const e = mockLog(S).filter((x) => x.tag === tag && x.step === stepNo + 1).at(-1);
  return e ? e.prevResults.at(-1) : null;
}
const unwrap = (r) => {
  try {
    return JSON.parse(r).content?.[0]?.text ?? r;
  } catch {
    return r;
  }
};
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
const projFacts = async (id, qs = '') => (await api(`/api/projects/${id}/facts?${qs}`)).body;
const bankFacts = async (bank, qs = '') => (await api(`/api/memory/facts?bank=${encodeURIComponent(bank)}&${qs}`)).body;

// ---------------------------------------------------------------- projects and sessions
let r = await post('/api/projects', { name: 'Alpha', root: A });
const PA = r.body;
r = await post('/api/projects', { name: 'Beta', root: B });
const PB = r.body;
check('two projects are created', PA.id?.startsWith('prj_') && PB.id?.startsWith('prj_'), { PA, PB });
const BANK_A = `project:${PA.id}`;
const J = { a1: 'web:shared:a1', a2: 'web:shared:a2', b1: 'web:shared:b1', out: 'web:shared:out' };
c.register({ folder: 'shared', name: 'A one', jid: J.a1, groupType: 'chat', projectId: PA.id });
c.register({ folder: 'shared', name: 'A two', jid: J.a2, groupType: 'chat', projectId: PA.id });
c.register({ folder: 'shared', name: 'B one', jid: J.b1, groupType: 'chat', projectId: PB.id });
c.register({ folder: 'shared', name: 'Outside', jid: J.out, groupType: 'chat' });
await sleep(1500);

// A fact that belongs to the whole agent (bank = the profile folder, no chat): the "common" facts. Nothing in the
// product writes one under per-chat scoping any more, so it is put in the table directly.
sql(`insert into mem_facts (id,bank,kind,text,mentioned_at,created_at,content_hash,source,origin)
     values ('common-1','shared','world','Lan lives in quokkaville and works at the ferry company.',strftime('%s','now'),strftime('%s','now'),'common-1','upload','')`);
sql(`insert into mem_facts_fts (fact_id,text) values ('common-1','lan lives in quokkaville and works at the ferry company')`);
check('the common fact is visible through the agent facts API', (await bankFacts('shared')).facts?.some((f) => f.id === 'common-1'));
check('a new project has no facts', (await projFacts(PA.id)).total === 0);

// ---------------------------------------------------------------- write: chat A1 of project A
await say(J.a1, 'Please remember what I tell you about Alpha: the payment gateway port and the release day.', [
  fact('The Alpha payment gateway listens on port 7421.', { entities: ['Alpha', 'payment gateway'] }),
  fact('The Alpha release is deployed on Friday.', { entities: ['Alpha', 'release'] }),
]);
const afterA1 = await until('project A to hold two facts', async () => {
  const f = await projFacts(PA.id);
  return f.total >= 2 ? f : null;
});
check('what A1 said lands in the project bank (2 facts)', afterA1?.total === 2 && afterA1.bank === BANK_A, afterA1);
check(
  'project facts are tagged with no chat and were extracted by the conversation',
  afterA1?.facts.every((f) => f.origin === '' && f.source === 'reflection'),
  afterA1?.facts,
);
const shared = await bankFacts('shared');
check("the profile's own bank got none of it", shared.total === 1 && shared.facts.every((f) => f.id === 'common-1'), shared);
const banks = (await api('/api/memory/facts/banks')).body.banks;
check('the bank listing shows project:<id> beside the profile', banks.some((b) => b.bank === BANK_A && b.current === 2) && banks.some((b) => b.bank === 'shared'), banks);

// ---------------------------------------------------------------- read: who is told what
const Q = 'What port does the Alpha payment gateway listen on, and where does Lan live?';
const tA2 = await say(J.a2, Q, null);
const tOut = await say(J.out, Q, null);
const tB1 = await say(J.b1, Q, null);
const rA2 = recallOf(tA2), rOut = recallOf(tOut), rB1 = recallOf(tB1);
check('chat A2 of the project is told what A1 said', rA2?.includes('7421'), rA2);
check('…and labels it a fact', /\(fact\)[^\n]*\n[^\n]*7421/.test(rA2 ?? ''), rA2);
check('…and is still told the agent-wide fact', rA2?.includes('quokkaville'), rA2);
check('a chat outside every project is NOT told it', rOut !== null && !rOut.includes('7421'), rOut);
check('…but is told the agent-wide fact', rOut?.includes('quokkaville'), rOut);
check('a chat of another project is NOT told it', rB1 !== null && !rB1.includes('7421'), rB1);
check('…but is told the agent-wide fact', rB1?.includes('quokkaville'), rB1);

// ---------------------------------------------------------------- a change of mind in another chat
await say(J.a2, 'Correction for Alpha: the release moved from Friday to Thursday.', [
  fact('The Alpha release is now deployed on Thursday.', { entities: ['Alpha', 'release'], replaces_matching: 'deployed on Friday' }),
]);
const moved = await until('the Thursday fact to replace the Friday one', async () => {
  const h = await projFacts(PA.id, 'history=true&limit=50');
  const old = h.facts.find((f) => f.text.includes('Friday'));
  return old?.supersededBy ? h : null;
});
check('A2 retires what A1 said; the old fact is kept for history', moved && moved.facts.some((f) => f.text.includes('Thursday') && !f.supersededBy), moved?.facts);
const live = await projFacts(PA.id);
check('the live list has the new value and not the old', live.facts.some((f) => f.text.includes('Thursday')) && !live.facts.some((f) => f.text.includes('Friday')), live.facts);
const tA1 = await say(J.a1, 'When is the Alpha release deployed?', null);
const rA1 = recallOf(tA1);
check('A1, who said Friday, is now told Thursday and not Friday', rA1?.includes('Thursday') && !rA1.includes('Friday'), rA1);

// ---------------------------------------------------------------- inherit_common is read live
r = await post(`/api/projects/${PA.id}`, { inheritCommon: false }, 'PATCH');
check('PATCH inheritCommon=false', r.status === 200 && r.body.inheritCommon === false, r);
const tNoCommon = await say(J.a2, Q, null);
const rNoCommon = recallOf(tNoCommon);
check('inheriting nothing: the project’s own facts stay, the agent-wide fact goes', rNoCommon?.includes('7421') && !rNoCommon.includes('quokkaville'), rNoCommon);
const tOut2 = await say(J.out, Q, null);
check('…and a chat outside the project is unaffected', recallOf(tOut2)?.includes('quokkaville'));
await post(`/api/projects/${PA.id}`, { inheritCommon: true }, 'PATCH');
const tBack = await say(J.a2, Q, null);
check('inheriting again brings it back on the next turn', recallOf(tBack)?.includes('quokkaville'), recallOf(tBack));

// ---------------------------------------------------------------- a fact typed by hand
r = await post(`/api/projects/${PA.id}/facts`, { text: 'The Alpha database is PostgreSQL 16.' });
check('POST a manual fact → 201, source manual, no chat', r.status === 201 && r.body.fact?.source === 'manual' && r.body.fact?.origin === '', r);
const manualId = r.body.fact?.id;
r = await post(`/api/projects/${PA.id}/facts`, { text: 'the alpha database is postgresql 16.' });
check('the same words again → 200 existing, same fact', r.status === 200 && r.body.existing === true && r.body.fact.id === manualId, r);
r = await post(`/api/projects/${PA.id}/facts`, { text: 'short' });
check('a fragment is refused', r.status === 400 && r.body.error === 'invalid_fact', r);
r = await post(`/api/projects/${PA.id}/facts`, { text: 'The key is sk-proj-abcdefghijklmnopqrstuvwxyz1234 for Alpha' });
check('a secret is refused', r.status === 400 && r.body.error === 'secret_in_fact', r);
const tDb = await say(J.a2, 'Which database does Alpha use?', null);
check('chat A2 is told the typed fact', recallOf(tDb)?.includes('PostgreSQL 16'), recallOf(tDb));
await say(J.a1, 'Alpha moved its database to MySQL 9 last week.', [
  fact('The Alpha database is now MySQL 9.', { entities: ['Alpha', 'database'], replaces_matching: 'PostgreSQL 16' }),
]);
await until('the MySQL fact to be stored', async () => (await projFacts(PA.id, 'q=MySQL')).count >= 1);
const hist = await projFacts(PA.id, 'history=true&limit=50');
const typed = hist.facts.find((f) => f.id === manualId);
check('a conversation that contradicts the typed fact does not replace it', typed && !typed.supersededBy && typed.source === 'manual', typed);
check('…and what it said is kept beside it', hist.facts.some((f) => f.text.includes('MySQL 9') && !f.supersededBy), hist.facts);

// ---------------------------------------------------------------- the agent's own tools
const LOAD = { t: 'ToolSearch', a: { query: 'select:mcp__core__memory_search_facts,mcp__core__memory_forget_fact', max_results: 5 } };
const SEARCH = (q) => ({ t: 'mcp__core__memory_search_facts', a: { query: q } });
let t = await say(J.a2, 'Search your facts.', null, [LOAD, SEARCH('payment gateway'), SEARCH('quokkaville')]);
const inProject = [unwrap(toolResult(t, 1)), unwrap(toolResult(t, 2))];
check('memory_search_facts from a project chat finds the project’s fact', inProject[0]?.includes('7421'), inProject);
check('…and the agent-wide one', inProject[1]?.includes('quokkaville'), inProject);
t = await say(J.out, 'Search your facts.', null, [LOAD, SEARCH('payment gateway')]);
const outside = unwrap(toolResult(t, 1));
check('memory_search_facts from outside the project does not', outside && !outside.includes('7421'), outside);
t = await say(J.b1, 'Search your facts.', null, [LOAD, SEARCH('payment gateway')]);
const otherProject = unwrap(toolResult(t, 1));
check('…nor from another project', otherProject && !otherProject.includes('7421'), otherProject);

r = await post(`/api/projects/${PA.id}/facts`, { text: 'The Alpha staging host is called orion-staging.' });
const stagingId = r.body.fact.id;
t = await say(J.out, 'Forget the staging host.', null, [LOAD, { t: 'mcp__core__memory_forget_fact', a: { id: stagingId } }]);
const refused = unwrap(toolResult(t, 1));
check('memory_forget_fact from outside the project cannot reach its fact', /No fact with id/.test(refused ?? '') && (await projFacts(PA.id, 'q=orion')).count === 1, refused);
t = await say(J.a1, 'Forget the staging host.', null, [LOAD, { t: 'mcp__core__memory_forget_fact', a: { id: stagingId } }]);
const forgot = unwrap(toolResult(t, 1));
check('…from inside it can', /Forgot fact/.test(forgot ?? '') && (await projFacts(PA.id, 'q=orion')).count === 0, forgot);
r = await post(`/api/projects/${PA.id}/facts/${stagingId}`, undefined, 'DELETE');
check('DELETE of a fact that is already gone → 404', r.status === 404 && r.body.error === 'fact_not_found', r);
const bFact = (await post(`/api/projects/${PB.id}/facts`, { text: 'The Beta cache is redis-7 in production.' })).body.fact;
r = await post(`/api/projects/${PA.id}/facts/${bFact.id}`, undefined, 'DELETE');
check('a project cannot delete another project’s fact', r.status === 404 && (await projFacts(PB.id)).total === 1, r);

// ---------------------------------------------------------------- facts_write
const sharedBefore = (await bankFacts('shared')).total;
r = await post(`/api/projects/${PB.id}`, { factsWrite: 'chat' }, 'PATCH');
check('PATCH factsWrite=chat', r.body.factsWrite === 'chat', r);
await say(J.b1, 'Beta uses the hazelnut broker for its queue, remember that.', [fact('The Beta queue runs on the hazelnut broker.', { entities: ['Beta', 'hazelnut'] })]);
const inShared = await until('the chat-level fact in the profile bank', async () => {
  const f = await bankFacts('shared', 'q=hazelnut');
  return f.count >= 1 ? f : null;
});
check('facts_write=chat: it goes to the profile’s bank, tagged with the chat', inShared?.facts[0]?.origin === J.b1, inShared);
check('…and not into the project', (await projFacts(PB.id, 'q=hazelnut')).count === 0);
const tChatLevel = await say(J.b1, 'Which broker does the Beta queue use? hazelnut', null);
check('…and that chat is told it', recallOf(tChatLevel)?.includes('hazelnut'), recallOf(tChatLevel));
const tOtherChat = await say(J.out, 'Which broker does the Beta queue use? hazelnut', null);
check('…and no other chat is', !recallOf(tOtherChat)?.includes('hazelnut'), recallOf(tOtherChat));

const totalBefore = sql('select count(*) from mem_facts');
r = await post(`/api/projects/${PA.id}`, { factsWrite: 'off' }, 'PATCH');
check('PATCH factsWrite=off', r.body.factsWrite === 'off', r);
await say(J.a1, 'The Alpha cdn is fastly-edge, remember that.', [fact('The Alpha cdn is fastly-edge.', { entities: ['Alpha', 'cdn'] })]);
await sleep(8000);
check('facts_write=off: nothing is retained anywhere', sql('select count(*) from mem_facts') === totalBefore && sql("select count(*) from mem_facts where text like '%fastly%'") === '0', { totalBefore, now: sql('select count(*) from mem_facts') });
await post(`/api/projects/${PA.id}`, { factsWrite: 'project' }, 'PATCH');

// ---------------------------------------------------------------- purge
r = await post('/api/projects', { name: 'Gamma', root: C });
const PC = r.body;
const BANK_C = `project:${PC.id}`;
c.register({ folder: 'shared', name: 'C one', jid: 'web:shared:c1', groupType: 'chat', projectId: PC.id });
await sleep(1000);
await say('web:shared:c1', 'Gamma notes, remember these two things about it.', [
  fact('Gamma is written in Zig.', { entities: ['Gamma', 'Zig'] }),
  fact('Gamma is owned by Mai.', { entities: ['Gamma', 'Mai'] }),
]);
await until('Gamma to hold its two facts', async () => (await projFacts(PC.id)).total >= 2);
for (let i = 0; i < 40; i++) await post(`/api/projects/${PC.id}/facts`, { text: `Gamma note number ${i} about module ${i % 7}` });
const gammaTotal = (await projFacts(PC.id)).total;
check('Gamma holds 42 facts (2 said, 40 typed)', gammaTotal === 42, gammaTotal);
const alphaBefore = (await projFacts(PA.id, 'history=true&limit=200')).total;
const commonBefore = sql("select count(*) from mem_facts where bank='shared'");
r = await post(`/api/projects/${PC.id}?purge=true`, undefined, 'DELETE');
check('purge → 200 and says how many facts went', r.status === 200 && r.body.deletedFacts === 42 && r.body.detachedChats?.length === 1, r);
check('the bank is empty', sql(`select count(*) from mem_facts where bank='${BANK_C}'`) === '0');
check('no entity, link or text-index row is left without its fact',
  sql('select count(*) from mem_fact_entities where fact_id not in (select id from mem_facts)') === '0' &&
  sql('select count(*) from mem_links where from_id not in (select id from mem_facts) or to_id not in (select id from mem_facts)') === '0' &&
  sql('select count(*) from mem_facts_fts where fact_id not in (select id from mem_facts)') === '0');
check('another project’s facts and the common ones are untouched',
  sql(`select count(*) from mem_facts where bank='${BANK_A}'`) === String(alphaBefore) && sql("select count(*) from mem_facts where bank='shared'") === commonBefore);
check('the purged project is gone', (await api(`/api/projects/${PC.id}`)).status === 404 && (await api(`/api/projects/${PC.id}/facts`)).status === 404);
await say('web:shared:c1', 'Gamma is gone but this chat carries on; remember its cat is called Miso.', [fact('The chat’s cat is called Miso.', { entities: ['Miso'] })]);
const afterPurge = await until('the chat-level fact after the purge', async () => {
  const f = await bankFacts('shared', 'q=Miso');
  return f.count >= 1 ? f : null;
});
check('a chat whose project was purged remembers like any other chat again', afterPurge?.facts[0]?.origin === 'web:shared:c1' && sql(`select count(*) from mem_facts where bank='${BANK_C}'`) === '0', afterPurge);

// ---------------------------------------------------------------- the daemon is still healthy
await sleep(2000);
const dlog = fs.readFileSync(path.join(S, 'log', 'daemon.log'), 'utf8');
const bad = dlog.split('\n').filter((l) => /panicked|ERROR|\[facts\] (could not|extraction)/.test(l));
check('no panic and no fact error in the daemon log', bad.length === 0, bad.slice(0, 8));
check('the daemon still answers', (await api('/api/projects')).status === 200);
check('no gateway error was raised', c.state.errors.length === 0, c.state.errors);

c.close();
const failed = checks.filter((x) => !x.ok);
fs.mkdirSync(path.join(path.dirname(new URL(import.meta.url).pathname), 'evidence'), { recursive: true });
fs.writeFileSync(path.join(path.dirname(new URL(import.meta.url).pathname), 'evidence', 'result-p2.json'), JSON.stringify({ label, total: checks.length, failed: failed.length, checks }, null, 1));
log(`${checks.length - failed.length}/${checks.length} checks passed`);
process.exit(failed.length ? 1 : 0);
