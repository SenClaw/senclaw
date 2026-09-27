---
name: wiki
description: Save learning/research documents to the personal wiki knowledge base and search existing content using the Wiki MCP server
version: 2.0.0
triggers:
  - "wiki"
  - "store"
  - "storegate"
  - "knowledge"
---

# Wiki Knowledge Base Management

The user's personal knowledge base is maintained using the **Wiki MCP server tools**.
The knowledge base is organized by topic folders, with each document as a Markdown file. 
All changes are automatically tracked in git.

## What to Save to the Wiki
- **Learning & Research**: Summaries of new concepts, language features, architectural patterns, or API usages.
- **Project Context**: Architecture decisions, standard operating procedures (SOPs), or project-specific setup guides.
- **Troubleshooting**: Solutions to complex bugs, environment issues, or non-obvious configurations.
- **DO NOT Save**: Ephemeral debug logs, scratchpad notes, or session-specific code that has no long-term value.

## Tools Available
You have access to the following MCP tools for managing the wiki:
- `wiki_status`: Show wiki root path and summary statistics
- `wiki_tree`: List the wiki directory tree as plain text
- `wiki_read`: Read a markdown wiki page by relative path
- `wiki_write`: Create or update a markdown wiki page (handles frontmatter + git commit)
- `wiki_search`: Search wiki pages by title, filename, or tags
- `wiki_stats`: Detailed wiki stats (categories, tags, recent files)
- `wiki_mkdir`: Create a subdirectory under the wiki

## Writing New Documents to Wiki

### Full Workflow

1. **View the directory structure** to understand existing topic categories by calling `wiki_tree`.
2. **Determine the category**:
   - Content belongs to an existing directory → save directly there.
   - No suitable directory exists → create a new topic directory by calling `wiki_mkdir`.
   - Completely uncertain → stage in `inbox/` and inform the user to categorize later.
3. **Format the document**:
   - Use clear, descriptive H1 (`#`) titles.
   - Organize content logically using H2 (`##`) and H3 (`###`) headers.
   - Use code blocks with language specifiers for technical snippets.
4. **Save the document** by calling `wiki_write`. Provide the `path`, `content`, and optional `tags` or `commit_message`.

### Example
When the user asks to save an article about Rust async:
1. Call `wiki_write` with:
   - `path`: "programming/rust/async-runtime.md"
   - `content`: "# Rust Async Runtime Explained\n\n## Core Concepts..."
   - `tags`: ["rust", "async", "tokio"]

## Organizing Existing Documents

Use this workflow when the user wants to organize, classify, or tidy up documents already on disk into the wiki.

### Full Workflow

1. **Read the document** using standard file reading tools to get the title and content.
2. **View the wiki directory structure** by calling `wiki_tree`.
3. **Determine the target category** using the same rules as saving.
4. **Copy the file** to the wiki by calling `wiki_write`:
   - Pass the document's text as `content`.
   - Set the `source` argument to the original file's absolute path (this will automatically add it to the frontmatter).
   - Provide relevant `tags`.
   - Do NOT rewrite or regenerate the document body content during organization. Keep the original text intact.

## Searching Existing Knowledge

When the user asks to search the knowledge base, use the `wiki_search` tool:
- Search by query: `wiki_search(query="rust async")`
- Filter by tags: `wiki_search(query="", filterTags=["tokio"])`

If you need to read a specific file from the search results, use `wiki_read(path="...")`.

## When to Trigger

**Write workflow** — user says:
- "add to wiki", "save to knowledge base", "archive this"
- "save this to wiki", "note it in the knowledge base"

**Organize workflow** — user says:
- "put this xxx file in wiki"
- "file these into the knowledge base"
- "sort these files into the wiki"

**Search workflow** — user says:
- "check my notes on X", "do I have anything in my wiki about X"
- "look up X in my knowledge base", "search my wiki for X"
- "what did I save about X", "have I documented X before"
- "find my notes on X", "pull up what I know about X"

## Configuration / Information Storage Guidelines
- **Filenames**: Use concise descriptions of the topic (`async-runtime.md` not `notes-on-async-runtime-learning.md`), no more than 40 characters.
- **Frontmatter**: Do not manually write YAML frontmatter. The `wiki_write` tool will automatically handle YAML frontmatter creation and updating based on the parameters you pass (`tags`, `source`).
- **Git Tracking**: The MCP server automatically commits changes to git. You do not need to run git commands manually.
