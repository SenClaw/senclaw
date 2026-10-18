// P0 (c): does OneShotOptions.custom_rules reach the worker's system prompt?
// A workflow agent step maps  guidance -> custom_rules,  persona.system_prompt -> system_prompt.
import fs from 'node:fs';
import path from 'node:path';
import { api, log, sleep, mockLog, SP, wsteps } from './lib.mjs';

const label = process.argv[2];
const S = path.join(SP, `run-${label}`);
const wf = `---
name: p0-wf
description: spike workflow for custom_rules
version: "1"
guidance: GUIDANCE-DEF-MARKER-91c2
steps:
  - id: s1
    kind: agent
    persona: p0-worker
    prompt: '${wsteps('WF1', [])}'
    guidance: GUIDANCE-STEP-MARKER-b84d
    timeout: 60
---
spike
`;
fs.writeFileSync(path.join(S, 'home', 'senclaw', 'workflows', 'p0-wf.md'), wf);
const list = await api('/api/workflows');
log('workflows listed:', JSON.stringify(list.body).slice(0, 200));
const run = await api('/api/workflows/p0-wf/run', { method: 'POST', headers: { 'content-type': 'application/json' }, body: '{}' });
log('run:', run.status, JSON.stringify(run.body).slice(0, 300));
let state;
for (let i = 0; i < 40; i++) {
  await sleep(1500);
  const runs = await api('/api/workflows/runs');
  state = (runs.body.runs ?? runs.body)[0];
  if (state && !/running|pending|queued/i.test(state.status ?? '')) break;
}
log('final run state:', JSON.stringify(state).slice(0, 400));
const rec = mockLog(S).filter((r) => r.tag === 'WF1' && !r.extraction).slice(-1)[0];
if (!rec) {
  log('no worker request reached the mock');
  process.exit(1);
}
const sys = fs.readFileSync(path.join(S, 'mock', 'sys', `${String(rec.seq).padStart(4, '0')}.txt`), 'utf8');
const bodyPath = path.join(S, 'mock', 'bodies', `${String(rec.seq).padStart(4, '0')}.json`);
const body = fs.existsSync(bodyPath) ? fs.readFileSync(bodyPath, 'utf8') : '';
const out = {
  requestSeq: rec.seq,
  systemPromptChars: sys.length,
  personaMarkerInSystemPrompt: sys.includes('PERSONA-SYSTEM-MARKER-7f3a'),
  guidanceDefMarkerInSystemPrompt: sys.includes('GUIDANCE-DEF-MARKER-91c2'),
  guidanceStepMarkerInSystemPrompt: sys.includes('GUIDANCE-STEP-MARKER-b84d'),
  anyGuidanceMarkerAnywhereInRequestBody: /GUIDANCE-(DEF|STEP)-MARKER/.test(body),
  requestBodyBytes: body.length,
};
// Also look through EVERY message of the request body: the mock stores only the system text and the last message,
// so check the roles summary too.
out.roles = rec.roles;
fs.writeFileSync(path.join(S, 'result-c.json'), JSON.stringify(out, null, 1));
console.log(JSON.stringify(out, null, 1));
