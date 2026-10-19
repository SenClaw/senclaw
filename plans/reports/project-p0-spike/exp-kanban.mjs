// P0 extra: what does a REAL kanban worker (dispatcher -> run_one_shot) receive?
//   node exp-kanban.mjs <runLabel>      (start the daemon with SENCLAW_DISPATCH_ENABLED unset; this script turns it on)
import fs from 'node:fs';
import path from 'node:path';
import { api, log, sleep, mockLog, SP, wsteps } from './lib.mjs';

const label = process.argv[2];
const S = path.join(SP, `run-${label}`);
const A = path.join(S, 'proj', 'A');
const post = (p, body) => api(p, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });
const unwrap = (r) => {
  try {
    return JSON.parse(r).content?.[0]?.text ?? r;
  } catch {
    return r;
  }
};

async function board(title, workspace_dir) {
  const b = await post('/api/kanban/boards', { title, template_id: 'standard', ...(workspace_dir ? { workspace_dir } : {}) });
  const id = b.body.id ?? b.body.board_id ?? b.body;
  const full = await api(`/api/kanban/board?id=${id}`);
  const cols = full.body.columns;
  const byRole = (r) => cols.find((c) => c.role === r)?.id;
  return { id, todo: byRole('todo'), ready: byRole('ready'), cols: cols.map((c) => `${c.role}:${c.id}`) };
}
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

const withWs = await board('p0 with workspace', A);
const noWs = await board('p0 no workspace', null);
log('boards', JSON.stringify({ withWs, noWs }));
// dispatcher is global; cards stay in Todo/Ready until it is switched on
const c1 = await card(withWs, 'K1', 'P0 card with workspace');
const c2 = await card(noWs, 'K2', 'P0 card without workspace');
log('cards', c1, c2);
log('dispatch-config ->', JSON.stringify((await post('/api/dispatch-config', { enabled: true })).body));

const out = { boards: { withWs, noWs }, cards: { c1, c2 } };
for (const [tag, id] of [['K1', c1], ['K2', c2]]) {
  let rec;
  for (let i = 0; i < 60; i++) {
    await sleep(2000);
    rec = mockLog(S).filter((r) => r.tag === tag && !r.extraction);
    const fin = rec[rec.length - 1];
    if (fin && fin.answered?.text) break; // MOCK-DONE
  }
  if (!rec?.length) {
    out[tag] = { error: 'no worker request reached the mock' };
    continue;
  }
  const first = rec[0];
  const fin = rec[rec.length - 1];
  const seq = String(first.seq).padStart(4, '0');
  const sys = fs.readFileSync(path.join(S, 'mock', 'sys', `${seq}.txt`), 'utf8');
  const body = JSON.parse(fs.readFileSync(path.join(S, 'mock', 'bodies', `${seq}.json`), 'utf8'));
  const user = body.messages.filter((m) => m.role === 'user').map((m) => (typeof m.content === 'string' ? m.content : JSON.stringify(m.content)));
  const detail = await api(`/api/kanban/card?id=${id}`);
  out[tag] = {
    systemPromptChars: sys.length,
    systemPromptHead: sys.slice(0, 200),
    systemPromptIsOnlyGuidance: sys.startsWith('You are running Kanban task #') && !sys.includes('You are a helpful AI assistant'),
    hasKanbanGuidance: sys.includes('You are running Kanban task #'),
    hasDefaultAssistantPrompt: sys.includes('You are a helpful AI assistant'),
    nTools: first.nTools,
    kanbanToolsInRoster: (first.tools ?? []).filter((t) => /kanban/.test(t)),
    userMessageHead: user[0]?.slice(0, 160),
    userMessageHasCardTitle: user.join('\n').includes(tag === 'K1' ? 'P0 card with workspace' : 'P0 card without workspace'),
    bashPwd: (fin.prevResults[1] ?? '').trim().replace(S + '/', ''),
    writeResult: (fin.prevResults[2] ?? '').split('\n')[0].replace(S + '/', ''),
    completeResult: unwrap(fin.prevResults[3] ?? '').slice(0, 160),
    cardAfter: JSON.stringify(detail.body).slice(0, 300),
  };
  log(tag, JSON.stringify(out[tag]));
}
fs.writeFileSync(path.join(S, 'result-kanban.json'), JSON.stringify(out, null, 1));
