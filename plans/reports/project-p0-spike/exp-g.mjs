// P0 extra: does the existing Stop (agent:control stop -> stop_agent) pick up a changed allowedWorkDirs?
//   node exp-g.mjs <runLabel>
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { connect, steps, log, sleep, mockLog, SP } from './lib.mjs';

const label = process.argv[2];
const S = path.join(SP, `run-${label}`);
const A = path.join(S, 'proj', 'A');
const A2 = path.join(S, 'proj', 'A2');
fs.mkdirSync(A2, { recursive: true });
const daemonPid = fs.readFileSync(path.join(S, 'daemon.pid'), 'utf8').trim();
const rel = (p) => (typeof p === 'string' ? p.replaceAll(S + '/', '') : p);
const children = () =>
  execFileSync('ps', ['-axo', 'pid=,ppid=,command='], { encoding: 'utf8' })
    .split('\n')
    .filter((r) => new RegExp(`^\\s*\\d+\\s+${daemonPid}\\s+.*core-server`).test(r)).length;
const unwrap = (r) => {
  try {
    return JSON.parse(r).content?.[0]?.text ?? r;
  } catch {
    return r;
  }
};
const J = 'web:gshare:1';
const c = await connect(S);
c.register({ folder: 'gshare', name: 'G1', jid: J, groupType: 'code', allowedWorkDirs: [A] });
await sleep(1200);
const LOAD = { t: 'ToolSearch', a: { query: 'select:mcp__core__workspace_info', max_results: 5 } };
const turn = async (tag) => {
  await c.say(J, steps(tag, [LOAD, { t: 'Bash', a: { command: 'pwd', description: 'print cwd' } }, { t: 'mcp__core__workspace_info', a: {} }]), 90000);
  await sleep(1500);
  const recs = mockLog(S).filter((r) => r.tag === tag && !r.extraction);
  const fin = recs[recs.length - 1];
  const t = { tag, bashPwd: rel((fin.prevResults[1] ?? '').trim()), workspaceInfo: rel(unwrap(fin.prevResults[2] ?? '').replace(/\s+/g, ' ')), messagesInFirstRequest: recs[0].nMessages, coreServers: children() };
  log(JSON.stringify(t));
  return t;
};
const out = [];
out.push(await turn('G1'));
c.send({ type: 'update:group', jid: J, allowedWorkDirs: [A2] });
await sleep(1200);
c.send({ type: 'subscribe', groupJid: J });
await sleep(800);
c.send({ type: 'agent:control', groupJid: J, action: 'stop' });
await sleep(2500);
out.push(await turn('G2'));
fs.writeFileSync(path.join(S, 'result-g.json'), JSON.stringify(out, null, 1));
c.close();
