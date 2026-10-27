// The spec flow with a real model (after up-lan.sh): brainstorm → draft → review → approve → code, with a scripted
// person who answers in Vietnamese and types only what a person would. Times every step, keeps a transcript.
// node real.mjs
import fs from 'node:fs';
import path from 'node:path';
import { execSync } from 'node:child_process';

const HERE = path.dirname(new URL(import.meta.url).pathname);
const S = path.join(HERE, 'lan');
const UI = 'http://127.0.0.1:48798', WSURL = 'ws://127.0.0.1:48799';
for (const u of [UI, WSURL]) if (/:(18788|18789)\b/.test(u)) throw new Error('refusing the real daemon ports');
const PROJ = fs.realpathSync(path.join(S, 'proj'));
const STORE = path.join(S, 'home/.senclaw/specs');
const TURN_MS = Number(process.env.TURN_MINUTES ?? 40) * 60 * 1000;
const t0 = Date.now();
const log = (...a) => console.log(`[${String(Math.round((Date.now() - t0) / 1000)).padStart(5)}s]`, ...a);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const api = async (p, init) => { const r = await fetch(UI + p, init); const t = await r.text(); try { return JSON.parse(t); } catch { return t; } };

const transcript = [];
let turnTools = [], turnReplies = [], busy = false, idle = null, waiter = null;
const ws = new WebSocket(WSURL);
await new Promise((res, rej) => { ws.onopen = () => { ws.send(JSON.stringify({ type: 'connect' })); setTimeout(res, 800); }; ws.onerror = () => rej(new Error('no ws')); });
ws.onmessage = (e) => {
  const m = JSON.parse(e.data);
  if (m.type === 'permission:request') { log('CARD', m.toolName, '|', m.title); turnTools.push({ card: m.toolName, title: m.title }); ws.send(JSON.stringify({ type: 'permission:response', requestId: m.requestId, optionKey: 'allow_once' })); }
  if (m.type === 'question:request') { log('QUESTION CARD', JSON.stringify(m).slice(0, 200)); ws.send(JSON.stringify({ type: 'question:response', requestId: m.requestId, answers: { 0: 0 } })); }
  if (m.type === 'tool:execution') {
    const out = String(m.summary ?? m.result ?? m.output ?? m.content ?? '').replace(/\n/g, ' ');
    turnTools.push({ tool: m.toolName, title: (m.title ?? '').slice(0, 120), out: out.slice(0, 300) });
    log('TOOL', m.toolName, '|', (m.title ?? '').slice(0, 90), '|', out.slice(0, 140));
  }
  if (m.type === 'agent:reply') { const t = m.text ?? m.content ?? ''; turnReplies.push(t); waiter?.(t); waiter = null; }
  if (m.type === 'agent:state') { const s = JSON.stringify(m.state ?? ''); if (/processing|thinking|running/i.test(s)) busy = true; if (/idle/i.test(s) && busy) { busy = false; idle?.(); } }
  if (m.type === 'session:error' || m.type === 'error') log('ERROR EVENT', JSON.stringify(m).slice(0, 300));
};

const created = await api('/api/code/sessions', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ name: 'spec-real', workspace: PROJ }) });
const jid = created.id;
log('session', jid, 'model', JSON.parse(fs.readFileSync(path.join(S, 'home/.senclaw/config.json'), 'utf8')).llmConfigs[0].modelName);

/** One agent turn: send, wait for idle, record. */
async function turn(step, text) {
  turnTools = []; turnReplies = []; busy = false;
  const started = Date.now();
  const done = new Promise((res, rej) => { idle = res; setTimeout(() => rej(new Error('timeout')), TURN_MS); });
  log(`>>> [${step}] person: ${text.slice(0, 160)}`);
  ws.send(JSON.stringify({ type: 'message', groupJid: jid, text }));
  let timedOut = false;
  try { await done; } catch { timedOut = true; log('TIMEOUT'); }
  await sleep(1500);
  const seconds = Math.round((Date.now() - started) / 1000);
  const reply = turnReplies.at(-1) ?? '';
  log(`<<< [${step}] ${seconds}s, ${turnTools.length} tool calls; agent: ${reply.slice(0, 700).replace(/\n/g, ' ')}`);
  const entry = { step, person: text, seconds, timedOut, startedAt: new Date(started).toISOString(), tools: turnTools, reply };
  transcript.push(entry);
  fs.writeFileSync(path.join(S, 'transcript.json'), JSON.stringify(transcript, null, 1));
  return entry;
}
/** A command the gateway answers itself. */
async function command(step, text) {
  const got = new Promise((res, rej) => { waiter = res; setTimeout(() => rej(new Error('no reply')), 30000); });
  log(`>>> [${step}] person: ${text}`);
  ws.send(JSON.stringify({ type: 'message', groupJid: jid, text }));
  const reply = await got.catch((e) => `(${e.message})`);
  log(`<<< [${step}] gateway: ${reply.replace(/\n/g, ' | ')}`);
  transcript.push({ step, person: text, gateway: reply });
  return reply;
}
const state = () => {
  if (!fs.existsSync(STORE)) return null;
  for (const k of fs.readdirSync(STORE)) { const f = path.join(STORE, k, 'state.json'); if (fs.existsSync(f)) return JSON.parse(fs.readFileSync(f, 'utf8')); }
  return null;
};
const specPath = () => { const s = state(); return s?.active ? path.join(PROJ, s.active) : null; };
const specText = () => { const p = specPath(); return p && fs.existsSync(p) ? fs.readFileSync(p, 'utf8') : ''; };
const runTests = () => {
  try { return 'PASS ' + execSync('python3 -m unittest 2>&1', { cwd: PROJ, timeout: 60000 }).toString().trim().split('\n').slice(-3).join(' | '); }
  catch (e) { return 'FAIL ' + ((e.stdout?.toString() ?? '') + (e.stderr?.toString() ?? '')).trim().split('\n').slice(-6).join(' | '); }
};
const refusals = (entry) => entry.tools.filter((t) => (t.out ?? '').includes('[spec]')).map((t) => t.out.slice(0, 160));
const summary = { model: 'google/gemma-4-e4b', steps: [] };
const note = (k, v) => { summary.steps.push({ [k]: v }); log('NOTE', k, JSON.stringify(v).slice(0, 400)); fs.writeFileSync(path.join(S, 'summary.json'), JSON.stringify(summary, null, 1)); };

// ---- 0. before any spec: the folder is held (required)
const pre = await turn('pre', 'Sửa hàm pending() trong todo.py để trả về danh sách sắp xếp theo id.');
note('pre_refusals', refusals(pre));

// ---- 1. brainstorm: answer questions until the brief is written
const ANSWER = 'Tôi dùng nó trong script cá nhân. Mỗi việc có thể có hạn chót (chỉ ngày, kiểu datetime.date, không bắt buộc). ' +
  'Cần một hàm liệt kê các việc quá hạn so với một ngày cho trước (mặc định hôm nay); việc đã xong không tính là quá hạn. ' +
  'Không được làm hỏng add/done/pending và các test hiện có. Ngoài phạm vi: nhắc nhở, lưu file, múi giờ, giờ phút.';
let e = await turn('brainstorm-1', '/brainstorm Thêm hạn chót cho công việc và liệt kê việc quá hạn');
note('brainstorm_open', { active: state()?.active, gateRefusals: refusals(e) });
for (let i = 2; i <= 7 && !/## Detailed|## Goals|## Overview/.test(specText()); i++) {
  e = await turn(`brainstorm-${i}`, i === 2 ? ANSWER : 'Đúng, tôi xác nhận các câu đó. Hãy ghi brief vào file spec ngay bây giờ.');
}
note('brief', { written: !!specText(), chars: specText().length, headings: (specText().match(/^## .+$/gm) ?? []) });
if (process.env.ONLY_BRAINSTORM) {
  note('brainstorm_questions', transcript.filter((x) => x.step.startsWith('brainstorm')).map((x) => ({ step: x.step, seconds: x.seconds, asked: /\?\s*$/.test(x.reply.replace(/<think>[\s\S]*?<\/think>/, '').trim()), tail: x.reply.replace(/<think>[\s\S]*?<\/think>/, '').trim().slice(-200) })));
  ws.close();
  process.exit(0);
}

// ---- 2. draft: agree until the implementation plan is written
e = await turn('draft-1', '/draft');
for (let i = 2; i <= 6 && !/^## Detailed Implementation Plan/m.test(specText()); i++) {
  e = await turn(`draft-${i}`, 'Đồng ý phần thiết kế. Viết tiếp phần Detailed Implementation Plan vào file spec.');
}
note('draft', { hasDetailed: /^## Detailed Implementation Plan/m.test(specText()), chars: specText().length, headings: (specText().match(/^##+ .+$/gm) ?? []) });
fs.copyFileSync(specPath() ?? '/dev/null', path.join(S, 'spec-after-draft.md'));

// ---- 3. review until approvable (max 3 rounds), timing each
let approved = '';
for (let round = 1; round <= 3; round++) {
  e = await turn(`review-${round}`, '/review');
  const r = state()?.specs?.[state()?.active]?.review;
  const lensSeconds = r ? Math.round((Date.parse(r.at) - Date.parse(e.startedAt)) / 1000) : null;
  note(`review_${round}`, { turnSeconds: e.seconds, lensSeconds, triageSeconds: lensSeconds == null ? null : e.seconds - lensSeconds, review: r, specChangedByTriage: e.tools.filter((t) => /Edit|Write/.test(t.tool ?? '')).length });
  const reportFile = r?.report ? path.join(STORE, fs.readdirSync(STORE)[0], r.report) : null;
  if (reportFile && fs.existsSync(reportFile)) fs.copyFileSync(reportFile, path.join(S, `review-${round}.md`));
  approved = await command(`approve-${round}`, '/approve');
  if (approved.startsWith('Approved')) break;
}
if (!approved.startsWith('Approved')) {
  approved = await command('approve-force', '/approve force');
  note('approve_forced', approved);
}
note('approved', approved);
fs.copyFileSync(specPath() ?? '/dev/null', path.join(S, 'spec-approved.md'));

// ---- 4. code: the agent must read the spec, then build it
e = await turn('build-1', 'Spec đã được duyệt. Hãy làm theo spec: sửa code, viết test và chạy test.');
note('build_1', { refusals: refusals(e), readSpec: e.tools.some((t) => t.tool === 'Read' && /docs\/designs/.test(t.title)), tests: runTests() });
for (let i = 2; i <= 3 && !/hạn|due|overdue|deadline/i.test(fs.readFileSync(path.join(PROJ, 'todo.py'), 'utf8')); i++) {
  e = await turn(`build-${i}`, 'Tiếp tục làm theo spec cho tới khi xong và test chạy được.');
  note(`build_${i}`, { refusals: refusals(e), tests: runTests() });
}
const status = await command('status', '/spec');
note('final', {
  tests: runTests(),
  diff: execSync('git diff --stat', { cwd: PROJ }).toString().trim(),
  untracked: execSync('git status --short', { cwd: PROJ }).toString().trim(),
  status,
  totalMinutes: Math.round((Date.now() - t0) / 60000),
});
ws.close();
process.exit(0);
