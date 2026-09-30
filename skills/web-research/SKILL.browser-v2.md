---
name: web-research
description: Autonomous multi-source web research with a browse → summarize-each-page → synthesize-all (map-reduce) pipeline. Use when a question needs an evidence-backed answer drawn from several live web pages (current data, comparisons, surveys, fact checks, "what's happening with X this week"). Composes the `agent-browser` toolkit and adds research methodology — query planning, source diversity, per-page distillation, cross-page synthesis, and citations with URLs. Runs end-to-end without asking the user for permission to open or read pages.
version: 2.1.0
when-to-use: questions that need a researched answer rather than a single fact ("compare X vs Y", "what's the current state / trend of Z", "find studies on…", "is it true that…", "summarize today's news on…", "sự chênh lệch giá vàng trong tuần thế nào", multi-day market analyses, technical surveys, fact-checks). Prefer `agent-browser` directly for single-shot operations such as opening one page, taking a screenshot, or filling a form.
triggers:
  # Research intent words that signal a MULTI-SOURCE, browse-many-pages answer.
  # Deliberately NOT the single-fact web words (giá / price / hôm nay / screenshot /
  # url / login) — those route to `agent-browser`. Keep that split intact.
  # --- Multi-source research intent ---
  - nghiên cứu
  - nghiên cứu sâu
  - tìm hiểu
  - khảo sát
  - research
  - deep research
  - survey
  # --- Comparison (multiple things weighed against each other) ---
  - so sánh
  - so với
  - đối chiếu
  - compare
  - comparison
  # --- Synthesis / cross-source analysis ---
  - tổng hợp
  - tổng hợp thông tin
  - phân tích
  - tổng quan
  - synthesize
  # --- Trend / state / movement over a period ---
  - xu hướng
  - tình hình
  - diễn biến
  - biến động
  - chênh lệch
  - trend
  # --- Fact-check / verification ---
  - kiểm chứng
  - xác minh
  - thực hư
  - có đúng không
  - có thật không
  - sự thật
  - fact check
  - fact-check
  - debunk
  - is it true
  # --- Expert opinion / "what do people say" ---
  - chuyên gia nói gì
  - ý kiến chuyên gia
  - what do experts say
  - expert opinion
---

# Web Research Skill

A methodology layer on top of `agent-browser`. Goal: produce an answer the user can **defend** — built by actually reading multiple pages, distilling each one, then synthesizing across all of them. Every claim has a source URL; contradictions are surfaced, not smoothed over.

## Three non-negotiable principles

1. **Browse, don't skim.** Open and read **multiple real pages**. Search snippets are only used to *choose which pages to open* — never as the basis for the final answer. A synthesis built from snippets alone is a failure, even if it looks right.
2. **Map, then reduce.** Summarize **each page on its own** first (the *map* step — one compact summary per page), then do a **second pass** that synthesizes all those summaries into one coherent answer (the *reduce* step). Two passes, always.
3. **Run autonomously.** Never ask the user for permission to open, navigate, or extract pages — that *is* the job. Never pause mid-research to ask "should I continue / read these?". The whole pipeline Search → Read-each → Synthesize runs end-to-end in a single go. (See **Autonomy** below.)

## When to use this vs `agent-browser` directly

| Request shape | Pick |
|---|---|
| "Open X / click Y / screenshot Z" | `agent-browser` (no methodology needed) |
| "Giá vàng SJC hôm nay là bao nhiêu" (one number, right now) | `agent-browser` (one search, 1–2 pages) |
| "Sự chênh lệch giá vàng trong tuần thế nào" (trend/analysis) | **`web-research`** (browse + map + reduce) |
| "Compare A vs B" | **`web-research`** (parallel queries per side) |
| "What do experts say about X" | **`web-research`** (multi-source) |
| "Summarize today's news on X" | **`web-research`** (multi-source) |
| "Is claim X true?" | **`web-research`** (adversarial verify) |
| "Survey of approaches to X" | **`web-research`** (deep mode) |

Default rule: if the honest answer is one number with one source, use `agent-browser`. If the answer needs comparing, trending, explaining, or reconciling several sources, use `web-research` — and that means **opening several pages**, not reading one set of snippets.

## Step 0 — The tools (already in your tool list)

Two calls do the work, listed as `mcp__core__browser_search` and `mcp__core__browser_read` (`mcp__senclaw-browser__…` names the same tools). Call them directly: no `ToolSearch` first.

- `browser_search { "query": "…" }` → `results` (title, url) and the results page's text.
- `browser_read { "url": "…" }` → the page's text and links, opened and read in one call. Add `"question": "…"` and the page is answered for you, figures quoted from it.

**Every turn of yours costs far more than the call it carries.** Put all the searches of a round in one turn, and all the page reads of a round in one turn (several calls in the same message): they run one after another in this chat's browser tab and come back together.

## The pipeline (the heart of this skill)

```
Search (per facet)
   │   snippets are leads only
   ▼
Select the best 4–8 pages to open  ← snippets used HERE, and only here
   │
   ▼
For EACH page:  browser_read (one call) → write a per-page summary             ← the MAP step
   │
   ▼
Verify (corroborate / refute across the per-page summaries)
   │
   ▼
Synthesize ALL per-page summaries into one cited answer                          ← the REDUCE step
```

You always reach the bottom. There is no branch that stops at "snippets agree, done" — snippets never produce the final answer.

## Modes set HOW MANY pages, never WHETHER to open them

Every mode reads at least one full page. The mode only caps fan-out so you don't over- or under-research.

| Mode | When | Search queries | Pages **read & summarized** | Verification | Output |
|---|---|---|---|---|---|
| **Quick** | Genuine single fact that just needs a source (a price, a status, a definition) | 1 | **1–2** | corroborate the one number | 1 paragraph + URL |
| **Medium** *(default)* | Compare / explain / "what's the trend" / "what's happening with X" | 3–5 distinct angles | **4–8** | cross-check each key claim across ≥2 domains | TL;DR + body sections + cited sources |
| **Deep** | Survey / fact-check / multi-day analysis / high-stakes claim | 8–15 (fan-out) | **10–20** | adversarial refute of every key claim | Structured report: TL;DR + sections + source table + gaps section |

Default = **Medium**. Promote to Deep when the user asks for "deep research", or the question is high-stakes (a claim for the record, a financial decision, a published source). Demote to Quick only when the user genuinely wants one number — and even then you open the page, you don't snippet-quote it.

> The screenshot that motivated this skill version showed a "trend this week" question answered from a single search's snippets. That is a Medium question and must read 4–8 pages. Don't repeat it.

## Workflow

### Step 1 — Restate the question + plan queries

Before any search, write down (in the thought channel):

1. **Concrete question** (one sentence). Convert vague asks into a specific question.
2. **Key facets** to cover (2–5 bullets). Each facet → one or more queries.
3. **Source-diversity targets**: news / docs / forums / official / opposing views. Note which apply.
4. **Recency requirement**: "as of today" vs "general" — affects query terms (add dates, "2026", "latest", "hôm nay").

**Do not ask the user for permission or for access.** If the request is genuinely ambiguous about *what* is being researched (e.g. "research Apple" — company or fruit?), pick the most likely reading, state the assumption in one line, and proceed. Only stop to ask if you truly cannot choose — and never to ask whether you may open pages.

### Step 2 — Search broadly, then SELECT pages

Run 1 query per facet — all of them in one turn, as several calls in the same message.

```
mcp__senclaw-browser__browser_search { "query": "<facet-specific phrase>" }
```

**Query craft rules:**
- Add date/year for time-sensitive facts (`"latest 2026"`, `"hôm nay"`, `"tuần này"`).
- Use the language of the likely sources (Vietnamese results need Vietnamese queries — `"giá vàng SJC"` not `"SJC gold price"`).
- For comparisons, run separate queries per side (`"X benefits"`, `"X drawbacks"`) — don't mix.
- For fact-checks, run a **refutation** query (`"X claim debunked"`, `"X false"`) alongside the supporting query.
- For surveys, target distinct site types in separate queries (`site:arxiv.org`, `site:reddit.com`, …).

**Use snippets to SELECT, not to answer.** Read the snippets only to pick the best pages to open:
- Choose the mode's quota of pages (Medium: 4–8), favoring **source diversity** — different domains, different stances, primary > secondary.
- Drop near-duplicates (5 results from one domain = one source; open one, not five).
- Then go open them. **Do not stop here and synthesize from snippets** — that is the exact failure this skill exists to prevent.

### Step 3 — Read each page = the MAP step (one summary per page)

For **every** selected page — all of a round's pages in one turn, as several calls in the same message:

```
mcp__senclaw-browser__browser_read { "url": "...", "question": "<what this page should tell you>" }   // an answer quoted from the page
mcp__senclaw-browser__browser_read { "url": "...", "max_chars": 8000 }                                // or the page's own text
```

Then immediately write a **compact per-page summary** before moving to the next page:

```
### <publisher / site> — <canonical URL>
- Date: <publish/update date, or "undated">
- Key facts: 2–5 bullets, each with the concrete number / claim relevant to the question
- Stance / angle: neutral report? opinion? promotion? (so the reduce step can weight it)
- Confidence: primary source / reputable secondary / low-authority
```

Rules for the map step:
- **Distill, never paste.** A per-page summary is a handful of bullets, not the page body. A 50K-token page collapses to ~5 lines.
- **Scope big pages**: ask a `question` instead of taking the whole text, or cap it with `max_chars`. Tables and figures come back in the answer when you ask for them.
- **One summary per page, every page.** If you opened 6 pages, you have 6 summaries. This is what "tổng hợp summary lại từng trang" means.
- **A page that needs clicking through** (a cookie wall, a "show more", a filter): `browser_task { "goal": "…", "url": "…", "question": "…" }` does it and answers from the result, in one call.

### Step 4 — Verify (across the per-page summaries)

Looking at the set of summaries together:

- **Corroboration**: each claim in the final answer is backed by ≥2 independent sources (different domain, different author).
- **Recency check**: time-sensitive claims come from sources dated inside the requested window.
- **Refutation pass** (Deep only): run one query trying to refute each key claim. If a credible refutation exists, keep it as a "but…" instead of dropping it.
- If summaries **contradict**, do NOT pick one and hide the rest — carry the disagreement into the synthesis with a one-line "why this might differ" (different methodology, time window, opinion vs fact).

### Step 5 — Synthesize ALL pages = the REDUCE step

Now do the **second pass**: combine every per-page summary into one answer. This is "rồi tổng hợp lại 1 lần nữa cho toàn bộ".

1. **Cluster** the per-page facts by sub-question / facet.
2. **Reconcile numbers**: if sources agree, state the consensus value; if they differ, give the range and explain the spread.
3. **Surface disagreements** explicitly — never average away a real conflict.
4. **Write** the answer in the mode's output template (below), with every factual sentence cited.
5. The synthesis reads **over the summaries**, not the raw pages again — that second pass is what turns a pile of page-notes into a coherent, defensible answer.

Citation rules:
- Every factual sentence ends with `[Source: URL]` or `(SOURCE_NAME, URL)`.
- Group sources at the bottom only if the body has 5+ citations — otherwise inline.
- Use the **canonical URL** the user can open (no tracking suffixes).
- A fact synthesized from several sources cites **all** of them, not just one.

### Step 6 — Report gaps (Deep only)

Deep research ends with a `## Gaps and uncertainties` section listing:
- Questions you couldn't answer from the available sources.
- Sources you wanted but couldn't reach (paywall, login, dead link).
- Areas where the evidence is weak.

Non-negotiable — pretending coverage was complete when it wasn't is the worst failure mode.

## Autonomy — never ask for access

This skill is **fully autonomous** about web access. The user asked once; that is the standing permission for the whole research run.

- **Never ask "should I open these pages?" / "do you want me to access X?" / "muốn tôi mở trang không?".** Opening and reading public pages is the entire point — just do it.
- **Never pause mid-research for approval** ("I found 9 results, shall I read them?", "tiếp tục không?"). Read them and continue.
- **Don't narrate intent to ask** — go straight from search to fetch to synthesis in one flow.
- The *only* question ever allowed is to disambiguate **what** is being researched, and only when you genuinely cannot pick a most-likely reading. Even then, prefer to proceed on the likely reading and note the assumption.
- Respect login/payment boundaries (don't post, pay, or send while logged in as the user) — but **reading** public content never needs a prompt.

## Output templates

### Quick mode (single paragraph + URL)

```
{Direct answer in 1–3 sentences}. [Source: {publisher}, {url}]
```

> Giá vàng SJC hôm nay 76.0 triệu đồng/lượng (bán). [Source: PNJ, https://www.pnj.com.vn/...]

### Medium mode

```
**TL;DR**: {2–3 sentence summary including the key number / claim}.

**{Section 1}**
- {Point}. [Source: ...]
- {Point}. [Source: ...]

**{Section 2}**
- ...

**Sources**:
1. {Publisher} — {url}
2. {Publisher} — {url}
```

### Deep mode

```
# {Question}

**TL;DR**: {3–5 sentence summary}.

## {Major theme 1}
{Synthesized prose with inline citations.}

## {Major theme 2}
{Synthesized prose with inline citations.}

## Disagreements / contradictions
- {Source A says X, but Source B says Y because...}

## Sources
| # | Publisher | Date | URL | Relevance |
|---|---|---|---|---|
| 1 | ... | ... | ... | ... |

## Gaps and uncertainties
- {What we couldn't pin down}
```

## Decision tree

```
User asks a research question…
├── Concrete enough to research?
│       NO and truly ambiguous → assume the likely reading, note it, continue (do NOT ask for access)
│       YES ↓
├── Pick mode (Quick / Medium=default / Deep)
├── Step 1: plan queries (facets, source types, recency)
├── Step 2: search (one query per facet) → SELECT 4–8 diverse pages from snippets
│              (snippets choose pages; they never become the answer)
├── Step 3: for EACH page → browser_read + write a per-page summary         [MAP]
├── Step 4: verify across summaries (corroborate; refute if Deep)
├── Conflicts? → carry them into the answer, don't hide
├── Step 5: synthesize ALL summaries into one cited answer                  [REDUCE]
└── Step 6 (Deep only): gaps section
```

## Rules

- **Always open multiple pages.** Snippets pick which pages to read; they are never the synthesis source. (Medium reads 4–8 pages, every time.)
- **One summary per page, then one synthesis over all.** Map then reduce — two passes, no shortcut.
- **Never ask for permission to access or open pages.** Run the whole pipeline autonomously.
- **Plan before searching.** Don't fire queries reactively.
- **Source diversity over quantity.** 4 distinct domains > 10 results from one site.
- **Cite everything.** No factual claim without `[Source: ...]` in Medium / Deep.
- **Surface disagreements.** If sources conflict, the user wants to know.
- **Locale-aware queries.** Vietnamese question → Vietnamese query.
- **Recency-aware queries.** "Today" / "this week" / "latest" → add date or "2026" / "hôm nay" / "tuần này".
- **No fabrication.** If a fact isn't in a fetched page, don't include it.
- **Batch your calls.** A round's searches in one turn, a round's page reads in one turn.
- **Respect mode caps.** Quick ≈ 1–2 pages, Medium 4–8, Deep 10–20. If you hit the cap unanswered, the question may need narrowing — proceed with what you have and note the limit in the answer; don't stop to ask permission.

## Failure handling

| Symptom | Cause | Fix |
|---|---|---|
| Answer built from snippets only | Skipped Step 3 (the map step) | STOP — open the selected pages, summarize each, then synthesize |
| Only one page read for a Medium question | Under-research | Open more pages up to the mode quota (4–8) before answering |
| `code: "blocked"` from `browser_search`, or a page that is a bot check | The site turned SenClaw's browser away | Reword and search again; read another source; for a page only the person's Chrome can open, `browser_read` with `"browser": "extension"` when they have it connected |
| Search returns 0 results | Bad query terms | Reword the query |
| Snippets all from one site | Lopsided coverage | Add `-site:dominant-domain.com` to the next query, or `site:other-domain.com` |
| Conflicting sources | Real disagreement | Report it in the synthesis — don't pick one silently |
| Paywall / login required | Source not accessible | Note in Gaps; open an alternative source instead |
| Tempted to ask "should I open these?" | Misread autonomy | Don't ask — just open them |
| One source, can't corroborate | Insufficient corroboration | Mark the claim "[Source X, uncorroborated]"; keep it out of the TL;DR |
| Stale dates everywhere | Time-sensitive ask, old data | Re-query with stricter recency (`"2026"`, `"tuần này"`); if still old, say so in Gaps |

## Examples

### Medium (the motivating case): "Sự chênh lệch giá vàng trong tuần thế nào?"

```
1. Plan: facets = (mua–bán spread), (đỉnh/đáy trong tuần), (chênh lệch trong nước vs thế giới). Recency = "tuần này 2026". Locale = vi.
2. Search — three browser_search calls in one turn (one per facet):
     - "chênh lệch giá mua bán vàng SJC tuần này"
     - "giá vàng SJC trong tuần biến động đỉnh đáy"
     - "chênh lệch giá vàng trong nước thế giới hôm nay"
3. From snippets, SELECT ~5 diverse pages (vnexpress, tuoitre, cafef, 24h, baomoi) — different domains, drop duplicates.
4. MAP — five browser_read calls in one turn, then a per-page summary of each:
     ### Tuổi Trẻ — <url>
     - Date: 6/6/2026
     - Key facts: SJC bán ~153.2tr; spread mua–bán nới lên 3.5–4tr/lượng
     - Stance: neutral report · Confidence: reputable secondary
     ### VnExpress — <url> … (and so on for all 5 pages)
5. VERIFY: the 4tr spread appears in ≥2 domains ✓; domestic–global gap figure cross-checked.
6. REDUCE — synthesize all 5 summaries into the Medium template:
     **TL;DR**: …
     **Chênh lệch mua–bán** …[Source]…  **Chênh lệch trong nước vs thế giới** …[Source]…
     **Sources**: 5 entries.
```

### Quick: "Giá Bitcoin bây giờ?"

```
1. browser_search { "query": "bitcoin price USD live 2026" }
2. browser_read the top authoritative page (CoinGecko / Coinbase) with question "Bitcoin price in USD right now?" (don't quote the snippet).
3. Reply: "Bitcoin ~$103,400 USD. [Source: CoinGecko, https://www.coingecko.com/en/coins/bitcoin]"
```

### Deep: "Has the AI-safety field reached consensus that RLHF is a complete solution?"

```
1. Plan: facets = pro-RLHF, criticisms, alternatives (Constitutional AI, DPO), recent papers. Sources: arxiv, lab blogs, alignment forum. Recency: 2025–2026.
2. Fan-out: 10–12 queries with site: operators, a few turns of several calls each.
3. SELECT ~12 diverse pages from snippets.
4. MAP: browser_read each (several per turn), one summary per page (~12 summaries).
5. VERIFY: run 2 refutation queries ("RLHF limitations criticism"); fold credible refutations in.
6. REDUCE: Deep template — TL;DR, "Where consensus exists", "Where it doesn't", "Alternatives", Disagreements, Sources table, Gaps.
```

## Composition with other skills

- After research → save key findings to the wiki via the `wiki` skill (one entry per question for reuse).
- Send the report to a chat/channel → use `bot-channels` to format for the destination.
- Research feeds a code change → pass the key URLs as context, not the full transcript.

## See also

- `assets/templates.md` — query templates for common research patterns (price, news, comparison, fact-check, survey)
- `agent-browser` skill — the browser tools themselves, and `browser_task` for anything that needs doing on a page
