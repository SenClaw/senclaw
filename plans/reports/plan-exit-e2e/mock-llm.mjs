#!/usr/bin/env node
// Scripted OpenAI-compatible LLM for the plan-exit / skill-guard e2e (copied from code-v3-e2e, extended).
// Scripted OpenAI-compatible LLM for the Project P0 spike. No inference: it replays tool calls
// that the test puts in the user message as  @@STEPS{"tag":"A1","steps":[{"t":"Bash","a":{...}}]}@@
// and records every request (system prompt, tool roster, tool results) for later grep.
//
//   node mock-llm.mjs --port 38931 --out <dir>
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';

const args = Object.fromEntries(process.argv.slice(2).flatMap((a, i, all) => (a.startsWith('--') ? [[a.slice(2), all[i + 1]]] : [])));
const PORT = Number(args.port ?? 38931);
const OUT = args.out ?? '.';
fs.mkdirSync(path.join(OUT, 'sys'), { recursive: true });
fs.mkdirSync(path.join(OUT, 'bodies'), { recursive: true });
const LOG = path.join(OUT, 'requests.jsonl');
let seq = 0;
let judged = 0;

const textOf = (c) =>
  typeof c === 'string' ? c : Array.isArray(c) ? c.map((p) => (typeof p === 'string' ? p : p.text ?? '')).join('\n') : '';

/** Last user message that carries a directive, and how many tool results follow it. */
function plan(messages) {
  let at = -1;
  let directive = null;
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i];
    if (m.role !== 'user') continue;
    const t = textOf(m.content);
    const hit = t.match(/§§STEPS(\{[\s\S]*?\})§§/) ?? t.match(/¶¶STEPS(\{[\s\S]*?\})¶¶/);
    if (hit) {
      try {
        directive = JSON.parse(hit[1]);
        at = i;
        break;
      } catch {
        /* keep looking */
      }
    }
  }
  if (!directive) return { directive: null, done: 0, results: [], nudges: 0 };
  // After "clear context and start fresh" the only message is the restart prompt, which carries the
  // request (and so the directive) again: replay the part of the script meant for after the restart.
  if (textOf(messages[at].content).includes('cleared because they approved the plan') && directive.afterRestart) {
    directive = { ...directive, steps: directive.afterRestart };
  }
  const results = [];
  let assistantCalls = 0;
  let nudges = 0;
  for (let i = at + 1; i < messages.length; i++) {
    const m = messages[i];
    if (m.role === 'tool') results.push(textOf(m.content));
    if (m.role === 'assistant' && m.tool_calls) assistantCalls += m.tool_calls.length;
    if (m.role === 'user' && ['announces something and then makes no tool call', 'edited and run nothing yet', 'You must finish by calling `ExitPlanMode', 'You loaded the skill `'].some((n) => textOf(m.content).includes(n))) nudges += 1;
  }
  return { directive, done: assistantCalls, results, nudges };
}

/** The step to answer with. A '@text' step is a reply with no tool call (an announcement); a nudge moves on past it unless it is stubborn. */
function pick(directive, done, nudges) {
  let tools = done;
  let texts = 0;
  for (let i = 0; i < directive.steps.length; i++) {
    const s = directive.steps[i];
    if (s.t === '@text') {
      texts += 1;
      if (!s.a?.stubborn && nudges >= texts) continue;
      return s;
    }
    if (tools > 0) { tools -= 1; continue; }
    return s;
  }
  return null;
}

function reply(res, stream, msg) {
  // msg: {text} | {call:{id,name,arguments}}
  if (!stream) {
    const message = msg.call
      ? { role: 'assistant', content: null, tool_calls: [{ id: msg.call.id, type: 'function', function: { name: msg.call.name, arguments: msg.call.arguments } }] }
      : { role: 'assistant', content: msg.text };
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ id: 'mock', object: 'chat.completion', created: 0, model: 'mock-model', choices: [{ index: 0, message, finish_reason: msg.call ? 'tool_calls' : 'stop' }], usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 } }));
    return;
  }
  res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
  const send = (o) => res.write(`data: ${JSON.stringify(o)}\n\n`);
  const base = { id: 'mock', object: 'chat.completion.chunk', created: 0, model: 'mock-model' };
  send({ ...base, choices: [{ index: 0, delta: { role: 'assistant' }, finish_reason: null }] });
  if (msg.call) {
    send({ ...base, choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: msg.call.id, type: 'function', function: { name: msg.call.name, arguments: '' } }] }, finish_reason: null }] });
    send({ ...base, choices: [{ index: 0, delta: { tool_calls: [{ index: 0, function: { arguments: msg.call.arguments } }] }, finish_reason: null }] });
    send({ ...base, choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }], usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 } });
  } else {
    send({ ...base, choices: [{ index: 0, delta: { content: msg.text }, finish_reason: null }] });
    send({ ...base, choices: [{ index: 0, delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 10, completion_tokens: 5, total_tokens: 15 } });
  }
  res.write('data: [DONE]\n\n');
  res.end();
}

http
  .createServer((req, res) => {
    if (req.method === 'GET' && req.url.startsWith('/v1/models')) {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ object: 'list', data: [{ id: 'mock-model', object: 'model', owned_by: 'mock' }] }));
      return;
    }
    if (req.method !== 'POST' || !req.url.includes('/chat/completions')) {
      res.writeHead(404);
      res.end('{}');
      return;
    }
    let raw = '';
    req.on('data', (d) => (raw += d));
    req.on('end', () => {
      const body = JSON.parse(raw || '{}');
      const messages = body.messages ?? [];
      const n = ++seq;
      const sys = messages.filter((m) => m.role === 'system').map((m) => textOf(m.content)).join('\n----\n');
      fs.writeFileSync(path.join(OUT, 'sys', `${String(n).padStart(4, '0')}.txt`), sys);
      if (/§§STEPS|¶¶STEPS/.test(raw)) fs.writeFileSync(path.join(OUT, 'bodies', `${String(n).padStart(4, '0')}.json`), raw);
      const { directive, done, results, nudges } = plan(messages);
      const tools = (body.tools ?? []).map((t) => t.function?.name ?? t.name);
      if (directive && !fs.existsSync(path.join(OUT, 'tools.json'))) fs.writeFileSync(path.join(OUT, 'tools.json'), JSON.stringify(body.tools ?? [], null, 1));
      const last = messages[messages.length - 1] ?? {};
      let out;
      let stepName = null;
      const isExtraction = sys.startsWith('You extract durable memories');
      const isJudge = sys.startsWith('You check whether an AI coding assistant');
      if (isJudge) {
        // finish judge: not complete the first time, complete after
        judged += 1;
        out = { text: judged === 1 ? JSON.stringify({ complete: false, missing: ['JUDGE-MISSING-ITEM: show the passing run'] }) : JSON.stringify({ complete: true, missing: [] }) };
      } else if (isExtraction) {
        // Memory-engine fact extraction: answer with three dated facts about "Orion", tagged by the chat's script tag.
        const all = messages.map((m) => textOf(m.content)).join('\n');
        const tg = (all.match(/"tag":"([A-Za-z0-9_-]+)"/) ?? [, 'untagged'])[1];
        out = {
          text: JSON.stringify({
            facts: [
              { text: `${tg}: the deploy day for project Orion is Friday.`, kind: 'world', when: '2026-10-18', entities: ['Orion', 'Friday', 'user'], subject: '', predicate: '' },
              { text: `${tg}: Orion uses PostgreSQL 16 as its database.`, kind: 'world', when: '2026-10-18', entities: ['Orion', 'PostgreSQL', 'user'], subject: '', predicate: '' },
              { text: `${tg}: Mai owns the Orion release checklist.`, kind: 'world', when: '2026-10-18', entities: ['Mai', 'Orion'], subject: '', predicate: '' },
            ],
          }),
        };
      } else if (directive && pick(directive, done, nudges)) {
        const s = pick(directive, done, nudges);
        stepName = s.t;
        out = s.t === '@text' ? { text: s.a?.text ?? '' } : { call: { id: `call_${n}`, name: s.t, arguments: JSON.stringify(s.a ?? {}) } };
      } else if (directive) {
        out = { text: `MOCK-DONE ${directive.tag}` };
      } else {
        out = { text: '[]' }; // not a scripted turn (title, extraction, router…): a harmless empty answer
      }
      fs.appendFileSync(
        LOG,
        JSON.stringify({
          seq: n,
          t: new Date().toISOString(),
          stream: !!body.stream,
          tag: directive?.tag ?? null,
          step: directive ? done : null,
          stepName,
          sysLen: sys.length,
          nMessages: messages.length,
          nTools: tools.length,
          planModeOffered: tools.includes('EnterPlanMode'),
          tools: n <= 3 || process.env.LOG_TOOLS ? tools : undefined,
          extraction: isExtraction || undefined,
          judge: isJudge || undefined,
          roles: messages.map((m) => `${m.role}:${textOf(m.content).slice(0, 60).replace(/\n/g, ' ')}`),
          restarted: messages.some((m) => m.role === 'user' && textOf(m.content).includes('cleared because they approved the plan')),
          lastRole: last.role,
          lastText: textOf(last.content).slice(0, 20000),
          prevResults: results,
          answered: out.call ? { call: out.call.name, args: out.call.arguments } : { text: out.text },
        }) + '\n',
      );
      reply(res, !!body.stream, out);
    });
  })
  .listen(PORT, '127.0.0.1', () => console.log(`mock-llm on ${PORT}, log ${LOG}`));
