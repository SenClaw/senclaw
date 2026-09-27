//! YAML frontmatter parsing, injection, and title extraction.

use std::path::Path;

use regex::Regex;

use super::manager::WikiManager;
use super::types::Frontmatter;

impl WikiManager {
    /// Parse YAML frontmatter block from content.
    /// Returns `(frontmatter, body_without_frontmatter)`.
    pub(crate) fn parse_frontmatter(content: &str) -> (Frontmatter, String) {
        let default_fm = Frontmatter {
            created: String::new(),
            updated: String::new(),
            tags: Vec::new(),
            source: "manual".to_string(),
        };
        if !content.starts_with("---") {
            return (default_fm, content.to_string());
        }
        let end = content[3..].find("\n---");
        let end = match end {
            Some(i) => i,
            None => return (default_fm, content.to_string()),
        };
        let yaml_block = &content[4..3 + end];
        let body = content[3 + end + 4..].trim_start_matches('\n').to_string();

        let mut fm = default_fm.clone();
        for line in yaml_block.lines() {
            let colon = match line.find(':') {
                Some(i) => i,
                None => continue,
            };
            let key = line[..colon].trim();
            let val = line[colon + 1..].trim();

            match key {
                "created" | "updated" | "source" => {
                    let val = val.to_string();
                    match key {
                        "created" => fm.created = val,
                        "updated" => fm.updated = val,
                        "source" => fm.source = val,
                        _ => {}
                    }
                }
                "tags" => {
                    let tag_str = val.trim_start_matches('[').trim_end_matches(']');
                    fm.tags = tag_str
                        .split(',')
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect();
                }
                _ => {}
            }
        }
        (fm, body)
    }

    /// Rebuild content with YAML frontmatter prepended.
    pub(crate) fn inject_frontmatter(content: &str, fm: &Frontmatter) -> String {
        let (_, body) = Self::parse_frontmatter(content);
        let tags = if fm.tags.is_empty() {
            "[]".to_string()
        } else {
            format!("[{}]", fm.tags.join(", "))
        };
        format!(
            "---\ncreated: {}\nupdated: {}\ntags: {tags}\nsource: {}\n---\n\n{body}",
            fm.created, fm.updated, fm.source,
        )
    }

    /// Extract H1 title from content, falling back to filename stem.
    pub(crate) fn extract_title(content: &str, rel_path: &str) -> String {
        let (_, body) = Self::parse_frontmatter(content);
        let re = Regex::new(r"^#\s+(.+)").unwrap();
        if let Some(cap) = re.captures(&body) {
            return cap[1].trim().to_string();
        }
        Path::new(rel_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(rel_path)
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use crate::wiki::manager::WikiManager;
    use crate::wiki::types::Frontmatter;

    #[test]
    fn test_parse_frontmatter_basic() {
        let content = "---\ncreated: 2024-01-01\nupdated: 2024-01-02\ntags: [rust, wiki]\nsource: agent\n---\n\n# Title\n\nBody text.";
        let (fm, body) = WikiManager::parse_frontmatter(content);
        assert_eq!(fm.created, "2024-01-01");
        assert_eq!(fm.updated, "2024-01-02");
        assert_eq!(fm.tags, vec!["rust", "wiki"]);
        assert_eq!(fm.source, "agent");
        assert_eq!(body, "# Title\n\nBody text.");
    }

    #[test]
    fn test_parse_frontmatter_no_fm() {
        let content = "# No frontmatter\n\nJust content.";
        let (fm, body) = WikiManager::parse_frontmatter(content);
        assert_eq!(fm.source, "manual");
        assert!(fm.tags.is_empty());
        assert_eq!(body, content);
    }

    #[test]
    fn test_inject_frontmatter() {
        let content = "# Old title\n\nOld body.";
        let fm = Frontmatter {
            created: "2024-01-01".to_string(),
            updated: "2024-01-02".to_string(),
            tags: vec!["demo".to_string()],
            source: "agent".to_string(),
        };
        let result = WikiManager::inject_frontmatter(content, &fm);
        assert!(result.starts_with("---\n"));
        assert!(result.contains("created: 2024-01-01"));
        assert!(result.contains("tags: [demo]"));
        assert!(result.contains("# Old title"));
    }

    #[test]
    fn test_extract_title() {
        let content = "---\ntitle: ignored\n---\n\n# The Real Title\n\nBody.";
        assert_eq!(
            WikiManager::extract_title(content, "some/path.md"),
            "The Real Title"
        );
    }

    #[test]
    fn test_extract_title_fallback() {
        let content = "No H1 here.";
        assert_eq!(
            WikiManager::extract_title(content, "some/my-page.md"),
            "my-page"
        );
    }
}
