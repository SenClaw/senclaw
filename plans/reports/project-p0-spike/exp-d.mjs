// P0 (d): destroy_agent_full(jid) on a live session, then keep chatting.
// Needs the spike build (a file trigger inside bind_events calls AgentPool::destroy_agent_full).
//   node exp-d.mjs <runLabel>
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { connect, steps, log, sleep, mockLog, SP } from './lib.mjs';

const label = process.argv[2];
const S = path.join(SP, `run-${label}`);
const HOME_SC = path.join(S, 'home', '.senclaw');
const A = path.join(S, 'proj', 'A');
const B = path.join(S, 'proj', 'B');
const A2 = path.join(S, 'proj', 'A2');
fs.mkdirSync(A2, { recursive: true });
fs.writeFileSync(path.join(A2, 'marker-A2.txt'), 'marker A2\n');
const daemonPid = fs.readFileSync(path.join(S, 'daemon.pid'), 'utf8').trim();
const rel = (p) => (typeof p === 'string' ? p.replaceAll(S + '/', '') : p);

const sh = (cmd, args) => {
  try {
    return execFileSync(cmd, args, { encoding: 'utf8', maxBuffer: 8 << 20 });
  } catch (e) {
    return '';
  }
};
/** core-server children of the daemon with the env that matters, keyed by their chat jid. */
function coreServers() {
  const rows = sh('ps', ['-axo', 'pid=,ppid=,command=']).split('\n').filter(Boolean);
  const out = [];
  for (const r of rows) {
    const m = r.trim().match(/^(\d+)\s+(\d+)\s+(.*)$/);
    if (!m || m[2] !== daemonPid || !/core-server/.test(m[3])) continue;
    const env = sh('ps', ['eww', '-p', m[1]]);
    const get = (k) => (env.match(new RegExp(`${k}=(\\S+)`)) ?? [])[1];
    out.push({
      pid: m[1],
      jid: get('SENCLAW_CHAT_JID'),
      stateFile: rel(get('SENCLAW_WORKSPACE_STATE_FILE')),
      defaultWorkspace: rel(get('SENCLAW_DEFAULT_WORKSPACE')),
      allowedWorkDirs: rel(get('SENCLAW_ALLOWED_WORK_DIRS')),
    });
  }
  return out;
}
const stateOf = (folder) => {
  try {
    return rel(JSON.parse(fs.readFileSync(path.join(HOME_SC, `workspace-state-${folder}.json`), 'utf8')).currentDir);
  } catch {
    return '(none)';
  }
};
const unwrap = (r) => {
  try {
    return JSON.parse(r).content?.[0]?.text ?? r;
  } catch {
    return r;
  }
};

const J1 = 'web:dshare:1';
const J2 = 'web:dshare:2';
const c = await connect(S);
const LOAD = { t: 'ToolSearch', a: { query: 'select:mcp__core__workspace_info', max_results: 5 } };
const turn = async (jid, tag) => {
  await c.say(jid, steps(tag, [LOAD, { t: 'Bash', a: { command: 'pwd', description: 'print cwd' } }, { t: 'mcp__core__workspace_info', a: {} }]), 90000);
  await sleep(1500);
  const recs = mockLog(S).filter((r) => r.tag === tag && !r.extraction);
  const first = recs[0];
  const fin = recs[recs.length - 1];
  const t = {
    tag,
    jid,
    nMessagesInFirstRequest: first.nMessages,
    rolesInFirstRequest: first.roles,
    bashPwd: rel((fin.prevResults[1] ?? '').trim()),
    workspaceInfo: rel(unwrap(fin.prevResults[2] ?? '').replace(/\s+/g, ' ')),
  };
  log(JSON.stringify(t));
  return t;
};

const res = { steps: [] };
c.register({ folder: 'dshare', name: 'D1', jid: J1, groupType: 'code', allowedWorkDirs: [A] });
c.register({ folder: 'dshare', name: 'D2', jid: J2, groupType: 'code', allowedWorkDirs: [B] });
await sleep(1500);
res.steps.push({ step: 'D1 turn 1', ...(await turn(J1, 'D11')) });
res.steps.push({ step: 'D1 turn 2', ...(await turn(J1, 'D12')) });
res.steps.push({ step: 'D2 turn 1', ...(await turn(J2, 'D21')) });
res.before = { coreServers: coreServers(), stateFileDshare: stateOf('dshare') };
log('BEFORE', JSON.stringify(res.before));

// Change the binding so a rebuilt core can only differ if it re-reads it.
c.send({ type: 'update:group', jid: J1, allowedWorkDirs: [A2] });
await sleep(1500);

// destroy_agent_full(J1)
fs.writeFileSync(path.join(HOME_SC, 'spike-destroy-full'), J1);
await sleep(2500);
res.afterDestroy = { coreServers: coreServers(), stateFileDshare: stateOf('dshare') };
log('AFTER DESTROY', JSON.stringify(res.afterDestroy));

res.steps.push({ step: 'D1 turn 3 (after destroy)', ...(await turn(J1, 'D13')) });
res.afterRecreate = { coreServers: coreServers(), stateFileDshare: stateOf('dshare') };
log('AFTER RECREATE', JSON.stringify(res.afterRecreate));
res.steps.push({ step: 'D2 turn 2 (sibling, after)', ...(await turn(J2, 'D22')) });

// Idempotence: destroy a jid that has no core, and one that never existed.
fs.writeFileSync(path.join(HOME_SC, 'spike-destroy-full'), 'web:dshare:nope');
await sleep(1500);
res.steps.push({ step: 'D1 turn 4 (after destroying a jid that does not exist)', ...(await turn(J1, 'D14')) });

fs.writeFileSync(path.join(S, 'result-d.json'), JSON.stringify(res, null, 1));
c.close();
log('done');
