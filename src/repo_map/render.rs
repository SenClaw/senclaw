//! Ranked definitions → the text block the model sees.
//!
//! ```text
//! src/agent/pool.rs:
//! │ 41│ pub struct AgentPool {
//! │120│ pub fn run_agent(&self, jid: &str) -> Result<()> {
//! ```
//! Files appear in path order once selected; within a file, definitions in
//! line order. Selection is by rank until the token budget is spent, so the
//! block is the best `budget` tokens of the repository, not its first ones.

use std::collections::BTreeMap;

use super::rank::RankedDef;
use super::Index;

/// Rough tokens for a line of code: ~4 chars per token, plus the line's
/// newline and gutter.
fn line_tokens(s: &str) -> usize {
    s.len() / 4 + 2
}

pub fn render(index: &Index, ranked: &[RankedDef], budget_tokens: usize) -> String {
    if budget_tokens == 0 || ranked.is_empty() {
        return String::new();
    }
    let mut chosen: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    let mut spent = 0usize;
    for r in ranked {
        let entry = &index.files[&r.file];
        let d = &entry.defs[r.def_idx];
        let cost = line_tokens(&d.sig) + if chosen.contains_key(r.file.as_str()) { 0 } else { line_tokens(&r.file) };
        if spent + cost > budget_tokens {
            if spent > budget_tokens * 9 / 10 {
                break;
            }
            continue;
        }
        spent += cost;
        chosen.entry(r.file.as_str()).or_default().push(r.def_idx);
    }
    let mut out = String::new();
    for (file, mut idxs) in chosen {
        idxs.sort_unstable();
        idxs.dedup();
        let entry = &index.files[file];
        out.push_str(file);
        out.push_str(":\n");
        for i in idxs {
            let d = &entry.defs[i];
            out.push_str(&format!("│{:>4}│ {}\n", d.line, d.sig));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_map::rank::rank;
    use crate::repo_map::{Def, FileEntry};

    #[test]
    fn budget_bounds_output_and_files_are_grouped() {
        let mut index = Index::new("/w".into());
        for f in 0..50 {
            let defs = (0..10)
                .map(|i| Def { name: format!("f{f}_{i}"), kind: "function".into(), line: i + 1, end_line: i + 1, sig: format!("pub fn f{f}_{i}(a: u32, b: u32) -> u32 {{") })
                .collect();
            index.files.insert(format!("src/m{f:02}.rs"), FileEntry { mtime: 0, size: 0, lang: "rust".into(), defs, refs: vec![] });
        }
        let ranked = rank(&index, &[]);
        let text = render(&index, &ranked, 300);
        assert!(!text.is_empty());
        assert!(text.len() / 4 <= 330, "≈ budget, got {} chars", text.len());
        assert!(text.contains("│   1│ pub fn"));
        // Every file header is followed by at least one definition line.
        for block in text.split("\n").filter(|l| l.ends_with(':')) {
            assert!(block.starts_with("src/"));
        }
    }
}
