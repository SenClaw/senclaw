# Query templates for common research patterns

Drop-in starting points. Replace `<…>` placeholders and adjust language to the user's request.

## 1. Price / market data (time-sensitive single fact, often Medium mode)

| Facet | Query template | Notes |
|---|---|---|
| Current price | `<asset> price <currency> hôm nay` (vi) / `<asset> price live <currency> 2026` (en) | Include date keywords |
| Comparison | `<asset> vs <other> price chart 2026` | For trend questions |
| Source authority | `<asset> price site:<official-domain>` | Pin to an authoritative source |

Verification: corroborate across at least 2 of {official exchange, news site, aggregator (e.g. CoinGecko, Yahoo Finance, PNJ)}.

## 2. News today / this week

| Facet | Query template |
|---|---|
| Top headlines | `<topic> news today` / `<topic> news this week` |
| Multiple angles | `<topic> opinion` / `<topic> analysis` / `<topic> reaction` (one query per) |
| Locale-specific | `<topic> Vietnam` / `<topic> tin tức` (Vietnamese sources) |

Source diversity: aim for ≥1 of {wire service (Reuters/AP), local outlet, opinion piece}.

## 3. Comparison ("X vs Y")

Run separate query branches per side, then a third query for "vs":

```
Query 1: <X> overview <year>           # X's pitch
Query 2: <Y> overview <year>           # Y's pitch
Query 3: <X> vs <Y> comparison         # Head-to-head reviews
Query 4: <X> drawbacks                 # X's weaknesses (refutation pass)
Query 5: <Y> drawbacks                 # Y's weaknesses
```

Output: side-by-side criteria table.

## 4. Fact-check ("Is it true that X?")

Always include the **refutation query** — don't only look for confirmation.

```
Query 1: <claim> evidence              # Supporting
Query 2: <claim> debunked              # Refuting
Query 3: <claim> study OR research     # Primary sources
Query 4 (optional): <claim> snopes OR politifact   # Dedicated fact-checkers
```

Verdict structure: `True / Mostly true / Mixed / Mostly false / False`, plus one paragraph explaining what shifts it from one bucket to the neighbour.

## 5. Technical survey / "state of X" (typically Deep mode)

Combine site operators to hit distinct ecosystems:

```
Query 1: <topic> latest research site:arxiv.org
Query 2: <topic> github OR project
Query 3: <topic> review OR survey 2025..2026
Query 4: <topic> reddit                # Practitioner voice
Query 5: <topic> hacker news
Query 6: <topic> production deployment OR case study
```

Output: state-of-the-art section, alternative-approaches section, open-problems section.

## 6. Tutorial / docs lookup ("How do I X with Y?")

Often the official docs are the right answer — search them first, only escalate if they're missing/outdated.

```
Query 1: <library/tool> <task> site:<official-domain>      # Official docs
Query 2: <library/tool> <task> tutorial 2026               # Recent tutorial
Query 3: <library/tool> <task> github example              # Real-world code
Query 4 (if errors): <error message> stackoverflow         # Known fixes
```

## 7. Person / company research

Privacy-aware — only what's already public.

```
Query 1: "<full name>" <affiliation>             # Disambiguate
Query 2: "<full name>" recent work OR talk OR paper
Query 3: <company> news 2026
Query 4: <company> revenue OR funding OR layoffs   # Financial
```

Never aggregate personal data beyond what's needed to answer the question.

## 8. Vietnamese-locale queries

Many domestic questions are best answered in Vietnamese — the source landscape differs.

| English query | Vietnamese equivalent |
|---|---|
| "X price today" | `giá <X> hôm nay` |
| "X news today" | `tin <X> mới nhất` |
| "X review" | `đánh giá <X>` |
| "X compared to Y" | `<X> so với <Y>` |
| "X stock price" | `giá cổ phiếu <X>` |

Site operators that work in Vietnamese: `site:vnexpress.net`, `site:tuoitre.vn`, `site:thanhnien.vn`, `site:cafef.vn` (finance), `site:cafebiz.vn` (business).

## Cheat sheet — when to use which engine

| Need | Engine |
|---|---|
| General / global | google |
| When google rate-limits | bing |
| Academic / research papers | google (then add `site:arxiv.org` / `site:scholar.google.com`) |
| Code / GitHub | google (then `site:github.com`) |
| Reddit / forums | google (then `site:reddit.com`) |

The `mcp__browser__search` tool's `engine` parameter accepts `"google"` / `"bing"`.

## Common pitfalls

- **One-domain echo chamber**: 5 results from the same site is one source, not five. Add `-site:<dominant-domain>` to the next query.
- **Quoting blog posts as primary sources**: a blog post citing a study is not the study. Click through to the actual paper if the claim hinges on it.
- **Year-stale sources**: a "guide to X" written in 2019 is unreliable for current tooling. Add `2025..2026` to the query, or `after:2025` for hard recency.
- **Over-extracting**: a 50K-token page is rarely needed. Use `extract_structured` with a tight schema or `extract_text` with a CSS selector to scope.
