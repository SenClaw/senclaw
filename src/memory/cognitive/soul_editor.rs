//! Pure markdown editor for `SOUL.md` mutations.
//!
//! Used by [`crate::tools::persona_update::PersonaUpdateTool`] to apply
//! structured persona patches the agent issues in response to user
//! instructions like "from now on, respond more concisely".
//!
//! ## Why a dedicated module
//!
//! We could just have the agent call `Write` against SOUL.md, but that
//! has three failure modes:
//!
//! 1. **Race** — the LLM can hallucinate the whole file when only one
//!    line needed changing, clobbering hand-written sections.
//! 2. **No idempotency** — re-issuing the same instruction appends a
//!    duplicate bullet every time.
//! 3. **No structure** — there's no anchor to the cognitive graph's
//!    `Persona(folder, "soul:<section>")` tags, so retrieval scope can't
//!    follow the edit.
//!
//! This editor handles all three with a small, testable transform:
//! parse → patch → serialise. The agent passes a structured patch (which
//! H2 section, what action, what content); the file's other sections are
//! preserved byte-for-byte.
//!
//! ## Section model
//!
//! We treat each `## Heading` as a section boundary. The body between two
//! H2 headers is a list of bullets (`- item`) interleaved with prose
//! lines; both are kept on read, both rewritten on edit. The auto-managed
//! `## Learned` block (markers from `soul_ingest.rs`) is left alone —
//! consolidate is the only writer for it.

use anyhow::Result;

const LEARNED_BEGIN: &str = "<!-- senclaw:learned:start -->";
const LEARNED_END: &str = "<!-- senclaw:learned:end -->";

/// What kind of edit the caller wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchAction {
    /// Append a bullet item to the section body. Dedupes against
    /// case-insensitive trimmed equality so re-issuing the same patch is
    /// idempotent.
    AddBullet,
    /// Append a free-form line to the section body. Same idempotency.
    AppendLine,
    /// Replace the entire body of the section (preserves the H2 header
    /// itself). Use when the agent reshapes a section wholesale.
    ReplaceSection,
    /// Remove a single bullet item by case-insensitive match. No-op when
    /// the item isn't found.
    RemoveBullet,
}

/// One structured edit. `section` is the H2 label; matched
/// case-insensitively. If the section doesn't exist (and the action is
/// any of Add/Append/Replace), it gets created at the end of the file.
#[derive(Debug, Clone)]
pub struct SoulPatch {
    pub section: String,
    pub action: PatchAction,
    pub content: String,
}

/// Apply a patch and return the new SOUL.md text. Idempotent for Add*
/// actions: dup-checked content yields the original text byte-for-byte.
///
/// `existing` may be empty (fresh SOUL.md) — we'll create the section
/// and prepend a sensible header if so.
pub fn apply_patch(existing: &str, patch: &SoulPatch) -> Result<String> {
    let learned = extract_learned_block(existing);
    let body_without_learned = strip_learned_block(existing);

    // Split into sections. Header lines (`## X`) keyed lowercased so the
    // patch can be case-insensitive.
    let mut sections = parse_sections(&body_without_learned);
    let key = patch.section.to_lowercase();

    // Locate or create the target section.
    let idx = match sections.iter().position(|s| s.label.to_lowercase() == key) {
        Some(i) => i,
        None => {
            sections.push(Section {
                label: titlecase_label(&patch.section),
                body: String::new(),
            });
            sections.len() - 1
        }
    };

    match patch.action {
        PatchAction::AddBullet => {
            let bullet = format!("- {}", patch.content.trim());
            if !contains_bullet_case_insensitive(&sections[idx].body, &patch.content) {
                if !sections[idx].body.ends_with('\n') && !sections[idx].body.is_empty() {
                    sections[idx].body.push('\n');
                }
                sections[idx].body.push_str(&bullet);
                sections[idx].body.push('\n');
            }
        }
        PatchAction::AppendLine => {
            let line = patch.content.trim();
            if !line.is_empty() && !sections[idx].body.contains(line) {
                if !sections[idx].body.ends_with('\n') && !sections[idx].body.is_empty() {
                    sections[idx].body.push('\n');
                }
                sections[idx].body.push_str(line);
                sections[idx].body.push('\n');
            }
        }
        PatchAction::ReplaceSection => {
            sections[idx].body = patch.content.trim_end().to_string();
            if !sections[idx].body.is_empty() && !sections[idx].body.ends_with('\n') {
                sections[idx].body.push('\n');
            }
        }
        PatchAction::RemoveBullet => {
            sections[idx].body = remove_bullet(&sections[idx].body, &patch.content);
        }
    }

    // Reassemble: pre-section prelude + each section + Learned block
    // (re-attached at the end so it never gets clobbered).
    let mut out = String::new();
    if let Some(prelude) = take_prelude(&body_without_learned) {
        out.push_str(&prelude);
        if !out.ends_with("\n\n") {
            // Ensure exactly one blank line between prelude and first H2.
            while out.ends_with('\n') {
                out.pop();
            }
            out.push_str("\n\n");
        }
    }
    for s in &sections {
        out.push_str(&format!("## {}\n\n", s.label));
        out.push_str(s.body.trim_end());
        if !s.body.is_empty() {
            out.push('\n');
        }
        out.push('\n');
    }
    if let Some(block) = learned {
        out.push_str(&block);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    Ok(out.trim_end().to_string() + "\n")
}

// =====================================================================
// Internals
// =====================================================================

#[derive(Debug, Clone)]
struct Section {
    label: String,
    body: String,
}

fn parse_sections(text: &str) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    let mut current: Option<Section> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            if let Some(s) = current.take() {
                sections.push(s);
            }
            current = Some(Section {
                label: rest.trim().to_string(),
                body: String::new(),
            });
        } else if line.starts_with("# ") {
            // H1 is the document title — kept as part of the prelude
            // (see `take_prelude`). Skip here.
            continue;
        } else if let Some(s) = current.as_mut() {
            s.body.push_str(line);
            s.body.push('\n');
        }
        // Lines before any H2 belong to the prelude — captured separately.
    }
    if let Some(s) = current {
        sections.push(s);
    }
    // Trim body whitespace once at parse time so dedupe checks stay clean.
    for s in &mut sections {
        s.body = s.body.trim().to_string();
    }
    sections
}

/// Returns text up to (but not including) the first `## ` header. Includes
/// the optional `# Title` line. Returns None when the file has no H2
/// header (entire file is prelude — we don't strip it).
fn take_prelude(text: &str) -> Option<String> {
    let mut out = String::new();
    for line in text.lines() {
        if line.starts_with("## ") {
            return Some(out.trim_end().to_string());
        }
        out.push_str(line);
        out.push('\n');
    }
    None
}

fn titlecase_label(s: &str) -> String {
    // Best-effort title case: first letter of each whitespace-separated
    // token upper-cased. Avoids dragging in heavyweight i18n deps.
    s.split_whitespace()
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().chain(chars).collect(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn contains_bullet_case_insensitive(body: &str, content: &str) -> bool {
    let needle = content.trim().to_lowercase();
    body.lines().any(|l| {
        let l = l.trim_start_matches('-').trim().to_lowercase();
        l == needle
    })
}

fn remove_bullet(body: &str, content: &str) -> String {
    let needle = content.trim().to_lowercase();
    body.lines()
        .filter(|l| {
            let stripped = l.trim_start_matches('-').trim().to_lowercase();
            stripped != needle
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// If the file contains the managed `## Learned` block, return its full
/// text (including the `## Learned` header) so we can re-attach it
/// untouched after editing other sections.
fn extract_learned_block(text: &str) -> Option<String> {
    let start_marker = text.find(LEARNED_BEGIN)?;
    let end_marker = text.find(LEARNED_END)?;
    if end_marker < start_marker {
        return None;
    }
    // Walk back to the nearest `## Learned` header so we capture the
    // whole block including the heading.
    let header = text[..start_marker].rfind("## Learned")?;
    let block_end = end_marker + LEARNED_END.len();
    Some(text[header..block_end].to_string())
}

fn strip_learned_block(text: &str) -> String {
    if let Some(block) = extract_learned_block(text) {
        text.replace(&block, "").trim_end().to_string() + "\n"
    } else {
        text.to_string()
    }
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn p(section: &str, action: PatchAction, content: &str) -> SoulPatch {
        SoulPatch {
            section: section.to_string(),
            action,
            content: content.to_string(),
        }
    }

    #[test]
    fn add_bullet_to_existing_section() {
        let src = "# Agent\n\n## Guidelines\n\n- Be friendly\n";
        let out = apply_patch(src, &p("Guidelines", PatchAction::AddBullet, "Be concise")).unwrap();
        assert!(out.contains("- Be friendly"));
        assert!(out.contains("- Be concise"));
    }

    #[test]
    fn add_bullet_is_idempotent() {
        let src = "## Guidelines\n\n- Be concise\n";
        let out1 =
            apply_patch(src, &p("guidelines", PatchAction::AddBullet, "be concise")).unwrap();
        let out2 = apply_patch(
            &out1,
            &p("guidelines", PatchAction::AddBullet, "Be Concise"),
        )
        .unwrap();
        assert_eq!(out1, out2, "duplicate bullet must not append again");
    }

    #[test]
    fn add_bullet_creates_missing_section() {
        let src = "# Agent\n\n## Identity\n\nYou are helpful.\n";
        let out = apply_patch(
            src,
            &p(
                "Style",
                PatchAction::AddBullet,
                "Prefer plain text over emoji",
            ),
        )
        .unwrap();
        assert!(out.contains("## Style"));
        assert!(out.contains("- Prefer plain text over emoji"));
        // Original section preserved.
        assert!(out.contains("You are helpful."));
    }

    #[test]
    fn replace_section_overwrites_body_only() {
        let src = "## Identity\n\nold body line 1\nold body line 2\n\n## Guidelines\n\n- one\n";
        let out = apply_patch(
            src,
            &p(
                "Identity",
                PatchAction::ReplaceSection,
                "fresh identity statement",
            ),
        )
        .unwrap();
        assert!(out.contains("fresh identity statement"));
        assert!(!out.contains("old body line"));
        // The other section is untouched.
        assert!(out.contains("## Guidelines"));
        assert!(out.contains("- one"));
    }

    #[test]
    fn remove_bullet_drops_the_line() {
        let src = "## Guidelines\n\n- keep this\n- drop this\n- and keep this\n";
        let out = apply_patch(
            src,
            &p("guidelines", PatchAction::RemoveBullet, "drop this"),
        )
        .unwrap();
        assert!(out.contains("- keep this"));
        assert!(out.contains("- and keep this"));
        assert!(!out.contains("drop this"));
    }

    #[test]
    fn learned_block_is_preserved_across_edits() {
        let src = "# Agent\n\n## Identity\n\nold\n\n## Learned\n<!-- senclaw:learned:start -->\nauto-content\n<!-- senclaw:learned:end -->\n";
        let out = apply_patch(src, &p("Identity", PatchAction::ReplaceSection, "new")).unwrap();
        assert!(out.contains("auto-content"), "Learned block must survive");
        assert!(out.contains("<!-- senclaw:learned:start -->"));
        assert!(out.contains("new"));
        assert!(!out.contains("old"));
    }

    #[test]
    fn append_line_dedupes() {
        let src = "## Notes\n\nFirst line.\n";
        let out1 = apply_patch(src, &p("Notes", PatchAction::AppendLine, "Second line.")).unwrap();
        let out2 =
            apply_patch(&out1, &p("Notes", PatchAction::AppendLine, "Second line.")).unwrap();
        assert_eq!(out1, out2);
    }

    #[test]
    fn empty_input_creates_section() {
        let out = apply_patch("", &p("Identity", PatchAction::AddBullet, "I am helpful")).unwrap();
        assert!(out.contains("## Identity"));
        assert!(out.contains("- I am helpful"));
    }
}
