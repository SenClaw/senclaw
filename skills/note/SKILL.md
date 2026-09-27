---
name: note
description: Structured note-taking skill — capture, organize, and retrieve notes using proven methodologies (Cornell, Zettelkasten, PARA) via the Space notes API
version: 1.0.0
when-to-use: When the user wants to take notes, record information, capture meeting minutes, journal, brainstorm, log decisions, or organize knowledge in their Space
triggers:
  - note
  - ghi chú
  - take note
  - meeting notes
  - journal
  - brainstorm
  - decision log
  - recap
  - tóm tắt
  - nhật ký
  - ý tưởng
mcp_servers:
  - senclaw-space
allowed-tools:
  - space_note_create
  - space_note_update
  - space_note_search
  - space_note_list
  - space_note_delete
  - space_current_time
---

# Note — Structured Note-Taking for AI Agents

You are a note-taking specialist. Your job is to capture information into Space notes with clarity, structure, and retrievability. Every note you create should be **useful when re-read weeks later** by someone with no context.

## Required Tool Discovery

Tools register as `space_<verb>` on the `senclaw-space` MCP server. Call them by the
**canonical bridge name** `mcp__space__<verb>` — the resolver strips the redundant
`space_` prefix once, so `space_note_create` → `mcp__space__note_create`. The bare
`space_<verb>(...)` notation used below in this doc maps to the same tool.

Before calling any note tool, verify it is visible. If not, call `ToolSearch` first:

```
ToolSearch { query: "select:mcp__space__note_create" }
ToolSearch { query: "select:mcp__space__note_search" }
ToolSearch { query: "select:mcp__space__current_time" }
```

---

## Core Principles

1. **Atomic**: One note = one topic. If a conversation covers 3 topics, create 3 notes.
2. **Titled for scanning**: Titles answer "what is this about?" in under 10 words. Use format: `[Type] Subject — Key detail`. Examples:
   - `[Meeting] Sprint Review — Q3 pricing decided`
   - `[Decision] Use PostgreSQL over MongoDB`
   - `[Idea] Auto-tag notes by conversation topic`
3. **Structured body**: Use the appropriate template (see below). Never dump raw text.
4. **Tagged for retrieval**: Every note gets 2–5 tags from the taxonomy.
5. **Timestamped**: Always call `space_current_time()` first to get accurate local time.

---

## Tag Taxonomy

Use consistent tags so notes are findable later. Combine a **type tag** with **topic tags**.

### Type tags (pick one)
| Tag | Use for |
|-----|---------|
| `meeting` | Meeting notes, standup recaps |
| `decision` | Decisions made, with rationale |
| `idea` | Ideas, brainstorms, what-ifs |
| `todo` | Action items, task lists |
| `research` | Research findings, comparisons |
| `journal` | Daily reflections, status updates |
| `reference` | How-tos, cheat sheets, configs |
| `incident` | Outage/bug post-mortems |
| `learning` | TILs, lessons learned |
| `draft` | Work-in-progress, not finalized |

### Topic tags (pick 1–4)
Use lowercase, hyphenated. Match the project/domain:
- Project names: `senclaw`, `chrome-ext`, `whisper-port`
- Domains: `infra`, `frontend`, `ml`, `design`, `security`
- People/teams: `team-core`, `team-ml`
- Urgency: `urgent`, `blocked`, `follow-up`

---

## Note Templates

Choose the template that fits the note type. Adapt — don't force content into sections that don't apply.

### Meeting Notes

```markdown
**Date**: {date} | **Attendees**: {names}
**Purpose**: {one-line goal}

## Key Points
- {point 1}
- {point 2}

## Decisions
- {decision}: {rationale}

## Action Items
- [ ] {task} — @{owner} by {deadline}
- [ ] {task} — @{owner} by {deadline}

## Open Questions
- {question that wasn't resolved}
```

### Decision Record

```markdown
**Date**: {date} | **Status**: Decided / Proposed / Superseded

## Context
{What situation prompted this decision? 2–3 sentences.}

## Options Considered
1. **{Option A}**: {pros} / {cons}
2. **{Option B}**: {pros} / {cons}

## Decision
{What was chosen and why.}

## Consequences
- {Expected impact}
- {Trade-offs accepted}
```

### Idea / Brainstorm

```markdown
## Core Idea
{One paragraph: what is it, why does it matter?}

## How It Could Work
- {Mechanism or approach}

## Open Questions
- {What needs validation?}

## Related
- {Links to related notes, projects, or references}
```

### Research Note

```markdown
## Question
{What are we trying to answer?}

## Findings
### {Source / Approach 1}
- {Key finding}

### {Source / Approach 2}
- {Key finding}

## Synthesis
{What does this mean? What's the recommendation?}

## Sources
- {source links or references}
```

### Daily Journal

```markdown
## Done Today
- {Completed item}

## In Progress
- {What's ongoing, current status}

## Blockers
- {What's stuck and why}

## Tomorrow
- {Plan for next day}
```

### Todo / Action List

```markdown
## Priority
- [ ] {urgent/important task}

## This Week
- [ ] {task with context}

## Backlog
- [ ] {lower priority task}

## Done
- [x] {completed task} — {date}
```

### Reference / How-To

```markdown
## What
{What this reference covers.}

## Steps / Details
1. {Step or fact}
2. {Step or fact}

## Gotchas
- {Common pitfall or edge case}

## See Also
- {Related references}
```

### Incident Post-Mortem

```markdown
**Date**: {date} | **Severity**: P0/P1/P2 | **Duration**: {time}

## Summary
{What happened in one sentence.}

## Timeline
- {HH:MM} — {event}
- {HH:MM} — {event}

## Root Cause
{Technical root cause.}

## Fix Applied
{What was done to resolve it.}

## Action Items
- [ ] {preventive measure}
```

---

## Workflow: How to Take Notes

### Step 1 — Identify note type
From the conversation or user request, determine which template applies. If unclear, ask: "Bạn muốn ghi chú dạng nào?" with options.

### Step 2 — Get current time
```
space_current_time()
```

### Step 3 — Extract and structure
Pull the key information from the conversation. Apply the template. Be concise — capture **what matters**, not every word said.

Rules for extraction:
- **Decisions**: Capture the decision AND the reasoning. "We chose X" is incomplete; "We chose X because Y outweighs Z" is useful.
- **Action items**: Must have owner and deadline when available.
- **Context**: Include enough that someone reading cold understands why this note exists.
- **Conversations**: Distill, don't transcribe. The note should be shorter than the conversation.

### Step 4 — Title and tag
Apply naming convention `[Type] Subject — Key detail` and select tags from taxonomy.

### Step 5 — Create the note
```
space_note_create(title, body, tags, folder_id?)
```

### Step 6 — Confirm to user
Show a brief summary of what was captured. Example:
> Đã ghi chú: **[Meeting] Sprint Review — Q3 pricing decided**
> Tags: `meeting`, `senclaw`, `pricing`
> 3 action items tracked.

---

## Smart Behaviors

### Auto-detect note opportunities
When a conversation contains any of these, **proactively suggest** creating a note:
- A decision was made → "Ghi lại quyết định này nhé?"
- Action items were discussed → "Tạo note todo cho các việc cần làm?"
- The user shares research findings → "Lưu lại findings này?"
- A problem was resolved → "Ghi chú cách fix?"

### Append to existing notes
Before creating, search for existing notes on the same topic:
```
space_note_search(query, limit: 5)
```
If a matching note exists, ask: "Đã có note [{title}] về chủ đề này. Cập nhật note đó hay tạo mới?"

### Link related notes
When creating a note related to an existing one, mention the relationship in the body:
```markdown
## Related
- See also: [Decision] Use PostgreSQL — {note_id}
```

### Batch creation
If a conversation yields multiple notes (e.g., a meeting with decisions + action items + ideas), create them separately and cross-reference.

---

## Folder Organization (PARA Method)

When `folder_id` is available, organize notes by the PARA framework:

| Folder | Contains | Example |
|--------|----------|---------|
| **Projects** | Active projects with deadlines | `senclaw-v2`, `chrome-ext` |
| **Areas** | Ongoing responsibilities | `infra`, `hiring`, `security` |
| **Resources** | Reference material | `rust-patterns`, `api-docs` |
| **Archive** | Completed/inactive items | `q2-launch`, `old-auth` |

---

## Search and Retrieval

When a user asks to find or recall information:

1. **Direct search**: `space_note_search(query)` with keywords
2. **Tag filter**: `space_note_list(tag: "decision")` for all decisions
3. **Folder browse**: `space_note_list(folder_id: "...")` for project-specific notes
4. **Combine**: Search first, then filter results by tag/date in your response

Present search results as a concise list:
```
Found 3 notes:
1. [Meeting] Sprint Review — Q3 pricing (2 days ago, #meeting #pricing)
2. [Decision] Pricing tier structure (1 week ago, #decision #pricing)
3. [Research] Competitor pricing analysis (2 weeks ago, #research #pricing)
```

---

## Language

- Match the user's language (Vietnamese or English)
- Titles and tags: always English for consistency and searchability
- Body content: match user's language
- Date display: use the format from `space_current_time().display`

---

## When to Use This Skill vs. Others

| Situation | Use |
|-----------|-----|
| Quick note from conversation | **This skill** |
| Long-form knowledge article | Wiki skill (`wiki_write`) |
| Remind about a future event | Space skill (`space_event_create`) |
| Daily standup recap | **This skill** (journal template) |
| Code documentation | Code skill or inline comments |
| Todo/task tracking | **This skill** (todo template) |

**User defaults override this table**: if the system prompt carries a
`## User defaults` block naming a default note store (`wiki_write` or
`memory_save` instead of `space_note_create`), send plain "ghi chú lại X"
requests there. The table above then only applies where the user's intent
clearly asks for a different store.


---

## Importing from Apple Notes

```
space_sync_apple_notes(limit: 500)
```

Reads the **local Notes.app** and imports into Space notes, matching each note
by its Notes id so a second run updates rather than duplicates.

- **macOS only.** iCloud Notes is not reachable over IMAP, so there is no
  route on Linux or Windows. Elsewhere the tool reports that in `errors` —
  do not read a zero count as "the user has no notes".
- **The first call raises a macOS permission prompt.** Until the user grants
  Automation access to Notes.app the tool answers with `needsReauth: true` and
  the setting to change; tell them the path rather than retrying.
- Notes are imported one way. Editing the Space copy does not change the note
  in Notes.app.
