/**
 * Tests for the Node Space App SDK.
 *
 * Deliberately run against `dist/` rather than `src/`: that is the artefact
 * `npm publish` ships and the artefact the in-repo apps resolve through their
 * `file:` dependency, so a broken build is a failing test rather than a
 * surprise at install time.
 *
 * Run: `npm test` (builds first). Node's own runner — no test framework.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';

import { SenclawSpace } from '../dist/index.js';
import {
  handleDispatch, mcpServer, outcome, workspace,
} from '../dist/dispatch.js';
import { validateManifest } from '../dist/lifecycle.js';

/** A daemon stub that records what the SDK actually put on the wire. */
async function fakeDaemon(reply = { status: 'ok' }) {
  const seen = [];
  const server = http.createServer((req, res) => {
    let body = '';
    req.on('data', c => { body += c; });
    req.on('end', () => {
      seen.push({ path: req.url, body: body ? JSON.parse(body) : null });
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify(reply));
    });
  });
  await new Promise(r => server.listen(0, '127.0.0.1', r));
  const port = server.address().port;
  return {
    seen,
    client: () => SenclawSpace.forDaemon('t', `http://127.0.0.1:${port}`),
    close: () => new Promise(r => server.close(r)),
  };
}

// ---------------------------------------------------------------------------
// The bridge wire contract
// ---------------------------------------------------------------------------
//
// The daemon's `SpaceAppBridgeBody` requires a field named `action` and defines
// no alias. Sending anything else — `capability` was the mistake that prompted
// these tests — is a 422 from serde before a line of handler code runs, which
// surfaces to an app author as "the bridge is down" rather than "you sent the
// wrong key". So the key is pinned here rather than trusted.

test('bridge sends `action`, never `capability`', async () => {
  const d = await fakeDaemon({ status: 'ok', text: 'hi' });
  try {
    await d.client().llm({ prompt: 'q' });
    const { body } = d.seen[0];
    assert.equal(body.action, 'llm.request');
    assert.ok(!('capability' in body), 'the daemon 422s on this');
    assert.equal(body.payload.prompt, 'q');
  } finally { await d.close(); }
});

test('llmDetailed surfaces model, finish and usage', async () => {
  const d = await fakeDaemon({
    status: 'ok', text: 'hello', model: 'm1', finish: 'length',
    usage: { inputTokens: 12, outputTokens: 3, cacheReadTokens: 9 },
  });
  try {
    const r = await d.client().llmDetailed({ prompt: 'q' });
    assert.equal(r.text, 'hello');
    assert.equal(r.model, 'm1');
    assert.equal(r.finish, 'length');
    assert.equal(r.usage.inputTokens, 12);
    assert.equal(r.usage.cacheReadTokens, 9);
    // Unreported by this provider, and 0 is the right reading of absent.
    assert.equal(r.usage.cacheCreationTokens, 0);
  } finally { await d.close(); }
});

test('llmDetailed reports usage as null when the provider sent none', async () => {
  // Distinct from "zero tokens" — some local models report nothing at all, and
  // recording that as 0 would quietly understate the daemon's totals.
  const d = await fakeDaemon({ status: 'ok', text: 'x', model: 'local' });
  try {
    assert.equal((await d.client().llmDetailed({ prompt: 'q' })).usage, null);
  } finally { await d.close(); }
});

test('llm throws rather than returning a truncated reply', async () => {
  const d = await fakeDaemon({ status: 'ok', text: 'partial', finish: 'length' });
  try {
    await assert.rejects(() => d.client().llm({ prompt: 'q' }), /truncated/);
  } finally { await d.close(); }
});

test('knowledge calls use the right actions and omit an absent space', async () => {
  const d = await fakeDaemon({ status: 'ok', hits: [{ name: 'n', summary: 's', score: 0.5 }] });
  try {
    const c = d.client();
    await c.knowledgeSave('remember', { space: 'proj', tags: ['a'] });
    const hits = await c.knowledgeSearch('q', { space: 'proj', limit: 3 });
    await c.knowledgeSave('private');
    assert.deepEqual(d.seen.map(s => s.body.action),
      ['knowledge.save', 'knowledge.search', 'knowledge.save']);
    assert.deepEqual(d.seen[0].body.payload.tags, ['a']);
    assert.equal(d.seen[1].body.payload.limit, 3);
    assert.equal(hits[0].name, 'n');
    assert.equal(hits[0].score, 0.5);
    // Omitted means "this app's own private space" — sending space:null would
    // be a different thing to the daemon than not sending the key at all.
    assert.ok(!('space' in d.seen[2].body.payload));
  } finally { await d.close(); }
});

test('usageReport never throws at the caller', async () => {
  // Fire-and-forget: accounting must not take down the work it describes.
  const c = SenclawSpace.forDaemon('t', 'http://127.0.0.1:9'); // nothing listening
  await c.usageReport({ model: 'm', provider: 'p', inputTokens: 1, outputTokens: 2 });
});

test('listModels reads /api/llm-config and skips entries with no id', async () => {
  const d = await fakeDaemon({
    activeId: 'a1',
    configs: [{ id: 'a1', modelName: 'Sonnet', adapt: 'anthropic' }, { nope: 1 }],
  });
  try {
    const { activeId, models } = await d.client().listModels();
    assert.equal(activeId, 'a1');
    assert.equal(models.length, 1);
    assert.equal(models[0].provider, 'anthropic');
    assert.equal(d.seen[0].path, '/api/llm-config');
  } finally { await d.close(); }
});

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

const provider = {
  finalized: [],
  beats: [],
  claimReady: (cap) => Array.from({ length: cap.total }, (_, i) => ({
    id: `t${i}`,
    prompt: 'do it',
    mcp: [mcpServer.stdio('kanban', 'senclaw', ['kanban-server'])],
    workspace: workspace.worktree('/repo', 'main'),
    depends_on: i ? ['t0'] : [],
    timeout_secs: 60,
  })),
  heartbeat(id) { this.beats.push(id); },
  reclaim: () => ['stale-1'],
  finalize(id, o) { this.finalized.push([id, o]); },
};

test('poll serialises the Rust wire shape', async () => {
  const r = await handleDispatch(provider, 'poll', { capacity: { total: 2, per_assignee: 1 } });
  assert.equal(r.status, 200);
  const it = r.body[1];
  // snake_case, exactly as serde expects — camelCase is dropped silently,
  // which would surface as a dependency that never held.
  assert.deepEqual(it.depends_on, ['t0']);
  assert.equal(it.timeout_secs, 60);
  assert.deepEqual(it.workspace, { kind: 'worktree', repo: '/repo', branch: 'main' });
  assert.equal(it.mcp[0].transport, 'stdio');
  assert.deepEqual(it.mcp[0].args, ['kanban-server']);
});

test('heartbeat, reclaim and finalize round-trip', async () => {
  await handleDispatch(provider, 'heartbeat', { item_id: 't1' });
  assert.deepEqual(provider.beats, ['t1']);
  assert.deepEqual((await handleDispatch(provider, 'reclaim', {})).body, ['stale-1']);
  await handleDispatch(provider, 'finalize', { item_id: 't1', outcome: outcome.completed('done', { n: 1 }) });
  assert.deepEqual(provider.finalized, [['t1', { status: 'completed', summary: 'done', metadata: { n: 1 } }]]);
});

test('a provider error becomes 500 {error}, not a thrown exception', async () => {
  const broken = { claimReady() { throw new Error('db is gone'); }, finalize() {} };
  const r = await handleDispatch(broken, 'poll', {});
  assert.equal(r.status, 500);
  assert.equal(r.body.error, 'db is gone');
});

test('optional provider methods may be omitted', async () => {
  // heartbeat/reclaim are optional: a source with no lease model should not
  // have to write two empty functions to be dispatchable.
  const minimal = { claimReady: () => [], finalize() {} };
  assert.equal((await handleDispatch(minimal, 'heartbeat', { item_id: 'x' })).status, 200);
  assert.deepEqual((await handleDispatch(minimal, 'reclaim', {})).body, []);
});

test('an unknown action is 404, not a silent 200', async () => {
  assert.equal((await handleDispatch(provider, 'nope', {})).status, 404);
});

// ---------------------------------------------------------------------------
// Manifest validation — the silent-failure spellings
// ---------------------------------------------------------------------------

test('a misspelled runtime.mode is reported, not defaulted quietly', () => {
  const problems = validateManifest({ id: 'x', runtime: { kind: 'server', start: 'run', mode: 'backgroud' } });
  assert.equal(problems.length, 1);
  assert.match(problems[0], /session/);
});

test('a good manifest has no problems', () => {
  assert.deepEqual(
    validateManifest({ id: 'x', runtime: { kind: 'server', start: 'run', mode: 'background' } }),
    [],
  );
});

test('network "hosts" with an empty list is caught', () => {
  const problems = validateManifest({ id: 'x', sandbox: { network: 'hosts', hosts: [] } });
  assert.equal(problems.length, 1);
  assert.match(problems[0], /no network/);
});

test('a bridge error envelope throws despite HTTP 200', async () => {
  // The daemon answers a failed action with HTTP 200 and status:"error".
  // Reading only the HTTP code turns a dead provider into an empty string,
  // which downstream reads as "the model had nothing to say".
  const d = await fakeDaemon({ status: 'error', message: 'LLM HTTP 404 Not Found' });
  try {
    await assert.rejects(() => d.client().llm({ prompt: 'q' }), /404/);
  } finally { await d.close(); }
});

test('a pending bridge names the real problem', async () => {
  const d = await fakeDaemon({ status: 'pending' });
  try {
    await assert.rejects(() => d.client().knowledgeRecall('q'), /not enabled/);
  } finally { await d.close(); }
});

test('a payload with no status field is not mistaken for a failure', async () => {
  // Not every action answers with an envelope; those must not be treated as
  // failures just because `status` is absent.
  const d = await fakeDaemon({ hits: [] });
  try {
    assert.deepEqual(await d.client().knowledgeSearch('q'), []);
  } finally { await d.close(); }
});

test('forDaemon resolves appId and baseUrl from the env the daemon injects', async () => {
  // Hardcoding 18788 works right up until someone runs the daemon elsewhere.
  const d = await fakeDaemon({ status: 'ok', text: 'hi' });
  const port = new URL(d.client().env.coreBase).port;
  const saved = { ...process.env };
  try {
    process.env.SENCLAW_SPACE_APP_ID = 'from-env';
    process.env.SENCLAW_BASE_URL = `http://127.0.0.1:${port}`;
    const c = SenclawSpace.forDaemon();
    await c.llm({ prompt: 'q' });
    assert.match(d.seen[0].path, /\/api\/space\/apps\/from-env\/bridge/);
  } finally {
    process.env = saved;
    await d.close();
  }
});

test('forDaemon says what is missing rather than building a broken client', () => {
  const saved = process.env.SENCLAW_SPACE_APP_ID;
  delete process.env.SENCLAW_SPACE_APP_ID;
  try {
    assert.throws(() => SenclawSpace.forDaemon(), /SENCLAW_SPACE_APP_ID/);
  } finally {
    if (saved !== undefined) process.env.SENCLAW_SPACE_APP_ID = saved;
  }
});

// ---------------------------------------------------------------------------
// The app's identity on the wire
// ---------------------------------------------------------------------------
//
// The access token is what tells the daemon *which* app is calling. It travels
// in a header, so it is invisible to the body-recording stub above — these
// tests keep the headers.

import { API_VERSION, HEADER_API_VERSION, HEADER_APP_TOKEN } from '../dist/index.js';
import { requireAppToken } from '../dist/mcp.js';

const TOKEN = 'sca_' + 'a'.repeat(64);

async function headerRecordingDaemon() {
  const seen = [];
  const server = http.createServer((req, res) => {
    seen.push(req.headers);
    req.resume();
    req.on('end', () => {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end('{"items":[]}');
    });
  });
  await new Promise(r => server.listen(0, '127.0.0.1', r));
  const port = server.address().port;
  return { seen, port, close: () => new Promise(r => server.close(r)) };
}

test('every daemon call carries the access token and the contract version', async () => {
  const d = await headerRecordingDaemon();
  try {
    const space = new SenclawSpace({
      appId: 't',
      apiBase: `http://127.0.0.1:${d.port}/api/space/apps`,
      appToken: TOKEN,
    });
    await space.listConfig();
    assert.equal(d.seen[0][HEADER_APP_TOKEN.toLowerCase()], TOKEN);
    assert.equal(d.seen[0][HEADER_API_VERSION.toLowerCase()], String(API_VERSION));
  } finally {
    await d.close();
  }
});

test('no token means no header, not an empty one', async () => {
  // Running the app by hand, or in the browser. Sending the header blank would
  // make the daemon try to resolve '' and refuse a call its default mode serves.
  const d = await headerRecordingDaemon();
  try {
    const space = new SenclawSpace({
      appId: 't',
      apiBase: `http://127.0.0.1:${d.port}/api/space/apps`,
      appToken: '',
    });
    await space.listConfig();
    assert.equal(d.seen[0][HEADER_APP_TOKEN.toLowerCase()], undefined);
  } finally {
    await d.close();
  }
});

test('the inbound guard refuses everyone but the daemon', async () => {
  const guard = requireAppToken({ token: TOKEN, skip: ['/health', '/public/*'] });
  const server = http.createServer((req, res) => {
    // A minimal express-shaped request: the guard reads `path`, `headers`, `query`.
    const url = new URL(req.url, 'http://x');
    const shim = { path: url.pathname, url: req.url, headers: req.headers, query: {} };
    const resShim = {
      status(code) { res.statusCode = code; return this; },
      json(payload) { res.end(JSON.stringify(payload)); },
    };
    guard(shim, resShim, () => { res.statusCode = 200; res.end('ok'); });
  });
  await new Promise(r => server.listen(0, '127.0.0.1', r));
  const port = server.address().port;
  const call = (path, token) => fetch(`http://127.0.0.1:${port}${path}`, {
    headers: token ? { [HEADER_APP_TOKEN]: token } : {},
  }).then(r => r.status);
  try {
    assert.equal(await call('/api/notes', TOKEN), 200, "the daemon's own request must pass");
    // What the guard exists to stop: another local process on the port.
    assert.equal(await call('/api/notes', null), 401);
    assert.equal(await call('/api/notes', 'sca_' + 'f'.repeat(64)), 401);
    // The health check runs before anything is proxied; locking it out would
    // make the app look permanently dead to the daemon.
    assert.equal(await call('/health', null), 200);
    assert.equal(await call('/public/logo.png', null), 200);
  } finally {
    await new Promise(r => server.close(r));
  }
});

test('the guard is inert when no token was issued', async () => {
  // A bare `npm start` outside SenClaw. Refusing everything would turn "not
  // launched by SenClaw" into "app is down".
  const guard = requireAppToken({ token: '' });
  let passed = false;
  guard({ path: '/api/x', url: '/api/x', headers: {}, query: {} }, null, () => { passed = true; });
  assert.equal(passed, true);
});
