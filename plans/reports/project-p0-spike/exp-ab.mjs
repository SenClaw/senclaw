// P0 (a) + (b): two code sessions on ONE agent profile, different allowedWorkDirs.
//   node exp-ab.mjs <runLabel> <bundled|separate>
import fs from 'node:fs';
import path from 'node:path';
import { connect, steps, wsteps, log, sleep, mockLog, SP } from './lib.mjs';

const label = process.argv[2];
const mode = process.argv[3] ?? 'bundled';
const S = path.join(SP, `run-${label}`);
const A = path.join(S, 'proj', 'A');
const B = path.join(S, 'proj', 'B');
const STATE = path.join(S, 'home', '.senclaw', 'workspace-state-shared.json');
const FOLDER = 'shared';
const JID = { A: 'web:shared:a', B: 'web:shared:b' };

const N =
  mode === 'bundled'
    ? { ws: 'mcp__core__workspace_info', ds: 'mcp__core__datagen_start', doc: 'mcp__core__doc_write', dag: 'mcp__core__create_parent_and_run' }
    : { ws: 'mcp__workspace__workspace_info', ds: 'mcp__datagen__datagen_start', doc: 'mcp__docs__doc_write', dag: 'mcp__dispatch__create_parent_and_run' };
const LOAD = { t: 'ToolSearch', a: { query: `select:${Object.values(N).join(',')}`, max_results: 20 } };

const readState = () => {
  try {
    return JSON.parse(fs.readFileSync(STATE, 'utf8')).currentDir;
  } catch (e) {
    return `(no file: ${e.code})`;
  }
};
const rel = (p) => (typeof p === 'string' ? p.replace(S + '/', '') : p);
const ls = (d) => (fs.existsSync(d) ? fs.readdirSync(d).sort() : []);
const walk = (d, base = d) =>
  ls(d).flatMap((n) => {
    const f = path.join(d, n);
    return fs.statSync(f).isDirectory() ? walk(f, base) : [path.relative(base, f)];
  });

const out = { label, mode, turns: [] };
const c = await connect(S);

async function turn(who, tag, extra = []) {
  const list = [
    LOAD,
    { t: 'Bash', a: { command: 'pwd', description: 'print cwd' } },
    { t: N.ws, a: {} },
    { t: 'Write', a: { file_path: `rel-${tag}.txt`, content: tag } },
    { t: N.ds, a: { pipeline: 'import', name: `imp-${tag}`, kind: 'labeled_text', license: 'private', source: { file: 'seeds.jsonl' } } },
    ...(mode === 'bundled' ? [] : [{ t: N.doc, a: { path: `p0-${tag}.md`, content: `# ${tag}\n`, overwrite: true } }]),
    ...extra,
  ];
  const before = { stateFile: readState() };
  await c.say(JID[who], steps(tag, list), 120000);
  await sleep(2500);
  const recs = mockLog(S).filter((r) => r.tag === tag && !r.extraction);
  const fin = recs[recs.length - 1];
  const res = fin?.prevResults ?? [];
  const unwrap = (r) => {
    try {
      const j = JSON.parse(r);
      return j.content?.[0]?.text ?? r;
    } catch {
      return r;
    }
  };
  const t = {
    who,
    tag,
    stateFileBefore: rel(before.stateFile),
    stateFileAfter: rel(readState()),
    bashPwd: rel((res[1] ?? '').trim()),
    workspaceInfo: res[2] ? rel(unwrap(res[2]).replace(/\s+/g, ' ')) : null,
    writeRel: (res[3] ?? '').slice(0, 160).replace(S + '/', ''),
    datagen: rel(unwrap(res[4] ?? '').replace(/\s+/g, ' ').slice(0, 220)),
    doc: res[5] ? rel(unwrap(res[5]).replace(/\s+/g, ' ').slice(0, 260)) : undefined,
    dirA: walk(A),
    dirB: walk(B),
  };
  const ds = path.join(S, 'home', 'senclaw', 'datasets', `imp-${tag}`, 'data.jsonl');
  t.datasetText = fs.existsSync(ds) ? fs.readFileSync(ds, 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l).text ?? l).join(' | ') : '(no dataset)';
  out.turns.push(t);
  log(JSON.stringify(t));
  return t;
}

c.register({ folder: FOLDER, name: 'A', jid: JID.A, groupType: 'code', allowedWorkDirs: [A] });
await sleep(1200);
await turn('A', 'A1');
c.register({ folder: FOLDER, name: 'B', jid: JID.B, groupType: 'code', allowedWorkDirs: [B] });
await sleep(1200);
await turn('B', 'B1');
await turn('A', 'A2');
await turn('B', 'B2');

// (b) DAG from each session. Workers are virtual personas, so their cwd is the parent's shared workspace.
const dagSteps = (who, tag) => [
  {
    t: N.dag,
    a: {
      goal: `p0 ${tag}`,
      timeoutSeconds: 60,
      tasks: [1, 2].map((i) => ({
        label: `w${i}`,
        agentName: 'persona:p0-worker',
        io: 'readOnly',
        prompt: wsteps(`W-${tag}-${i}`, [
          { t: 'Bash', a: { command: 'pwd', description: 'print cwd' } },
          { t: 'Write', a: { file_path: `dag-${tag}-${i}.txt`, content: `${tag}-${i}` } },
        ]),
      })),
    },
  },
];
async function dag(who, tag) {
  const state0 = readState();
  await c.say(JID[who], steps(tag, [LOAD, ...dagSteps(who, tag)]), 150000);
  await sleep(2500);
  const workers = mockLog(S)
    .filter((r) => r.tag?.startsWith(`W-${tag}-`) && r.step === 2)
    .map((r) => ({ tag: r.tag, bashPwd: rel((r.prevResults[0] ?? '').trim()), write: (r.prevResults[1] ?? '').replace(S + '/', '').slice(0, 140) }));
  const recs = mockLog(S).filter((r) => r.tag === tag && !r.extraction);
  const fin = recs[recs.length - 1];
  const t = { who, tag, kind: 'dag', stateFile: rel(state0), workers, dagResult: rel((fin?.prevResults?.[1] ?? '').replace(/\s+/g, ' ').slice(0, 400)), dirA: walk(A), dirB: walk(B) };
  out.turns.push(t);
  log(JSON.stringify(t));
}
await dag('A', 'A3');
await dag('B', 'B3');

fs.writeFileSync(path.join(S, 'result-ab.json'), JSON.stringify(out, null, 1));
c.close();
log('done');
