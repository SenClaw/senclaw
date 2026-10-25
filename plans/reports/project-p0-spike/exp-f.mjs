// P0 extra: DAG queue and stop are keyed by the agent profile folder, not by the session.
import fs from 'node:fs';
import path from 'node:path';
import { connect, steps, wsteps, log, sleep, SP } from './lib.mjs';
const label = process.argv[2];
const S = path.join(SP, `run-${label}`);
const A = path.join(S, 'proj', 'A');
const B = path.join(S, 'proj', 'B');
const DS = path.join(S, 'home', '.senclaw', 'dispatch-state.json');
const J1 = 'web:fshare:1';
const J2 = 'web:fshare:2';
const view = () => {
  const st = JSON.parse(fs.readFileSync(DS, 'utf8'));
  return st.parents
    .filter((p) => p.adminFolder === 'fshare')
    .map((p) => ({
      id: p.id,
      chat: p.chatJid,
      status: p.status,
      sharedWorkspace: p.sharedWorkspace ?? null,
      tasks: p.tasks.map((t) => `${t.label}:${t.status}${t.error || t.result ? ` (${String(t.error || t.result).slice(0, 60)})` : ''}`),
    }));
};
const c = await connect(S);
c.register({ folder: 'fshare', name: 'F1', jid: J1, groupType: 'code', allowedWorkDirs: [A] });
c.register({ folder: 'fshare', name: 'F2', jid: J2, groupType: 'code', allowedWorkDirs: [B] });
await sleep(1200);
const LOAD = { t: 'ToolSearch', a: { query: 'select:mcp__core__create_parent', max_results: 5 } };
const dag = (tag, secs) => ({
  t: 'mcp__core__create_parent',
  a: {
    goal: tag,
    timeoutSeconds: 120,
    tasks: [
      { label: 'slow', agentName: 'persona:p0-worker', io: 'readOnly', prompt: wsteps(`W-${tag}-slow`, [{ t: 'Bash', a: { command: `sleep ${secs}`, description: 'wait' } }]) },
      { label: 'fast', agentName: 'persona:p0-worker', io: 'readOnly', prompt: wsteps(`W-${tag}-fast`, [{ t: 'Bash', a: { command: 'pwd', description: 'cwd' } }]) },
    ],
  },
});
await c.say(J1, steps('F1dag', [LOAD, dag('F1dag', 40)]), 60000);
await sleep(1500);
log('after F1 create_parent:', JSON.stringify(view()));
await c.say(J2, steps('F2dag', [LOAD, dag('F2dag', 1)]), 60000);
await sleep(1500);
log('after F2 create_parent (F1 DAG still running):', JSON.stringify(view()));
// F2 presses Stop. It is a different session from the one that started F1's DAG.
c.send({ type: 'subscribe', groupJid: J2 });
await sleep(800);
c.send({ type: 'agent:control', groupJid: J2, action: 'stop' });
await sleep(3000);
log('after F2 Stop:', JSON.stringify(view()));
fs.writeFileSync(path.join(S, 'result-f.json'), JSON.stringify(view(), null, 1));
c.close();
