// Purging a project with a LARGE bank, on an isolated daemon: how long does the purge take, and how long is every
// other request kept waiting while it runs?
//   P0_RUNS=<dir> P0_UI=49788 P0_WS=49789 P0_MOCK=49931 node scale-purge.mjs <runLabel> [facts=20000]
// Needs ./env-up.sh <runLabel> <daemon binary> first. The facts are bulk-inserted straight into the daemon's database
// (entities, links, text index included) — they are synthetic, so this measures deletion only.
//
// Reference: P0 measured one `DELETE FROM mem_facts WHERE bank=…` over 20 000 facts at 17.8 s; the database is one
// connection behind a mutex, so that is 17.8 s in which nothing else can query it. `Db` deletes in slices of 500.
import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { sleep, log, api, SP } from '../project-p0-spike/lib.mjs';

const label = process.argv[2];
const N = Number(process.argv[3] ?? 20000);
const S = path.join(SP, `run-${label}`);
const DB = path.join(S, 'home', '.senclaw', 'senclaw.db');
const sql = (q, db = DB) => execFileSync('sqlite3', ['-cmd', '.timeout 20000', db, q], { encoding: 'utf8', maxBuffer: 1 << 26 }).trim();

let r = await api('/api/projects', {
  method: 'POST',
  headers: { 'content-type': 'application/json' },
  body: JSON.stringify({ name: 'Big', root: path.join(S, 'proj', 'C') }),
});
const id = r.body.id;
const bank = `project:${id}`;
const t0 = Date.now();
sql(`
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < ${N})
INSERT INTO mem_facts (id,bank,kind,text,mentioned_at,created_at,content_hash,source,origin)
SELECT 'bulk-'||i, '${bank}', 'world', 'bulk fact number '||i||' about module '||(i%97)||' of service '||(i%31), 1792152000, 1792152000, 'bulk-'||i, 'reflection', '' FROM n;
INSERT INTO mem_facts_fts (fact_id,text) SELECT id, lower(text) FROM mem_facts WHERE bank='${bank}';
INSERT INTO mem_fact_entities (fact_id,entity) SELECT id, 'module '||(abs(random())%97) FROM mem_facts WHERE bank='${bank}';
INSERT INTO mem_fact_entities (fact_id,entity) SELECT id, 'service '||(abs(random())%31) FROM mem_facts WHERE bank='${bank}';
INSERT OR IGNORE INTO mem_links (from_id,to_id,link_type,weight)
  SELECT a.id, b.id, 'semantic', 0.5 FROM mem_facts a JOIN mem_facts b ON b.id = 'bulk-'||(CAST(substr(a.id,6) AS INTEGER)+1) WHERE a.bank='${bank}';
`);
log(`inserted ${N} synthetic facts in ${Date.now() - t0} ms`);
const count = (b) => Number(sql(`select count(*) from mem_facts where bank='${b}'`));
const rows = (t) => Number(sql(`select count(*) from ${t}`));
const before = { facts: count(bank), entities: rows('mem_fact_entities'), links: rows('mem_links'), fts: rows('mem_facts_fts') };
log('before', JSON.stringify(before));

// Reference on this machine: the same deletion as ONE statement, on a copy of the database.
const copy = path.join(S, 'single-statement-copy.db');
fs.rmSync(copy, { force: true });
sql(`vacuum into '${copy}'`);
const tRef = Date.now();
sql(`PRAGMA foreign_keys=ON; DELETE FROM mem_facts WHERE bank='${bank}';`, copy);
const singleMs = Date.now() - tRef;
fs.rmSync(copy, { force: true });
log(`one DELETE statement over ${N} facts: ${singleMs} ms (everything else waits that long)`);

// Probe: a cheap read every 20 ms while the purge runs; the slowest answer is how long anybody was kept waiting.
let stop = false;
const lat = [];
const probe = (async () => {
  while (!stop) {
    const t = Date.now();
    await api('/api/projects');
    lat.push(Date.now() - t);
    await sleep(20);
  }
})();
await sleep(300);
const tp = Date.now();
r = await api(`/api/projects/${id}?purge=true`, { method: 'DELETE' });
const purgeMs = Date.now() - tp;
await sleep(300);
stop = true;
await probe;
lat.sort((a, b) => a - b);
const pct = (q) => lat[Math.min(lat.length - 1, Math.round((lat.length - 1) * q))];
const after = { facts: count(bank), entities: rows('mem_fact_entities'), links: rows('mem_links'), fts: rows('mem_facts_fts') };
const result = {
  facts: N,
  purgeStatus: r.status,
  deletedFacts: r.body.deletedFacts,
  purgeMs,
  singleStatementMs: singleMs,
  probe: { requests: lat.length, median: pct(0.5), p95: pct(0.95), max: lat[lat.length - 1] },
  before,
  after,
};
log(JSON.stringify(result));
fs.writeFileSync(path.join(path.dirname(new URL(import.meta.url).pathname), 'evidence', 'scale-purge.json'), JSON.stringify(result, null, 1));
const ok = r.status === 200 && r.body.deletedFacts === N && after.facts === 0 && after.entities === 0 && after.links === 0 && after.fts === 0;
log(ok ? 'PASS the bank, its entities, links and text index are gone' : 'FAIL');
process.exit(ok ? 0 : 1);
