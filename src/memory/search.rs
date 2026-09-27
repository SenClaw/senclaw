//! Shared memory retrieval. Mirrors `src-old/memory/MemorySearch.ts`.
//!
//! Reads from:
//!   - `memory/memories.json` — structured memory entries
//!   - `memory/YYYY-MM-DD.md` — daily conversation logs
//!
//! Used by both the MCP memory-server and AgentPool pre-retrieval.

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

const MEMORIES_FILE: &str = "memories.json";
pub const MAX_MEMORIES: usize = 100;
const DAILY_SEARCH_DAYS: usize = 7;

static DATE_FILE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{4}-\d{2}-\d{2})\.md$").unwrap());

fn split_by_headings(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, part) in raw.split("\n## ").enumerate() {
        if i == 0 {
            if part.starts_with("## ") {
                out.push(part.to_string());
            }
        } else {
            out.push(format!("## {part}"));
        }
    }
    out
}

// ===== Data structures =====

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub content: String,
    pub created: String,
    pub hits: u32,
    pub last_hit: String,
}

#[derive(Debug, Clone)]
pub struct MemorySearchResult {
    pub id: String,
    pub content: String,
    pub created: String,
    pub hits: u32,
}

#[derive(Debug, Clone)]
pub struct DailyLogResult {
    pub date: String,
    pub section: String,
}

// ===== memories.json read/write =====

pub fn read_memories(memory_dir: &Path) -> Vec<MemoryEntry> {
    let file_path = memory_dir.join(MEMORIES_FILE);
    match fs::read_to_string(&file_path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

pub fn write_memories(memory_dir: &Path, entries: &[MemoryEntry]) {
    let _ = fs::create_dir_all(memory_dir);
    let file_path = memory_dir.join(MEMORIES_FILE);
    if let Ok(json) = serde_json::to_string_pretty(entries) {
        let _ = fs::write(&file_path, json);
    }
}

// ===== Eviction =====

fn retention_score(entry: &MemoryEntry) -> f64 {
    let elapsed = match chrono::DateTime::parse_from_rfc3339(&entry.last_hit) {
        Ok(dt) => {
            let now = chrono::Utc::now();
            (now - dt.with_timezone(&chrono::Utc)).num_seconds() as f64 / 86_400.0
        }
        Err(_) => 365.0,
    };
    entry.hits as f64 / (elapsed + 1.0)
}

pub fn evict_if_needed(entries: &[MemoryEntry]) -> Vec<MemoryEntry> {
    if entries.len() <= MAX_MEMORIES {
        return entries.to_vec();
    }
    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| {
        retention_score(b)
            .partial_cmp(&retention_score(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    sorted.truncate(MAX_MEMORIES);
    sorted
}

// ===== Tokenization =====

fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(
            &[
                ' ', '\t', '\n', ',', '.', '!', '?', ';', ':', '，', '。', '！', '？', '、',
            ][..],
        )
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

fn matches_tokens(text: &str, tokens: &[String]) -> bool {
    let lower = text.to_lowercase();
    tokens.iter().any(|t| lower.contains(t.as_str()))
}

// ===== memories.json search =====

pub fn search_memories(memory_dir: &Path, query: &str, top_n: usize) -> Vec<MemorySearchResult> {
    let tokens = tokenize(query);
    if tokens.is_empty() {
        return Vec::new();
    }

    let mut entries = read_memories(memory_dir);
    let mut hit_ids: HashSet<String> = HashSet::new();
    let mut matched: Vec<MemoryEntry> = Vec::new();

    for e in &entries {
        if matches_tokens(&e.content, &tokens) {
            hit_ids.insert(e.id.clone());
            matched.push(e.clone());
        }
    }

    if matched.is_empty() {
        return Vec::new();
    }

    let now = chrono::Utc::now().to_rfc3339();
    for e in entries.iter_mut() {
        if hit_ids.contains(&e.id) {
            e.hits += 1;
            e.last_hit = now.clone();
        }
    }
    write_memories(memory_dir, &entries);

    matched.sort_by(|a, b| b.hits.cmp(&a.hits));
    matched.truncate(top_n);
    matched
        .into_iter()
        .map(|e| MemorySearchResult {
            id: e.id,
            content: e.content,
            created: e.created,
            hits: e.hits,
        })
        .collect()
}

// ===== Daily log search =====

pub fn search_daily_logs(memory_dir: &Path, query: &str, top_n: usize) -> Vec<DailyLogResult> {
    let tokens = tokenize(query);
    if tokens.is_empty() {
        return Vec::new();
    }

    let mut results: Vec<DailyLogResult> = Vec::new();

    let dir_iter = match fs::read_dir(memory_dir) {
        Ok(d) => d,
        Err(_) => return results,
    };

    let mut date_files: Vec<String> = Vec::new();
    for entry in dir_iter.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().to_string();
        if DATE_FILE_RE.is_match(&name) {
            date_files.push(name);
        }
    }
    date_files.sort();
    let recent: Vec<&String> = date_files.iter().rev().take(DAILY_SEARCH_DAYS).collect();

    'outer: for file in recent.iter().rev() {
        let caps = match DATE_FILE_RE.captures(file) {
            Some(c) => c,
            None => continue,
        };
        let date = caps.get(1).unwrap().as_str().to_string();

        let raw = match fs::read_to_string(memory_dir.join(file)) {
            Ok(s) => s,
            Err(_) => continue,
        };

        for sec in &split_by_headings(&raw) {
            if !sec.starts_with("## ") {
                continue;
            }
            if matches_tokens(sec, &tokens) {
                results.push(DailyLogResult {
                    date: date.clone(),
                    section: sec.trim().to_string(),
                });
                if results.len() >= top_n {
                    break 'outer;
                }
            }
        }
    }

    results
}

// ===== Formatting =====

pub fn format_memory_context(
    memories: &[MemorySearchResult],
    daily_logs: &[DailyLogResult],
) -> String {
    if memories.is_empty() && daily_logs.is_empty() {
        return String::new();
    }

    let mut out = String::new();

    if !memories.is_empty() {
        out.push_str("Relevant memories:\n");
        for m in memories {
            let date = if m.created.len() >= 10 {
                &m.created[..10]
            } else {
                &m.created
            };
            out.push_str(&format!("- [{}] {}\n", date, m.content));
        }
    }

    if !daily_logs.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("Recent activity:\n");
        for d in daily_logs {
            out.push_str(&format!("[{}]\n{}\n\n", d.date, d.section));
        }
    }

    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(id: &str, content: &str, hits: u32, days_ago: f64) -> MemoryEntry {
        let ts = chrono::Utc::now() - chrono::Duration::seconds((days_ago * 86_400.0) as i64);
        MemoryEntry {
            id: id.to_string(),
            content: content.to_string(),
            created: ts.to_rfc3339(),
            hits,
            last_hit: ts.to_rfc3339(),
        }
    }

    #[test]
    fn test_read_memories_empty_dir() {
        let dir =
            std::env::temp_dir().join(format!("test-memories-empty-{}", uuid::Uuid::new_v4()));
        let entries = read_memories(&dir);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_write_and_read_memories() {
        let dir = std::env::temp_dir().join(format!("test-memories-rw-{}", uuid::Uuid::new_v4()));
        let entries = vec![make_entry("1", "hello world", 5, 0.0)];
        write_memories(&dir, &entries);
        let read = read_memories(&dir);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].id, "1");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_eviction_under_limit() {
        let entries: Vec<MemoryEntry> = (0..50)
            .map(|i| make_entry(&i.to_string(), "test", 1, i as f64))
            .collect();
        assert_eq!(evict_if_needed(&entries).len(), 50);
    }

    #[test]
    fn test_eviction_over_limit() {
        let entries: Vec<MemoryEntry> = (0..150)
            .map(|i| make_entry(&i.to_string(), "test", 1, i as f64))
            .collect();
        assert_eq!(evict_if_needed(&entries).len(), MAX_MEMORIES);
    }

    #[test]
    fn test_eviction_keeps_high_score() {
        let mut entries = vec![
            make_entry("low", "test", 1, 30.0),
            make_entry("high", "test", 100, 0.0),
        ];
        for i in 0..148 {
            entries.push(make_entry(&format!("filler-{i}"), "test", 1, 365.0));
        }
        let retained = evict_if_needed(&entries);
        let ids: Vec<&str> = retained.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&"high"), "high-score entry should be kept");
    }

    #[test]
    fn test_tokenize() {
        let tokens = tokenize("hello world");
        assert!(tokens.contains(&"hello".to_string()));
        assert!(tokens.contains(&"world".to_string()));
    }

    #[test]
    fn test_matches_tokens() {
        let tokens = tokenize("hello world");
        assert!(matches_tokens("hello world", &tokens));
        assert!(matches_tokens("hello", &tokens));
        assert!(!matches_tokens("foo bar", &tokens));
    }

    #[test]
    fn test_search_memories_no_match() {
        let dir = std::env::temp_dir().join(format!("test-memsearch-{}", uuid::Uuid::new_v4()));
        write_memories(&dir, &[make_entry("1", "hello world", 0, 0.0)]);
        let results = search_memories(&dir, "xyzq", 5);
        assert!(results.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_search_memories_match() {
        let dir = std::env::temp_dir().join(format!("test-memsearch-{}", uuid::Uuid::new_v4()));
        write_memories(&dir, &[make_entry("1", "hello world", 0, 0.0)]);
        let results = search_memories(&dir, "hello", 5);
        assert!(!results.is_empty());
        assert_eq!(results[0].id, "1");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_format_empty() {
        assert_eq!(format_memory_context(&[], &[]), "");
    }

    #[test]
    fn test_format_memories() {
        let memories = vec![MemorySearchResult {
            id: "1".into(),
            content: "test memory".into(),
            created: "2026-01-15T00:00:00Z".into(),
            hits: 3,
        }];
        let out = format_memory_context(&memories, &[]);
        assert!(out.contains("Relevant memories:"));
        assert!(out.contains("test memory"));
    }

    #[test]
    fn test_format_daily_logs() {
        let logs = vec![DailyLogResult {
            date: "2026-01-15".into(),
            section: "## 14:30\nDid something".into(),
        }];
        let out = format_memory_context(&[], &logs);
        assert!(out.contains("Recent activity:"));
        assert!(out.contains("2026-01-15"));
    }
}
