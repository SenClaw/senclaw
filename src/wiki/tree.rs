//! Directory tree scanning and rendering.

use std::fs;
use std::path::Path;

use anyhow::Result;

use super::manager::WikiManager;
use super::types::{DirNode, NodeType};

impl WikiManager {
    /// Get the full directory tree (dirs first, files include frontmatter).
    pub fn get_tree(&self) -> Result<Vec<DirNode>> {
        self.scan_dir(&self.wiki_dir.clone(), "")
    }

    /// Plain-text tree representation (for CLI / Agent display).
    pub fn tree_text(&self) -> Result<String> {
        let nodes = self.get_tree()?;
        let mut lines: Vec<String> = Vec::new();
        Self::render_tree(&nodes, "", &mut lines);
        Ok(lines.join("\n"))
    }

    // ===== private helpers =====

    fn scan_dir(&self, abs_dir: &Path, rel_base: &str) -> Result<Vec<DirNode>> {
        let entries = match fs::read_dir(abs_dir) {
            Ok(e) => e,
            Err(_) => return Ok(Vec::new()),
        };

        let mut nodes: Vec<DirNode> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy().to_string();
            if self.excluded.contains(&name_str) || name_str.starts_with('.') {
                continue;
            }
            let rel_path = if rel_base.is_empty() {
                name_str.clone()
            } else {
                format!("{rel_base}/{name_str}")
            };

            if entry.file_type().map_or(false, |t| t.is_dir()) {
                let children = self.scan_dir(&entry.path(), &rel_path)?;
                nodes.push(DirNode {
                    name: name_str,
                    path: rel_path,
                    node_type: NodeType::Dir,
                    children: Some(children),
                    frontmatter: None,
                });
            } else if entry.file_type().map_or(false, |t| t.is_file()) && name_str.ends_with(".md")
            {
                let fm = fs::read_to_string(entry.path())
                    .ok()
                    .map(|c| Self::parse_frontmatter(&c).0);
                nodes.push(DirNode {
                    name: name_str,
                    path: rel_path,
                    node_type: NodeType::File,
                    children: None,
                    frontmatter: fm,
                });
            }
        }

        nodes.sort_by(|a, b| match (&a.node_type, &b.node_type) {
            (NodeType::Dir, NodeType::File) => std::cmp::Ordering::Less,
            (NodeType::File, NodeType::Dir) => std::cmp::Ordering::Greater,
            _ => a.name.cmp(&b.name),
        });
        Ok(nodes)
    }

    pub(crate) fn collect_md_files(&self) -> Vec<(String, String)> {
        let mut files = Vec::new();
        self.collect_md_files_recursive("", &mut files);
        files
    }

    fn collect_md_files_recursive(&self, rel_base: &str, files: &mut Vec<(String, String)>) {
        let abs_dir = if rel_base.is_empty() {
            self.wiki_dir.clone()
        } else {
            self.wiki_dir.join(rel_base)
        };
        let entries = match fs::read_dir(&abs_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy().to_string();
            if self.excluded.contains(&name_str) || name_str.starts_with('.') {
                continue;
            }
            let child_rel = if rel_base.is_empty() {
                name_str.clone()
            } else {
                format!("{rel_base}/{name_str}")
            };
            if entry.file_type().map_or(false, |t| t.is_dir()) {
                self.collect_md_files_recursive(&child_rel, files);
            } else if name_str.ends_with(".md") {
                if let Ok(content) = fs::read_to_string(entry.path()) {
                    files.push((child_rel, content));
                }
            }
        }
    }

    pub(crate) fn count_dirs(&self, dir: &Path) -> usize {
        let mut count = 0;
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return 0,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy().to_string();
            if self.excluded.contains(name_str.as_str()) || name_str.starts_with('.') {
                continue;
            }
            if entry.file_type().map_or(false, |t| t.is_dir()) {
                count += 1;
                count += self.count_dirs(&entry.path());
            }
        }
        count
    }

    fn render_tree(nodes: &[DirNode], indent: &str, lines: &mut Vec<String>) {
        for node in nodes {
            if node.node_type == NodeType::Dir {
                lines.push(format!("{indent}{}/", node.name));
                if let Some(ref children) = node.children {
                    Self::render_tree(children, &format!("{indent}  "), lines);
                }
            } else {
                lines.push(format!("{indent}{}", node.name));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiki::manager::WikiManager;
    use crate::wiki::types::{DirNode, NodeType};
    use std::path::Path;

    fn make_manager(tmp: &Path) -> WikiManager {
        WikiManager::new(tmp.join("wiki"))
    }

    #[test]
    fn test_tree_text_empty() {
        let tmp = std::env::temp_dir().join(format!("wiki-empty-{}", uuid::Uuid::new_v4()));
        let mgr = make_manager(&tmp);
        fs::create_dir_all(&mgr.wiki_dir).unwrap();
        let text = mgr.tree_text().unwrap();
        assert_eq!(text, "");
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_render_tree() {
        let nodes = vec![
            DirNode {
                name: "inbox".to_string(),
                path: "inbox".to_string(),
                node_type: NodeType::Dir,
                children: Some(vec![DirNode {
                    name: "note.md".to_string(),
                    path: "inbox/note.md".to_string(),
                    node_type: NodeType::File,
                    children: None,
                    frontmatter: None,
                }]),
                frontmatter: None,
            },
            DirNode {
                name: "README.md".to_string(),
                path: "README.md".to_string(),
                node_type: NodeType::File,
                children: None,
                frontmatter: None,
            },
        ];
        let mut lines = Vec::new();
        WikiManager::render_tree(&nodes, "", &mut lines);
        assert_eq!(lines, vec!["inbox/", "  note.md", "README.md"]);
    }
}
