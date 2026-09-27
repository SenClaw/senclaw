# Search Space App — federated search, evidence aggregation & research reports

**Status:** P0 shipped & verified live · **App:** `apps/search` · **Port:** 4530 · **MCP:** `search-mcp`
**Date:** 2026-07-20

> **P0 status (2026-07-20).** Built and verified against the running daemon: 5 transports,
> sources `web` + `knowledge` + `wiki`, dedupe + RRF + fair-share selection, 6 MCP tools, REST,
> React UI, 43 unit tests green. A live `SenClaw` query fanned out to all three sources
> (20 raw → 8 fused) and `search_query` over JSON-RPC returned real SERP results.
> Two things changed from the original design during implementation — see §5.3 (fair-share
> selection, added after a live failure) and §4.2 (knowledge goes through REST, not the bridge).

---

## 1. Goal

One app that answers a question by **searching everywhere SenClaw can reach at once**, then
aggregates enough independent evidence that the answer can be trusted — and hands that capability
to every other component through MCP.

Three requirements, from the request:

1. **Aggregate broadly enough that the result is actually correct** — web + social + internal
   knowledge + wiki + documents + any other MCP, fused and cross-checked, with a confidence score
   and explicit contradictions.
2. **Produce search reports** — a cited, versioned Markdown/JSON report per run.
3. **`search-mcp` serves other components** — other agents, apps and skills call `search_query` /
   `search_ask` / `search_deep` instead of each re-implementing retrieval.

Non-goal: replacing `senclaw-browser` or `social-mcp`. Search is a **consumer and fuser** of those,
never a re-implementation.

---

## 2. The transport problem (and why this design looks the way it does)

A Space App cannot call arbitrary MCP tools. `mcp.call` is advertised in the bridge capability list
(`src/gateway/ui_server/space.rs:1412`) but is a hard-coded stub:

```rust
// src/gateway/ui_server/space.rs:1704
"mcp.call" => Ok(Json(json!({ "status": "pending",
    "message": "mcp.call bridge action is not enabled yet." }))),
```

The documented workaround — `agent.run` with a `tools` allowlist — puts an **LLM in the retrieval
loop**: slow, non-deterministic, and it burns tokens on what is mechanically a fan-out. Unacceptable
as the primary path for an app whose entire job is fan-out.

Two verified openings make a deterministic path possible:

**(a) Every Space App's MCP is a plain unauthenticated JSON-RPC endpoint.**
`apps/social/src/mcp.rs:50` — `mcp_message(State, Json<JsonRpcRequest>) -> Json<Value>` handles
`initialize` / `tools/list` / `tools/call`. Nothing stops an app from being the *client*:

```
POST http://127.0.0.1:4520/api/mcp/message
{"jsonrpc":"2.0","id":1,"method":"tools/call",
 "params":{"name":"social_search","arguments":{"platform":"threads","handle":"…","query":"…"}}}
→ {"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"<pretty JSON>"}]}}
```

No app in the repo does this today. It is the generic app→app MCP call the bridge was supposed to
provide, and it works for **every** Space App (social, youtube, deepwiki, crm, ontology, kanban…).

**(b) The browser bridge WS is open and untagged.**
`src/gateway/websocket_gateway/gateway.rs:213` registers `/browser-mcp`; `senclaw-browser`'s own MCP
server is just a client of it (`src/mcp/browser_server.rs:521-545`) — one fresh WS per request,
send a `DaemonMessage`, read back `ExtensionMessage::Response { result }`. `DaemonMessage` is
`#[serde(tag = "type")]`, so the frame is trivially reproducible:

```json
{"type":"Search","request_id":"<uuid>","query":"…","engine":"google","num_results":10,"ephemeral":true}
```

So `apps/search` speaks the same protocol directly and gets `SearchResults { results:
[{position,title,url,snippet}], total_estimated, search_url }` with **no agent and no LLM**.

### 2.1 The five transports

| Transport | Reaches | Mechanism | Deterministic |
|---|---|---|---|
| `browser_ws` | `senclaw-browser` | WS `ws://127.0.0.1:{SENCLAW_WS_PORT}/browser-mcp`, `DaemonMessage::{Search,Navigate,ExtractText,ExtractLinks,CrawlStart}` | ✅ |
| `app_mcp` | **any** Space App MCP | `POST {app_origin}/api/mcp/message` JSON-RPC `tools/call`; discovery via `GET /api/space/apps` | ✅ |
| `core_rest` | daemon REST | `GET /api/wiki/search`, `POST /api/cognitive/search`, `POST /api/cognitive/recall` | ✅ |
| `bridge` | daemon bridge | `POST /api/space/apps/search/bridge` → `knowledge.search`, `knowledge.recall`, `llm.request` | ✅ |
| `agent_run` | anything else | bridge `agent.run` with `tools:["mcp__senclaw-memory__memory_search"]` | ❌ LLM-mediated, **fallback only** |

Rule: a source uses `agent_run` **only** when no deterministic path exists. Today that is exactly
one source (`memory`, §4.4). Every `agent_run` use is logged in `run_sources` so the cost is visible.

> **Upstream fix worth doing later:** implement `mcp.call` in core and collapse `app_mcp` +
> `agent_run` into it. This design does not block on that.

---

## 3. Core model

```rust
/// One retrievable surface. Sources are uniform regardless of transport.
#[async_trait]
pub trait SearchSource: Send + Sync {
    fn id(&self) -> &str;                     // "web", "wiki", "social:threads", "mcp:acme"
    fn kind(&self) -> SourceKind;             // Web|Internal|Social|Docs|Code|Custom
    fn weight(&self) -> f32;                  // prior trust, used in fusion
    async fn health(&self) -> SourceHealth;   // Ready | Degraded(reason) | Unavailable(reason)
    async fn search(&self, q: &SubQuery, budget: Budget) -> Result<Vec<Evidence>>;
}
```

```rust
/// The unit everything downstream operates on. One retrieved item, with provenance.
pub struct Evidence {
    pub id: String,                 // blake3(canonical_url | source_id+title+snippet)
    pub source_id: String,
    pub kind: SourceKind,
    pub title: String,
    pub url: Option<String>,        // citation target
    pub canonical_url: Option<String>,
    pub snippet: String,            // as retrieved
    pub full_text: Option<String>,  // populated by the deepen stage
    pub author: Option<String>,
    pub published_at: Option<i64>,
    pub retrieved_at: i64,
    pub rank: u32,                  // rank *within its own source*
    pub raw_score: f32,             // source-native score (BM25 / cosine / SERP pos) — NOT comparable across sources
    pub lang: Option<String>,
    pub meta: Value,
}
```

`raw_score` is deliberately never compared across sources — see fusion (§5.3).

---

## 4. Sources shipped in v1

### 4.1 `web` — public web (transport `browser_ws`)
`DaemonMessage::Search` → SERP items. `engine` defaults to `google`, anything else routes to Bing
(`senclaw-extension-chrome/src/agent/SearchEngine.ts:51`). Deepening uses `Navigate` +
`ExtractText` per top-N URL.

**Honest limits, surfaced in `search_sources`, not hidden:**
- Requires the SenClaw Chrome extension connected. Not connected → `Unavailable`, and the run
  reports the web source as unavailable rather than returning an empty web set.
- Google CAPTCHA / "unusual traffic" is a hard error (`SearchEngine.ts:25-35`). Policy: on CAPTCHA,
  fail over to Bing once, then mark `Degraded` and back off for the rest of the run.
- `country` and `safe_search` exist on `browser_search`'s params but are dropped before the wire
  (`browser_server.rs:1262`); we do not pretend to honor them.

### 4.2 `knowledge` — cognitive graph (transport `core_rest`)
`POST /api/cognitive/search` with `mode:"hybrid"` (load-bearing — see
`apps/crm/src/senclaw.rs:84`).

**Changed during implementation.** The design said `bridge`; the bridge is wrong here. Its
`knowledge.search` action defaults `space` to the *calling app's id* (`space.rs:1612`), which would
silently confine every search to the `search` space. The REST handler treats `space: None` as
**global** — it only sets `node_sets` when a space is supplied (`cognitive.rs:727`) — which is what
a federated search actually needs.

### 4.3 `wiki` — git knowledge base (transport `core_rest`)
`GET /api/wiki/search?q=&tags=&limit=`. Note the wiki's FTS builds an **AND**-joined prefix match
(`src/wiki/search.rs:130`) while memory/cognitive OR-join — so the wiki source gets the *narrow*
sub-query variant, not the expanded one, or it silently returns nothing.

### 4.4 `memory` — file memory (transport `agent_run`, the one fallback)
There is no `/api/memory/*` REST surface; `memory_search` is MCP-only
(`src/mcp/memory_server.rs:115`). Until core exposes REST, this source runs
`agent.run { tools: ["mcp__senclaw-memory__memory_search"] }` with a strict extract-only prompt.
Disabled by default in `deep` runs' hot path; enabled for `search_ask`.

### 4.5 `social` — FB / X / Threads / IG / TikTok (transport `app_mcp` → `social-mcp`)
`social_search { platform, handle, query }` per configured account, plus `social_feed` for
brand/keyword monitoring.

Capability reality from `apps/social/src/channels/mod.rs:70-105`, mirrored into our source registry:

| Platform | Search path | Status |
|---|---|---|
| Threads | Official `keyword_search` | working |
| YouTube | Official `search.list` | working |
| Facebook / X / Instagram | extension replay | **`not_wired`** — `background.js:115` returns `{not_wired:true}` because `social_search` exposes no `url` param |
| TikTok | PageSign | needs the page signer, not shipped |

We register all six but mark the last four `Degraded(not_wired)` so a run never silently reports
"no Facebook results" when the truth is "Facebook search was never wired".

### 4.6 `youtube` (transport `app_mcp` → `youtube-mcp`) — `youtube_search`. Superseded by `social`
upstream; kept as a distinct source because it is independently wired via InnerTube.

### 4.7 `deepwiki` — code & repo docs (transport `app_mcp` → `deepwiki-mcp`)
`deepwiki_explore` + `deepwiki_snippet` for "where is this implemented / how does this work".

### 4.8 `corpus` — the app's own documents (in-process)
Upload PDF / DOCX / MD / TXT / HTML → extract → chunk → app-local SQLite FTS5 with
`tokenize='unicode61 remove_diacritics 2'` (Vietnamese-correct; the same setting the wiki uses and
memory does not). Optionally mirrored into cognitive via bridge `knowledge.save` so the corpus also
enriches the graph.

**As built (P1).** `src/corpus.rs` (extract → chunk → FTS expression) + `sources/corpus.rs`.
Extraction reuses the crates `apps/ontology/src/ingest.rs` already uses (`pdf-extract`,
`quick-xml`+`zip`), so behaviour matches the one place in the repo that had already solved this.
Four rules, each guarding a silent failure:

* **A scan is an error, not an empty document.** A PDF with no text layer is refused by name
  ("hãy OCR trước"). Storing it would list a document in the UI that answers every future query
  with nothing — indistinguishable from "the answer isn't in your documents".
* **The user's query is not an FTS5 expression.** `giá "vàng" - SJC`, `a AND b`, `foo*`,
  `(unbalanced` are all syntax errors if interpolated. `corpus::fts_query` quotes every token
  (doubling embedded `"`), which neutralises punctuation *and* the `AND`/`OR`/`NOT`/`NEAR`
  keywords, then OR-joins for recall — deliberately unlike the wiki's AND-join.
* **Chunks are paragraph-aligned with overlap**, and an oversized single paragraph is still split,
  so one wall of text cannot become one unsearchable chunk. Sizes are counted in *characters*, not
  bytes — Vietnamese is multibyte.
* **`DELETE` must reach `corpus_fts`.** It is a contentless FTS5 table with no foreign key, so
  `ON DELETE CASCADE` does not touch it; deleting only the doc row would leave orphan rows that
  keep matching and cite a document that no longer exists. Verified by test.

Re-uploading identical bytes is detected by SHA-256 and refused, because duplicated chunks would
read as independent corroboration in §5.4. Verified live: an unaccented query (`lai suat dieu hanh`)
retrieves an accented Vietnamese document.

### 4.9 `mcp:*` — user-registered generic MCP sources
The extensibility answer to *"các nguồn yêu cầu từ MCP khác"*. A row in `mcp_sources` turns **any**
MCP tool into a search source with zero code:

```jsonc
{
  "id": "mcp:ontology",
  "target": { "kind": "app", "app_id": "ontology" },     // or {"kind":"http","url":"…/mcp/message"}
  "tool": "ontology_sparql",
  "query_arg": "query",                                   // where the query text goes
  "extra_args": { "graph": "default", "limit": 20 },
  "map": {                                                // JSONPath-ish → Evidence
    "items": "$.results.bindings",
    "title": "$.label.value",
    "url":   "$.uri.value",
    "snippet": "$.comment.value"
  },
  "kind": "Custom", "weight": 0.7, "timeout_ms": 8000
}
```

Result unwrapping is uniform: MCP `content[0].text` → parse as JSON if possible, else treat as
plain text → apply `map`. Registered via `search_source_add` or the UI.

**As built (P1).** `sources/mcp_source.rs`. Three things the sketch above got wrong or missed, each
found by pointing it at a real running app:

1. **`map` is optional, and usually empty.** The mapper auto-detects the item array (bare array →
   known keys → first array value) and the field names. Verified against `crm_search`
   (`{count, hits:[…], q}`) and `deepwiki_search` (a bare array) with no `map` at all. Field paths
   are dotted (`a.b`), not JSONPath — there is no JSONPath dependency.
2. **`url_template` is required for some sources, not a nicety.** `youtube_search` returns a
   `videoId` and *no URL whatsoever*; without a template its results can never be cited and never
   dedupe against a web hit for the same video. If a placeholder can't be resolved the URL is
   dropped — emitting a literal `…watch?v={videoId}` as a citation is worse than no URL.
3. **The query argument is usually not called `query`.** `crm_search` takes `q` and answers
   `"q is required"` otherwise. The UI reads the tool's own `inputSchema` and matches the real
   parameter names; `limit_arg` is omitted entirely when the tool declares no such parameter,
   because sending an undeclared argument is an error, not a no-op.

Two guards exist because both failure modes are silent:

* **Reserved ids.** A user source may not be named `web`/`knowledge`/`wiki`/`memory`/`corpus` — it
  would shadow the built-in and appear to work while doing something else.
* **Self-targeting.** Registering the search app's own MCP is refused. `search_query` as a source of
  itself fans out into itself and recurses until every timeout fires. The check matches on port plus
  a loopback host, since `127.0.0.1` / `localhost` / `0.0.0.0` / `::1` all name the same server.

**Presets vs. templates.** `sources/presets.rs` auto-registers a peer app *only* when the app is
installed, enabled, and its search tool needs nothing but a query — currently `youtube` and
`deepwiki`. `social_search` requires `platform` **and** `handle` (the specific logged-in account
whose session the extension replays), so it is offered as a *template* the user completes, never
auto-registered: no default can be guessed, and guessing wrong searches under someone else's
identity. Registration reports every skipped preset and why, the same way the pipeline reports every
failed source.

---

## 5. Pipeline

```
query
  │
  ├─▶ 1 PLAN        llm.request → {sub_queries[], sources[], lang, freshness, depth}
  │                 (deterministic fallback: raw query → all enabled sources)
  ├─▶ 2 FAN-OUT     source × sub_query, bounded semaphore, per-source timeout
  ├─▶ 3 NORMALIZE   → Evidence
  ├─▶ 4 DEDUPE      canonical URL + near-duplicate text
  ├─▶ 5 FUSE        Reciprocal Rank Fusion + independence bonus
  ├─▶ 6 DEEPEN      full-text fetch for top-N web results (depth ≥ 2)
  ├─▶ 7 CLAIMS      llm.request → atomic claims, each bound to evidence ids
  ├─▶ 8 CORROBORATE independent-source counting → confidence tier + contradictions
  ├─▶ 9 VERIFY      adversarial refuters (level 2 only)
  └─▶10 SYNTHESIZE  cited Markdown report + JSON
```

Stages 7–10 run only for `search_deep` / `search_ask`. `search_query` stops after stage 5 — that is
the cheap, fast, LLM-free tool other components call.

### 5.1 Fan-out & degradation
`tokio::spawn` per (source, sub_query) behind a semaphore (default 8). Each task has its own
timeout and error boundary. **A failing source degrades the run, never fails it** — the outcome is
recorded in `run_sources { source_id, sub_query, status, item_count, ms, error }` and rendered in
both the UI and the report's provenance appendix. No silent truncation: if a cap dropped results,
the cap and the drop count are recorded.

### 5.2 Dedupe
- **URL**: lowercase host, strip `www.`, strip fragment, strip `utm_*`/`fbclid`/`gclid`/`ref`,
  collapse trailing slash → `canonical_url`. Same canonical URL = same evidence, merged with the
  union of its `source_id`s (this is what makes independence counting work).
- **Text**: 3-gram SimHash on the snippet; Hamming distance ≤ 3 = near-duplicate, keep the
  longest-text copy, merge provenance.

### 5.3 Fusion — Reciprocal Rank Fusion
Source scores are incomparable (BM25 vs cosine vs SERP position vs graph activation), so we fuse on
**rank**, not score:

```
rrf(e) = Σ_{s ∈ sources(e)}  w_s / (k + rank_{s}(e))          k = 60
score(e) = rrf(e) · (1 + β · (independent_kinds(e) - 1))       β = 0.25
```

`independent_kinds` counts distinct `SourceKind`s, not distinct sources — three social platforms
echoing one press release is one kind, not three. This is the first place "aggregate enough to be
correct" is mechanically enforced.

**Fair-share selection (added in P0, after a live failure).** Weighted RRF has a failure mode that
only appears on real data: when sources carry systematically different weights, `w_s / (K + rank)`
orders by *source weight first, rank second*. The first live run made this obvious — wiki (w=1.3)
took all 8 slots and the web source's 7 real results contributed **nothing**. A federated search
that returns one source has not aggregated anything.

So truncation is not `take(limit)`. `fusion::select_diverse` caps each source at
`ceil(limit / sources_present)`, walking in fused order and *deferring* rather than dropping the
overflow, then refills any unused slots from the deferred pool. A source with few hits cannot shrink
the list, and a lone source still gets the whole list. Re-verified live: 3 wiki / 3 knowledge /
2 web.

Known limit, not yet addressed: the cap bounds each source's *share* but does not interleave the
*head*. When the result count is at or under `limit` nothing is truncated, so the highest-weighted
source can still occupy the first several rows. Fixing that means round-robin ordering across
sources, which trades away cross-source score comparison — a ranking-semantics decision, deliberately
left open rather than changed silently.

### 5.4 Claims & corroboration (verification level 1)
An `llm.request` extracts atomic, checkable claims from the fused top-K, each carrying the
`evidence_id`s it came from. Then, purely mechanically:

```
independent  = distinct (source_kind, registrable_domain) pairs supporting the claim
agreement    = supporting / (supporting + refuting)
```

| Tier | Rule |
|---|---|
| `verified` | independent ≥ 3 ∧ agreement ≥ 0.8 ∧ not refuted by adversarial pass |
| `supported` | independent ≥ 2 ∧ agreement ≥ 0.7 |
| `single-source` | independent = 1 |
| `disputed` | agreement < 0.7 — sources genuinely conflict |
| `unverified` | no evidence binding survived |

**Contradictions are first-class.** When two claims conflict they are stored in `contradictions`
with both sides and their evidence, rendered in their own report section, and never resolved by
silently picking one. Naming a disagreement is more useful than hiding it.

**As built (P2).** `src/claims.rs` (deterministic) + `src/extract.rs` (the one LLM call).
The split is the point: a tier decided by a model is a model's opinion; a tier decided by counting
independent sources is a fact about what was retrieved. Four decisions worth recording:

1. **A claim may only cite evidence that exists in the run.** Models invent ids. Invented ones are
   stripped and reported in `dropped_citations` — surfaced in the UI in red — so a claim can never
   look better-sourced than it is. A claim citing *only* invented evidence becomes `unverified`,
   never `supported`.
2. **Evidence is presented to the model as `[E1]…[En]`, not as raw ids.** Asking a model to copy
   `ev_18c40e2ab2215be80` back verbatim invites transcription errors; a small integer is hard to get
   wrong and an out-of-range one is caught deterministically. Index `0` is treated as out-of-range
   rather than silently mapped to the first item.
3. **Agreement is counted in independent units, not raw evidence rows** — a deviation from the
   sketch above. One chatty source emitting five refuting snippets must not outvote two genuinely
   separate publishers; the test pins this at an honest 2-vs-1 instead of a misleading 2-vs-5.
4. **A contradicted claim cannot stay `verified`/`supported`.** `mark_disputed` demotes both sides
   after validation, so the UI can never show a settled-looking chip on a claim the sources disagree
   about.

Truncated model output is repaired rather than discarded (the bridge has a real output ceiling):
the bracket **stack** is closed in reverse order, and a cut landing mid-key falls back to the last
complete element. Closing only `}` — as the first implementation did — leaves `[` open and every
truncated response still fails to parse.

Degradation is split into two distinct signals, because they mean different things: `claims_error`
(evidence was found, the analysis on top of it failed — the evidence is still usable) versus
`claims_note` (there was no evidence to analyse; go read `sources`). Both verified live with the
bridge down.

`confidence` is a **provenance score, not a truth score** — the report says so in as many words.
Three sources copying one wrong wire story yields high provenance and a wrong fact; the independence
rule (domain + kind) mitigates but does not eliminate this.

### 5.5 Adversarial verification (level 2)
For claims that are `verified`/`supported` **and** flagged high-stakes (numeric, legal, medical,
financial, or a named-entity attribution), spawn N=3 refuters. Each gets a distinct lens —
*source-quality*, *contradicting-evidence*, *logical/temporal consistency* — and is prompted to
**refute**, defaulting to refuted when uncertain. Cheap refuters use `llm.request` over the evidence
already retrieved; the contradicting-evidence lens uses `agent.run` so it can search for the
counter-case. Majority refute → demote to `disputed` with the refutation attached.

Level is per-run (`verify: "cited" | "corroborate" | "adversarial"`), default **`corroborate`**,
with `adversarial` available and used automatically for high-stakes claims when the run's budget
allows. All three levels ship, as requested.

### 5.6 Synthesis
Markdown with inline `[^n]` footnotes bound to `evidence` rows. A post-check flags any assertive
sentence carrying no citation; flagged sentences are either cited or cut before the report is
stored. Report sections: **Answer · Key findings (with confidence chips) · Contradictions ·
Timeline (if temporal) · Sources · Provenance appendix** (which sources ran, which failed, caps hit,
token/time cost).

---

## 6. Schema (app-local SQLite, `search.db`)

```sql
runs(id, query, params_json, status, depth, verify_level, started_at, finished_at,
     evidence_count, claim_count, token_cost, error)
run_sources(run_id, source_id, sub_query, status, item_count, dropped_count, ms, error)
evidence(id, run_id, source_id, kind, title, url, canonical_url, snippet, full_text,
         author, published_at, retrieved_at, rank, raw_score, fused_score, lang, meta_json)
claims(id, run_id, text, tier, confidence, independent_count, agreement, high_stakes, verdict_json)
claim_evidence(claim_id, evidence_id, stance)          -- supports | refutes
contradictions(id, run_id, claim_a, claim_b, summary)
reports(id, run_id, version, format, title, body_md, body_json, created_at)
corpus_docs(id, name, mime, bytes, sha256, uploaded_at, status)
corpus_chunks(id, doc_id, ord, text, page)
corpus_fts USING fts5(chunk_id UNINDEXED, text, tokenize='unicode61 remove_diacritics 2')
mcp_sources(id, target_json, tool, query_arg, extra_args_json, map_json, kind, weight, timeout_ms, enabled)
source_config(source_id, enabled, weight, timeout_ms, max_results, options_json)
monitors(id, query, params_json, cron, last_run_id, notify_json, enabled)
```

---

## 7. `search-mcp` — the surface other components use

| Tool | Args | Returns |
|---|---|---|
| `search_query` | `query`, `sources?`, `limit?`, `lang?`, `freshness?` | ranked evidence — **no LLM, fast**; the workhorse for other agents |
| `search_ask` | `query`, `sources?`, `verify?` | grounded answer + citations + confidence |
| `search_deep` | `query`, `depth?`, `verify?`, `sources?`, `budget?` | `{run_id}` (async) |
| `search_run_status` | `run_id` | stage, per-source progress, counts |
| `search_report` | `run_id \| report_id`, `format?` | report Markdown / JSON |
| `search_report_list` | `limit?` | recent reports |
| `search_evidence` | `run_id`, `filter?` | evidence rows (re-read without re-searching) |
| `search_claims` | `run_id` | claims + tiers + contradictions |
| `search_verify` | `claim`, `sources?`, `level?` | standalone fact-check of a supplied claim |
| `search_sources` | — | source list + health + capability caveats |
| `search_source_config` | `source_id`, `enabled?`, `weight?`, … | tune a source |
| `search_register_mcp_source` | see §4.9 | register any MCP tool as a source |
| `search_delete_mcp_source` | `id` | — |
| `search_corpus_upload` | `name`, `content_base64 \| path` | doc id, chunk count |
| `search_corpus_list` / `search_corpus_delete` | — / `doc_id` | — |
| `search_monitor_create` / `_list` / `_delete` | `query`, `cron`, `notify` | recurring search + change detection |
| `search_status` | — | health, counts, extension connectivity |

18 tools. Naming follows the repo convention: server `search-mcp`, prefix `search_`.

**Skills:** `search-research` (deep research → report), `search-verify` (fact-check a claim),
`search-find` (fast lookup). **Personas:** `researcher`, `fact-checker`.

---

## 8. Web UI (React + Vite + AntD, served by the app's axum server)

- **Search** — query box, source toggles with live health dots, depth/verify selectors.
- **Run view** — live SSE progress: per-source status, counts, timings; failures visible in place.
- **Evidence** — sortable table: title, source, domain, date, fused score, which sources
  corroborate; click to read extracted full text.
- **Claims** — confidence chips (`verified`/`supported`/`single-source`/`disputed`), expandable to
  supporting *and* refuting evidence. Contradictions get their own panel.
- **Report** — rendered Markdown with working footnote links, version history, copy/export.
- **Sources** — enable/weight/timeout, register a generic MCP source, corpus upload.

Vite `base: "./"` (relative assets) so the same `dist` works both direct on :4530 and under the
daemon proxy `/api/space/apps/search/proxy/` — the pattern every other app uses.

---

## 9. Manifest

```jsonc
{
  "id": "search", "name": "Search", "icon": "🔎",
  "description": "Tìm kiếm tổng hợp: gom kết quả từ web, mạng xã hội, tri thức nội bộ, wiki, tài liệu và mọi MCP khác, đối chiếu chéo giữa các nguồn độc lập rồi tạo báo cáo có trích dẫn kèm điểm tin cậy.",
  "runtime": { "kind": "server", "start": "./search", "healthPath": "/api/status", "port": 4530 },
  "integration": { "type": "iframe", "url": "/" },
  "bridge": { "postMessage": true, "capabilities": ["space.rest", "llm.request", "agent.run",
                                                    "knowledge.search", "knowledge.recall"] },
  "mcp": { "name": "search-mcp", "transport": "http", "path": "/api/mcp/sse", "autoRegister": true }
}
```

Port 4530 — first free slot above `social` (4520). `scripts/pack.sh` + `release/` follow the
DeepWiki template exactly (binary + manifest + skills/ + personas/ + `web/dist` → `web_dist`).

---

## 10. Phasing

| Phase | Delivers | Done when |
|---|---|---|
| **P0 — spine** ✅ | app skeleton, DB, the 5 transports, sources `web` + `wiki` + `knowledge`, dedupe + RRF + fair-share selection, `search_query` / `search_sources` / `search_source_config` / `search_runs` / `search_run` / `search_status`, REST, React UI, 43 tests | **done** — verified live against the running daemon |
| **P1 — breadth** 🚧 | ✅ generic `McpSource` + peer presets + `mcp_sources` registration + `corpus` upload/extract/chunk/FTS + source & corpus UI · 14 MCP tools, 94 tests · ⬜ `memory` via `agent_run`, preset auto-discovery unverified (daemon was down) | user-registered MCP sources **verified live** against two running peer apps (crm, deepwiki) with zero code; corpus verified with an unaccented Vietnamese query |
| **P2 — correctness** 🚧 | ✅ claims, corroboration, confidence tiers, contradictions, `search_ask` + `search_claims`, claims UI · ⬜ `search_verify` (adversarial, §5.5) · claim extraction unverified end-to-end (daemon down) | conflicting sources produce a `disputed` claim instead of a confident wrong answer — the corroboration maths is fully tested; only the LLM call awaits a live daemon |
| **P3 — reports** | adversarial verify, synthesis + citation post-check, `search_deep` / `search_report`, report UI + export | a deep run yields a cited report with a provenance appendix |
| **P4 — ops** | monitors + scheduler, skills, personas, `pack.sh`, install zip | installable app; a scheduled monitor reports what changed |

---

## 11. Known risks — stated, not papered over

1. **Web search depends on the Chrome extension.** No extension → no web source. Surfaced as
   `Unavailable`, never as an empty result set.
2. **SERP scraping is fragile.** Google CAPTCHA, DOM drift, rate limits. Bing failover + backoff
   help; an API-key SERP provider (Brave/Serper/Tavily) is the real fix and slots in as one more
   `SearchSource` with no pipeline change. None exists in core today (verified: zero hits for
   brave/serper/tavily/searxng across `src/`).
3. **Corroboration ≠ truth.** Independent-domain counting reduces but does not remove echo-chamber
   risk. The report states this; confidence is labelled *provenance*.
4. **Social search is mostly not wired upstream.** FB/X/IG replay search returns `not_wired`; TikTok
   needs the page signer. We declare it rather than shipping a source that always returns empty.
5. **Claim extraction is an LLM step** and inherits the bridge's constraints: no `temperature`, and
   a fixed-ish output ceiling — chunk the evidence set and cap `maxTokens` accordingly
   (see [[space-app-llm-bridge-output-ceiling]], [[space-app-llm-bridge-no-temperature]]).
6. **`agent_run` cost.** One source uses it. If core ever exposes `/api/memory/search` or implements
   `mcp.call`, that source becomes deterministic and the fallback transport can be deleted.

---

*Cross-refs:* [[social-app-extension-design]] · [[codegraph-deepwiki-apps]] ·
[[knowledge-cognitive-v2]] · [[knowledge-multi-space]] · [[senclaw-mcp-naming]] ·
[[browser-multiagent-concurrency]] · `docs/mcp-dispatcher-design.md` · `docs/social-unified-design.md`
