#!/usr/bin/env node
// Changed-mind eval through the REAL chat path: a person states a preference in a
// chat, later changes their mind, and the fact store must stop recalling the old
// one. Unlike `cargo test memory::engine::eval` (a scripted or real extractor fed
// fixed windows) this drives a running daemon the way the web UI does — chat turns
// over the WS gateway, the reflection window, the extraction call, retain — so it
// sees what the assistant's own replies, the long system prompt and the idle
// flush do to extraction.
//
// A run is isolated: its own agent folder, so earlier runs never leak into it.
// Pass = after the change, no LIVE fact still states the old preference, and the
// old fact is kept for history (listed with `history=true`, `supersededBy` set).
//
// Run it against a disposable daemon, never the real one (see CLAUDE.md "Never
// develop against a live install"):
//
//   S=$(mktemp -d); mkdir -p $S/home/.senclaw $S/root
//   # a model: any OpenAI-compatible endpoint, e.g. sen-mlx
//   #   ~/.senclaw/runtimes/sen-mlx/<v>/bin/sen-mlx serve --host 127.0.0.1 --port 38931 --model <dir>
//   # point the scratch daemon's three LLM slots at it in $S/home/.senclaw/config.json
//   #   (llmConfigs[0] = {id,label,provider:"custom",baseURL:"http://127.0.0.1:38931/v1",
//   #    modelName,adapt:"openai",apiKey:"local",maxTokens:2048,contextLength:32000},
//   #    activeLlmConfigId / activeQuickLlmConfigId / activeCognitiveLlmConfigId = its id)
//   cd $S/root && HOME=$S/home SENCLAW_HOME=$S/home/.senclaw SENCLAW_UI_PORT=28788 \
//     SENCLAW_WS_PORT=28789 SENCLAW_COGNITIVE_REFLECT_WINDOW_IDLE_MS=6000 senclaw start &
//   node scripts/evals/memory-changed-mind.mjs --runs 3
//
// Node 22+ (global WebSocket and fetch). Exit code 0 when every run passed.

const args = Object.fromEntries(
  process.argv.slice(2).flatMap((a, i, all) => (a.startsWith('--') ? [[a.slice(2), all[i + 1] ?? 'true']] : [])),
);
const UI = args.ui ?? 'http://127.0.0.1:28788';
const WS = args.ws ?? 'ws://127.0.0.1:28789';
const RUNS = Number(args.runs ?? 1);
const TURN_TIMEOUT = Number(args.timeout ?? 300) * 1000; // one chat turn
const FACT_TIMEOUT = Number(args['fact-timeout'] ?? 240) * 1000; // flush + extraction

// The real daemon is on these. A destructive eval must never go there.
for (const url of [UI, WS]) {
  if (/:(18788|18789)\b/.test(url)) {
    console.error(`refusing ${url}: 18788/18789 is the real daemon. Start a scratch one (see the header).`);
    process.exit(2);
  }
}

const OLD_TURN = args['old-turn'] ?? 'Mình thích dùng React cho frontend.';
const NEW_TURN = args['new-turn'] ?? 'Mình đổi ý rồi, giờ mình dùng Vue cho frontend, không dùng React nữa.';
const OLD = new RegExp(args.old ?? 'react', 'i');
const NEW = new RegExp(args.new ?? 'vue', 'i');

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const stamp = () => new Date().toISOString().slice(11, 19);
const log = (...a) => console.log(stamp(), ...a);

/** One chat connection: registers its group and sends turns, resolving when the agent is idle again. */
async function connect(folder, jid) {
  const ws = new WebSocket(WS);
  let waiter = null;
  let busy = false;
  await new Promise((resolve, reject) => {
    ws.onopen = () => {
      ws.send(JSON.stringify({ type: 'connect' })); // grants the admin the register call needs
      ws.send(JSON.stringify({ type: 'register:group', folder, name: folder, jid, channel: 'web' }));
      setTimeout(resolve, 2500);
    };
    ws.onerror = () => reject(new Error(`cannot reach ${WS}`));
  });
  ws.onmessage = (e) => {
    const m = JSON.parse(e.data);
    if (m.type === 'error') log('gateway error:', m.message);
    if (m.type === 'agent:state') {
      const state = JSON.stringify(m.state ?? '');
      if (/processing/i.test(state)) busy = true;
      if (/idle/i.test(state) && busy && waiter) {
        busy = false;
        waiter();
      }
    }
  };
  return {
    async say(text) {
      const done = new Promise((resolve, reject) => {
        waiter = resolve;
        setTimeout(() => reject(new Error(`no answer within ${TURN_TIMEOUT / 1000}s`)), TURN_TIMEOUT);
      });
      ws.send(JSON.stringify({ type: 'message', groupJid: jid, text }));
      await done;
    },
    close: () => ws.close(),
  };
}

async function facts(folder, history) {
  const r = await fetch(`${UI}/api/memory/facts?bank=${encodeURIComponent(folder)}&limit=200${history ? '&history=true' : ''}`);
  if (!r.ok) throw new Error(`GET facts → ${r.status}`);
  return (await r.json()).facts;
}

/** Wait for a live fact matching `re`; the reflection window flushes after the chat goes quiet. */
async function waitForFact(folder, re) {
  const until = Date.now() + FACT_TIMEOUT;
  while (Date.now() < until) {
    if ((await facts(folder, false)).some((f) => re.test(f.text))) return true;
    await sleep(4000);
  }
  return false;
}

async function oneRun(n) {
  const id = Math.random().toString(36).slice(2, 8);
  const folder = `eval-${id}`;
  const jid = `web:eval-${id}`;
  log(`run ${n}: agent folder ${folder}`);
  const chat = await connect(folder, jid);
  try {
    await chat.say(OLD_TURN);
    if (!(await waitForFact(folder, OLD))) return { pass: false, why: 'the old preference was never stored as a fact', folder };
    log(`run ${n}: old preference stored; changing mind`);
    await chat.say(NEW_TURN);
    if (!(await waitForFact(folder, NEW))) return { pass: false, why: 'the new preference was never stored as a fact', folder };
    // The flush that stores the new fact is the one that retires the old; give a trailing
    // window (the assistant's reply) time to land too, since its facts are part of the result.
    await sleep(Number(args.settle ?? 25) * 1000);
  } finally {
    chat.close();
  }
  const live = await facts(folder, false);
  const all = await facts(folder, true);
  // A live fact that states the old preference and not the new one is stale. The fact
  // that announces the change names both ("moved to Vue, no longer React") and is fine.
  const stale = live.filter((f) => OLD.test(f.text) && !NEW.test(f.text));
  const retired = all.filter((f) => f.supersededBy);
  return {
    pass: stale.length === 0 && retired.length > 0,
    why:
      stale.length > 0
        ? `${stale.length} live fact(s) still state the old preference`
        : retired.length === 0
          ? 'nothing was retired (the old fact is not in the history)'
          : 'ok',
    folder,
    live,
    retired,
    stale,
  };
}

const results = [];
for (let n = 1; n <= RUNS; n++) {
  try {
    const r = await oneRun(n);
    results.push(r);
    log(`run ${n}: ${r.pass ? 'PASS' : 'FAIL'} — ${r.why}`);
    for (const f of r.live ?? []) console.log(`    live    ${f.slot ? `<${f.slot.subject} · ${f.slot.predicate}> ` : ''}${f.text}`);
    for (const f of r.retired ?? []) console.log(`    retired ${f.text}`);
    for (const f of r.stale ?? []) console.log(`    STALE   ${f.text}`);
  } catch (e) {
    results.push({ pass: false, why: String(e.message ?? e) });
    log(`run ${n}: FAIL — ${e.message ?? e}`);
  }
}
const passed = results.filter((r) => r.pass).length;
log(`changed mind: ${passed}/${RUNS} runs passed`);
process.exit(passed === RUNS ? 0 : 1);
