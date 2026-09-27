//! Which definitions matter most — Aider's recipe.
//!
//! Files are nodes. A file that *references* a name has an edge to every
//! file that *defines* it, weight split across the definers. PageRank over
//! that graph (personalized toward the files the conversation is about) ranks
//! files; a file's rank flows to its definitions in proportion to how often
//! each is referenced from elsewhere. The result is a list of definitions
//! to show, best first, that the renderer cuts to a token budget.

use std::collections::HashMap;

use super::Index;

const DAMPING: f64 = 0.85;
const ITERATIONS: usize = 40;
/// A name defined in more files than this is treated as ambiguous and
/// contributes no edges.
const MAX_DEFINERS: usize = 3;

/// One ranked definition: file, position in that file's `defs`, score.
pub struct RankedDef {
    pub file: String,
    pub def_idx: usize,
    pub score: f64,
}

pub fn rank(index: &Index, focus: &[String]) -> Vec<RankedDef> {
    let files: Vec<&String> = index.files.keys().collect();
    let n = files.len();
    if n == 0 {
        return Vec::new();
    }
    let idx_of: HashMap<&str, usize> = files.iter().enumerate().map(|(i, f)| (f.as_str(), i)).collect();

    // name → files defining it
    let mut definers: HashMap<&str, Vec<usize>> = HashMap::new();
    for (f, entry) in &index.files {
        let i = idx_of[f.as_str()];
        for d in &entry.defs {
            let v = definers.entry(d.name.as_str()).or_default();
            if !v.contains(&i) {
                v.push(i);
            }
        }
    }

    // out-edges per file: (target, weight); and inbound reference counts per (file, name)
    let mut out_edges: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    let mut inbound_refs: HashMap<(usize, &str), f64> = HashMap::new();
    for (f, entry) in &index.files {
        let i = idx_of[f.as_str()];
        // Count references per name once per file so a name used 200 times in
        // one file does not outweigh one used in 20 files.
        let mut per_name: HashMap<&str, usize> = HashMap::new();
        for r in &entry.refs {
            *per_name.entry(r.name.as_str()).or_default() += 1;
        }
        for (name, count) in per_name {
            // Names defined all over the tree (`Error`, `new`, `Result`) say
            // nothing about which file depends on which; short names are
            // mostly noise from the type-mention patterns.
            if name.len() < 3 {
                continue;
            }
            let Some(targets) = definers.get(name) else { continue };
            if targets.len() > MAX_DEFINERS {
                continue;
            }
            let others: Vec<usize> = targets.iter().copied().filter(|t| *t != i).collect();
            if others.is_empty() {
                continue;
            }
            // Aider damps very common names: sqrt of the count so hot names
            // still matter but do not dominate.
            let w = (count as f64).sqrt() / others.len() as f64;
            for t in others {
                out_edges[i].push((t, w));
                *inbound_refs.entry((t, name)).or_default() += w;
            }
        }
    }

    // Personalization: focus files (and files they reference) get the restart
    // mass; with no focus every file shares it equally.
    let mut personal = vec![0.0f64; n];
    let mut any_focus = false;
    for f in focus {
        if let Some(&i) = idx_of.get(f.as_str()) {
            personal[i] += 1.0;
            any_focus = true;
        }
    }
    if !any_focus {
        personal.iter_mut().for_each(|p| *p = 1.0);
    }
    let total: f64 = personal.iter().sum();
    personal.iter_mut().for_each(|p| *p /= total);

    let out_weight: Vec<f64> = out_edges.iter().map(|e| e.iter().map(|(_, w)| w).sum()).collect();
    let mut rank = personal.clone();
    for _ in 0..ITERATIONS {
        let mut next = vec![0.0f64; n];
        let mut dangling = 0.0;
        for i in 0..n {
            if out_weight[i] <= 0.0 {
                dangling += rank[i];
                continue;
            }
            for (t, w) in &out_edges[i] {
                next[*t] += rank[i] * (w / out_weight[i]);
            }
        }
        for i in 0..n {
            next[i] = (1.0 - DAMPING) * personal[i] + DAMPING * (next[i] + dangling * personal[i]);
        }
        rank = next;
    }

    let mut ranked = Vec::new();
    for (f, entry) in &index.files {
        let i = idx_of[f.as_str()];
        if entry.defs.is_empty() {
            continue;
        }
        let per_def = rank[i] / entry.defs.len() as f64;
        for (di, d) in entry.defs.iter().enumerate() {
            let inbound = inbound_refs.get(&(i, d.name.as_str())).copied().unwrap_or(0.0);
            ranked.push(RankedDef {
                file: f.clone(),
                def_idx: di,
                score: per_def * (1.0 + inbound),
            });
        }
    }
    ranked.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_map::{Def, FileEntry, Reference};

    fn entry(defs: &[&str], refs: &[&str]) -> FileEntry {
        FileEntry {
            mtime: 0,
            size: 0,
            lang: "rust".into(),
            defs: defs
                .iter()
                .map(|n| Def { name: n.to_string(), kind: "function".into(), line: 1, end_line: 1, sig: format!("fn {n}()") })
                .collect(),
            refs: refs.iter().map(|n| Reference { name: n.to_string(), line: 2 }).collect(),
        }
    }

    #[test]
    fn a_widely_referenced_definition_ranks_first() {
        let mut index = Index::new("/w".into());
        // util.rs defines `helper`, referenced by three files; lonely.rs
        // defines `alone`, referenced by nobody.
        index.files.insert("util.rs".into(), entry(&["helper"], &[]));
        index.files.insert("lonely.rs".into(), entry(&["alone"], &[]));
        for f in ["a.rs", "b.rs", "c.rs"] {
            index.files.insert(f.into(), entry(&[&format!("{f}_main")], &["helper"]));
        }
        let ranked = rank(&index, &[]);
        assert_eq!(ranked[0].file, "util.rs");
        let alone_pos = ranked.iter().position(|r| r.file == "lonely.rs").unwrap();
        assert!(alone_pos > 0);
    }

    #[test]
    fn focus_pulls_its_dependencies_up() {
        let mut index = Index::new("/w".into());
        index.files.insert("x.rs".into(), entry(&["x_fn"], &[]));
        index.files.insert("y.rs".into(), entry(&["y_fn"], &[]));
        index.files.insert("main.rs".into(), entry(&["main"], &["x_fn"]));
        index.files.insert("other.rs".into(), entry(&["o"], &["y_fn"]));
        let ranked = rank(&index, &["main.rs".into()]);
        let x = ranked.iter().position(|r| r.file == "x.rs").unwrap();
        let y = ranked.iter().position(|r| r.file == "y.rs").unwrap();
        assert!(x < y, "the file main.rs depends on must outrank an unrelated one");
    }
}
