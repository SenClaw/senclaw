---
name: project-docs
description: Write documentation as a markdown file inside the project being worked on, so a change can be followed later and a request for documentation produces a file the user can reopen
version: 1.0.0
triggers:
  - "tài liệu"
  - "documentation"
  - "document this"
  - "viết tài liệu"
  - "ghi lại"
  - "docs"
---

# Project documentation

Documentation about a project's code belongs **in that project**, so it travels
with a clone and a second person can read it. That is what the `doc_*` tools
are for.

Answering "cho tôi tài liệu về X" in the chat does not count as documenting it.
The chat scrolls away. Write the file, then tell the user its path.

## Which tool, and when

| Situation | Do |
|---|---|
| User asks for documentation about anything in this project | `doc_write`, then quote the returned path |
| A change altered behaviour, a contract, a schema, or a decision | Offer to record it, then `doc_write` |
| You need to know what is already documented | `doc_list` first — do not create a second document on the same subject |
| A document was deleted or renamed outside the tools | `doc_index_rebuild` |
| Knowledge that is about **you and the user**, not this project | `wiki_write` instead — the wiki is one personal knowledge base across every project |
| A rename, a typo fix, a formatting pass | Nothing. Not every change deserves a page |

## Before writing

1. **`doc_list` first.** It reports the docs root, whether one exists, what is
   already there, and which language those documents are written in.
2. **Match that language.** A project whose `docs/` is Vietnamese keeps getting
   Vietnamese, whatever language the question came in. When the project has no
   documents yet, use the language the user is writing in.
3. **A project with no `docs/` directory**: creating one is the user's call.
   Ask before the first write, and say that it will become part of their repo.

## What a document must contain

Open with a status line, because a design that was never built reads exactly
like a shipped feature otherwise:

```markdown
**Status:** shipped 2026-09-12 · draft, not implemented · superseded by <link>
```

Then, in whatever order suits the subject:

- **What changed and why** — the problem in one paragraph, not a changelog.
- **Where it lives** — file paths, so the reader can follow the flow of the
  code rather than search for it.
- **What is verified and what is not.** This is the most valuable section and
  the one most often skipped. Compiling is not running. Say plainly which
  claims are backed by a test, which by a live run, and which by neither.
- **The traps** — the mistakes that cost time, especially the ones that fail
  silently. A reader who avoids one of these got more from the page than from
  any summary of the design.

## Rules

- **Never invent verification.** If you did not run it, write that you did not.
- **`doc_write` does not commit.** The file is written and left in the working
  tree; committing is the user's decision about their own history.
- **Paths are relative to `docs/` and must end in `.md`.** Anything that climbs
  out with `..`, is absolute, or resolves through a symlink to somewhere else
  is refused — that refusal is the tool working, not a bug to route around
  with `Write`.
- **Do not overwrite silently.** `doc_write` refuses an existing path unless
  you pass `overwrite`. Prefer a new path, or read the existing document first
  and update it deliberately.
- **The index is generated.** `docs/README.md` is rebuilt from the files on
  disk after every write; do not hand-edit the block between its markers.
  Anything you write outside those markers is preserved.

## Suggested layout

```
docs/
  README.md              generated index — do not hand-edit the marked block
  <subject>.md           one page per subject
  changes/<date>-<slug>.md   what a particular change did
```
