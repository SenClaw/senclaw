//! Curated memory — agent-authored, human-readable memory files with a `MEMORY.md` index.
//!
//! A curated layer on top of the basic-memory infrastructure: each memory is one
//! `base/memory/{name}.md` file with YAML frontmatter, and `base/MEMORY.md` is a
//! newest-first index of `- [Title](memory/file.md) — hook` lines. The daemon's
//! `MemoryManager` watcher picks the files up on its next poll and FTS/vector-indexes
//! them, so recall reuses the existing `hybrid_search` — no new storage.
//!
//! Layout mirrors `MemoryManager::sync_folder`: `base` is `agents_dir/{folder}` (or a
//! custom cowork dir); `MEMORY.md` lives at `base/MEMORY.md`; files at `base/memory/*.md`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The four memory categories (see docs/curated-memory-design.md).
pub const MEMORY_TYPES: [&str; 4] = ["project", "reference", "feedback", "user"];

/// Outcome of a save.
pub struct SavedMemory {
    pub name: String,
    pub path: PathBuf,
    /// True if an existing memory of the same name was overwritten.
    pub updated: bool,
}

/// Normalize an arbitrary label into a safe kebab-case slug (a-z, 0-9, `-`).
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Derive a human-readable title from a slug (`is-admin-removed` -> `is admin removed`).
fn title_from_slug(slug: &str) -> String {
    slug.replace('-', " ")
}

/// Emit a YAML scalar, quoting when the value could be misparsed.
fn yaml_scalar(s: &str) -> String {
    let needs_quote = s.is_empty()
        || s.contains(':')
        || s.contains('#')
        || s.contains('"')
        || s.starts_with([
            '-', '?', '*', '&', '!', '[', ']', '{', '}', '>', '|', '\'', '@', '`',
        ])
        || s.starts_with(char::is_whitespace)
        || s.ends_with(char::is_whitespace);
    if needs_quote {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

fn memory_dir(base: &Path) -> PathBuf {
    base.join("memory")
}

/// Save (or update) a curated memory. Returns an error if a same-name file exists and
/// `supersede` is false (the update-not-duplicate rule).
#[allow(clippy::too_many_arguments)]
pub fn save(
    base: &Path,
    name: &str,
    description: &str,
    body: &str,
    mem_type: &str,
    title: Option<&str>,
    origin: &str,
    date: &str,
    supersede: bool,
) -> Result<SavedMemory> {
    let slug = slugify(name);
    if slug.is_empty() {
        bail!("invalid name '{name}': produces an empty slug after normalization");
    }
    if description.trim().is_empty() {
        bail!("description is required (it is the recall hook)");
    }
    let mem_type = mem_type.trim();
    if !MEMORY_TYPES.contains(&mem_type) {
        bail!(
            "invalid type '{mem_type}'; expected one of: {}",
            MEMORY_TYPES.join(", ")
        );
    }

    let dir = memory_dir(base);
    fs::create_dir_all(&dir).with_context(|| format!("create memory dir {}", dir.display()))?;
    let path = dir.join(format!("{slug}.md"));

    let updated = path.exists();
    if updated && !supersede {
        bail!(
            "memory '{slug}' already exists — pass supersede=true to update it (do not create a duplicate)"
        );
    }

    let title = title
        .map(|t| t.to_string())
        .unwrap_or_else(|| title_from_slug(&slug));

    let mut content = String::new();
    content.push_str("---\n");
    content.push_str(&format!("name: {slug}\n"));
    content.push_str(&format!(
        "description: {}\n",
        yaml_scalar(description.trim())
    ));
    content.push_str("metadata:\n");
    content.push_str("  node_type: memory\n");
    content.push_str(&format!("  type: {mem_type}\n"));
    content.push_str(&format!("  originSessionId: {}\n", yaml_scalar(origin)));
    content.push_str(&format!("  createdAt: {date}\n"));
    content.push_str("---\n\n");
    content.push_str(body.trim_end());
    content.push('\n');

    fs::write(&path, content).with_context(|| format!("write memory {}", path.display()))?;
    update_index(base, &slug, &title, description.trim())?;

    Ok(SavedMemory {
        name: slug,
        path,
        updated,
    })
}

/// Delete a curated memory and its index entry. Returns true if the file existed.
pub fn delete(base: &Path, name: &str) -> Result<bool> {
    let slug = slugify(name);
    if slug.is_empty() {
        bail!("invalid name '{name}'");
    }
    let path = memory_dir(base).join(format!("{slug}.md"));
    let existed = path.exists();
    if existed {
        fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    remove_from_index(base, &slug)?;
    Ok(existed)
}

/// Insert/replace the index line for `slug`, keeping it newest-first.
/// Also prunes any leftover `conversation-summary-*` entries that were
/// written by older versions of the verbatim fallback — those are now
/// FTS-only and should not appear in the curated index.
fn update_index(base: &Path, slug: &str, title: &str, description: &str) -> Result<()> {
    let link = format!("memory/{slug}.md");
    let new_line = format!("- [{title}]({link}) — {description}");
    write_index(base, |bullets| {
        bullets.retain(|b| {
            !b.contains(&format!("]({link})")) && !b.contains("memory/conversation-summary-")
        });
        bullets.insert(0, new_line.clone());
    })
}

/// Remove all `conversation-summary-*` entries from MEMORY.md. Called
/// once at startup or when the first real curated memory is saved, so
/// profiles polluted by the old verbatim fallback are cleaned up.
pub fn purge_summary_index_entries(base: &Path) -> Result<usize> {
    let index_path = base.join("MEMORY.md");
    if !index_path.exists() {
        return Ok(0);
    }
    let before = fs::read_to_string(&index_path).unwrap_or_default();
    let count = before
        .lines()
        .filter(|l| l.contains("memory/conversation-summary-"))
        .count();
    if count == 0 {
        return Ok(0);
    }
    write_index(base, |bullets| {
        bullets.retain(|b| !b.contains("memory/conversation-summary-"));
    })?;
    tracing::info!("[curated] purged {count} conversation-summary entries from MEMORY.md");
    Ok(count)
}

fn remove_from_index(base: &Path, slug: &str) -> Result<()> {
    let link = format!("memory/{slug}.md");
    write_index(base, |bullets| {
        bullets.retain(|b| !b.contains(&format!("]({link})")));
    })
}

/// Read `MEMORY.md`, split off the bullet list, let `mutate` edit it, rewrite.
fn write_index(base: &Path, mutate: impl FnOnce(&mut Vec<String>)) -> Result<()> {
    let index_path = base.join("MEMORY.md");
    let existing = fs::read_to_string(&index_path).unwrap_or_default();

    let mut header: Vec<&str> = Vec::new();
    let mut bullets: Vec<String> = Vec::new();
    let mut seen_bullet = false;
    for line in existing.lines() {
        if line.trim_start().starts_with("- ") {
            seen_bullet = true;
            bullets.push(line.to_string());
        } else if !seen_bullet {
            header.push(line);
        }
        // Non-bullet lines after the list are dropped (the index is bullets-only).
    }

    mutate(&mut bullets);

    // Normalize the header to a single title line + blank separator.
    let title_line = header
        .iter()
        .find(|l| l.trim_start().starts_with('#'))
        .map(|s| s.to_string())
        .unwrap_or_else(|| "# Memory index".to_string());

    let mut out = String::new();
    out.push_str(&title_line);
    out.push_str("\n\n");
    for b in &bullets {
        out.push_str(b);
        out.push('\n');
    }

    fs::create_dir_all(base).ok();
    fs::write(&index_path, out).with_context(|| format!("write {}", index_path.display()))?;
    Ok(())
}

/// Frontmatter fields relevant to recall presentation.
pub struct MemoryMeta {
    pub name: String,
    pub description: String,
    pub mem_type: String,
}

/// Parse the YAML frontmatter of a curated memory file. Returns `None` if the file has
/// no frontmatter block (e.g. daily logs, `MEMORY.md`).
pub fn read_meta(path: &Path) -> Option<MemoryMeta> {
    let content = fs::read_to_string(path).ok()?;
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let front = &rest[..end];

    let mut name = String::new();
    let mut description = String::new();
    let mut mem_type = String::new();
    for line in front.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("name:") {
            name = unquote(v.trim());
        } else if let Some(v) = t.strip_prefix("description:") {
            description = unquote(v.trim());
        } else if let Some(v) = t.strip_prefix("type:") {
            mem_type = unquote(v.trim());
        }
    }
    if name.is_empty() {
        // fall back to the filename stem
        name = path.file_stem()?.to_string_lossy().to_string();
    }
    Some(MemoryMeta {
        name,
        description,
        mem_type,
    })
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("curated-test-{}", std::process::id()));
        // fresh per-test subdir via a counter-ish nonce from the call site
        p
    }

    #[test]
    fn slugify_normalizes() {
        assert_eq!(slugify("Is_Admin Removed!"), "is-admin-removed");
        assert_eq!(slugify("  hello  world  "), "hello-world");
        assert_eq!(slugify("已经-DONE"), "done");
        assert_eq!(slugify("---"), "");
    }

    #[test]
    fn yaml_quoting() {
        assert_eq!(yaml_scalar("plain hook"), "plain hook");
        assert_eq!(yaml_scalar("has: colon"), "\"has: colon\"");
        assert_eq!(yaml_scalar("- dash start"), "\"- dash start\"");
    }

    #[test]
    fn save_recall_delete_roundtrip() {
        let base = tmp().join("roundtrip");
        let _ = fs::remove_dir_all(&base);

        let saved = save(
            &base,
            "Test Memory",
            "a short recall hook",
            "**Why:** because.\n**How to apply:** carefully.",
            "project",
            None,
            "group-1",
            "2026-07-01",
            false,
        )
        .unwrap();
        assert_eq!(saved.name, "test-memory");
        assert!(!saved.updated);

        let file = fs::read_to_string(base.join("memory/test-memory.md")).unwrap();
        assert!(file.contains("name: test-memory"));
        assert!(file.contains("type: project"));
        assert!(file.contains("**Why:**"));

        let index = fs::read_to_string(base.join("MEMORY.md")).unwrap();
        assert!(index.starts_with("# Memory index"));
        assert!(index.contains("- [test memory](memory/test-memory.md) — a short recall hook"));

        // Duplicate without supersede fails.
        let dup = save(
            &base,
            "test-memory",
            "hook",
            "body",
            "project",
            None,
            "group-1",
            "2026-07-01",
            false,
        );
        assert!(dup.is_err());

        // Supersede updates in place; index keeps one line.
        let up = save(
            &base,
            "test-memory",
            "updated hook",
            "new body",
            "reference",
            Some("Nice Title"),
            "group-1",
            "2026-07-02",
            true,
        )
        .unwrap();
        assert!(up.updated);
        let index = fs::read_to_string(base.join("MEMORY.md")).unwrap();
        assert_eq!(index.matches("memory/test-memory.md").count(), 1);
        assert!(index.contains("- [Nice Title](memory/test-memory.md) — updated hook"));

        // Delete removes file + index line.
        assert!(delete(&base, "test-memory").unwrap());
        assert!(!base.join("memory/test-memory.md").exists());
        let index = fs::read_to_string(base.join("MEMORY.md")).unwrap();
        assert!(!index.contains("memory/test-memory.md"));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn index_orders_newest_first() {
        let base = tmp().join("ordering");
        let _ = fs::remove_dir_all(&base);
        for (i, n) in ["first", "second", "third"].iter().enumerate() {
            save(
                &base,
                n,
                &format!("hook {i}"),
                "body",
                "project",
                None,
                "g",
                "2026-07-01",
                false,
            )
            .unwrap();
        }
        let index = fs::read_to_string(base.join("MEMORY.md")).unwrap();
        let third = index.find("memory/third.md").unwrap();
        let first = index.find("memory/first.md").unwrap();
        assert!(
            third < first,
            "newest (third) must come before oldest (first)"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn purge_removes_summary_entries_keeps_curated() {
        let base = tmp().join("purge");
        let _ = fs::remove_dir_all(&base);

        // Simulate old state: MEMORY.md has both curated and summary entries.
        fs::create_dir_all(&base).unwrap();
        fs::write(
            base.join("MEMORY.md"),
            "# Memory\n\n\
             - [Conversation summary 2026-07-11](memory/conversation-summary-2026-07-11.md) — Auto-saved\n\
             - [real memory](memory/real-memory.md) — a real curated memory\n\
             - [Conversation summary 2026-07-07](memory/conversation-summary-2026-07-07.md) — Auto-saved\n",
        )
        .unwrap();

        let purged = purge_summary_index_entries(&base).unwrap();
        assert_eq!(purged, 2);

        let index = fs::read_to_string(base.join("MEMORY.md")).unwrap();
        assert!(!index.contains("conversation-summary"));
        assert!(index.contains("real-memory.md"));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn save_also_prunes_legacy_summary_entries() {
        let base = tmp().join("save-prune");
        let _ = fs::remove_dir_all(&base);

        // Pre-populate with a summary entry (simulates old behavior).
        fs::create_dir_all(&base).unwrap();
        fs::write(
            base.join("MEMORY.md"),
            "# Memory\n\n\
             - [Conversation summary 2026-07-11](memory/conversation-summary-2026-07-11.md) — Auto-saved\n",
        )
        .unwrap();

        // Save a real curated memory — should prune the summary entry.
        save(
            &base,
            "real-thing",
            "hook",
            "body",
            "project",
            None,
            "g",
            "2026-07-13",
            false,
        )
        .unwrap();
        let index = fs::read_to_string(base.join("MEMORY.md")).unwrap();
        assert!(
            !index.contains("conversation-summary"),
            "summary entry should be pruned"
        );
        assert!(index.contains("real-thing.md"));

        let _ = fs::remove_dir_all(&base);
    }
}
