//! Full-text search across wiki markdown files.
//!
//! Builds a transient in-memory SQLite FTS5 index from the markdown files on
//! each query. The wiki content lives in git-backed files that can change on
//! disk (git pull, manual edits), so a freshly-built index is always correct —
//! no persistent index to drift out of sync. For a personal-scale wiki (tens to
//! a few hundred small files) the build cost is negligible.

use anyhow::Result;
use rusqlite::Connection;

use super::manager::WikiManager;
use super::types::SearchResult;

impl WikiManager {
    /// Search wiki documents by full text (body, title, filename, tags).
    /// When the query is empty, returns all documents (useful for tag-only
    /// filtering), sorted by last-updated.
    pub fn search(
        &self,
        query: &str,
        filter_tags: Option<&[String]>,
        limit: Option<usize>,
    ) -> Result<Vec<SearchResult>> {
        let limit = limit.unwrap_or(20);
        let filter_tags = filter_tags.unwrap_or(&[]);
        let files = self.collect_md_files();

        // Empty query → list everything (optionally tag-filtered), newest first.
        if query.trim().is_empty() {
            let mut results: Vec<SearchResult> = Vec::new();
            for (rel_path, content) in &files {
                let (fm, _) = Self::parse_frontmatter(content);
                if !tags_match(&fm.tags, filter_tags) {
                    continue;
                }
                results.push(SearchResult {
                    path: rel_path.clone(),
                    title: Self::extract_title(content, rel_path),
                    tags: fm.tags.clone(),
                    updated: fm.updated.clone(),
                    snippet: String::new(),
                });
            }
            results.sort_by(|a, b| b.updated.cmp(&a.updated));
            results.truncate(limit);
            return Ok(results);
        }

        // Turn the user's query into an FTS5 prefix-AND match expression.
        // e.g. "client app" → `"client"* "app"*`. Non-alphanumeric chars are
        // treated as token separators so punctuation can't break the syntax.
        let match_expr = build_match_expr(query);
        if match_expr.is_empty() {
            return Ok(Vec::new());
        }

        // Build the in-memory index.
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE VIRTUAL TABLE docs USING fts5(\
                 path UNINDEXED, title, body, tags, \
                 tokenize = 'unicode61 remove_diacritics 2');",
        )?;

        {
            let mut stmt =
                conn.prepare("INSERT INTO docs(path, title, body, tags) VALUES (?1, ?2, ?3, ?4)")?;
            for (rel_path, content) in &files {
                let (fm, body) = Self::parse_frontmatter(content);
                let title = Self::extract_title(content, rel_path);
                let filename = rel_path.rsplit('/').next().unwrap_or(rel_path);
                // Fold filename into the title column so path matches still hit.
                let title_col = format!("{title}\n{filename}");
                stmt.execute(rusqlite::params![
                    rel_path,
                    title_col,
                    body,
                    fm.tags.join(" "),
                ])?;
            }
        }

        // Rank by bm25; snippet from the body column (index 2).
        let mut stmt = conn.prepare(
            "SELECT path, \
                    snippet(docs, 2, '', '', '…', 12) AS snip \
             FROM docs WHERE docs MATCH ?1 ORDER BY bm25(docs) LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![match_expr, limit as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;

        // Re-read metadata (title/tags/updated) for the matched paths.
        let mut results: Vec<SearchResult> = Vec::new();
        for row in rows {
            let (path, snip) = row?;
            let content = files
                .iter()
                .find(|(p, _)| p == &path)
                .map(|(_, c)| c.clone())
                .unwrap_or_default();
            let (fm, _) = Self::parse_frontmatter(&content);
            if !tags_match(&fm.tags, filter_tags) {
                continue;
            }
            results.push(SearchResult {
                path: path.clone(),
                title: Self::extract_title(&content, &path),
                tags: fm.tags.clone(),
                updated: fm.updated.clone(),
                snippet: snip.split_whitespace().collect::<Vec<_>>().join(" "),
            });
        }
        Ok(results)
    }
}

/// True if `tags` satisfies the tag filter (empty filter matches everything).
fn tags_match(tags: &[String], filter: &[String]) -> bool {
    if filter.is_empty() {
        return true;
    }
    let lower: Vec<String> = tags.iter().map(|t| t.to_lowercase()).collect();
    filter.iter().any(|t| lower.contains(&t.to_lowercase()))
}

/// Build an FTS5 MATCH expression: each whitespace/punctuation-delimited token
/// becomes a quoted prefix term, ANDed together. Returns "" if no usable token.
fn build_match_expr(query: &str) -> String {
    let terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        // Quote each term (doubling any stray quote) and add a prefix `*`.
        .map(|t| format!("\"{}\"*", t.replace('"', "\"\"")))
        .collect();
    terms.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn mgr_with(files: &[(&str, &str)]) -> (WikiManager, std::path::PathBuf) {
        let tmp = std::env::temp_dir().join(format!("wiki-fts-{}", uuid::Uuid::new_v4()));
        let wiki = tmp.join("wiki");
        for (path, content) in files {
            let abs = wiki.join(path);
            fs::create_dir_all(abs.parent().unwrap()).unwrap();
            fs::write(abs, content).unwrap();
        }
        (WikiManager::new(wiki), tmp)
    }

    #[test]
    fn matches_body_content() {
        let (mgr, tmp) = mgr_with(&[
            (
                "notes/meeting.md",
                "# Sync\n\nDiscussed the new client onboarding flow.",
            ),
            ("notes/other.md", "# Other\n\nUnrelated content here."),
        ]);
        let res = mgr.search("client", None, None).unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].path, "notes/meeting.md");
        assert!(res[0].snippet.to_lowercase().contains("client"));
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn prefix_and_title_match() {
        let (mgr, tmp) = mgr_with(&[(
            "client-guide.md",
            "# Client Guide\n\nHow to handle accounts.",
        )]);
        // Prefix: "clien" should still match "client".
        assert_eq!(mgr.search("clien", None, None).unwrap().len(), 1);
        // Filename match.
        assert_eq!(mgr.search("guide", None, None).unwrap().len(), 1);
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn empty_query_lists_all() {
        let (mgr, tmp) = mgr_with(&[("a.md", "# A\nx"), ("b.md", "# B\ny")]);
        assert_eq!(mgr.search("", None, None).unwrap().len(), 2);
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn punctuation_only_query_is_safe() {
        let (mgr, tmp) = mgr_with(&[("a.md", "# A\nhello")]);
        assert_eq!(mgr.search("!!!", None, None).unwrap().len(), 0);
        fs::remove_dir_all(&tmp).ok();
    }
}
