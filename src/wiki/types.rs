use serde::{Deserialize, Serialize};

// ===== Core types =====

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Frontmatter {
    pub created: String,
    pub updated: String,
    pub tags: Vec<String>,
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DirNode {
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub node_type: NodeType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<DirNode>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frontmatter: Option<Frontmatter>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum NodeType {
    Dir,
    File,
}

#[derive(Debug, Clone)]
pub struct WikiDoc {
    pub path: String,
    pub content: String,
    pub frontmatter: Frontmatter,
    pub git_log: Vec<GitCommit>,
}

// ===== Search & stats types =====

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub path: String,
    pub title: String,
    pub tags: Vec<String>,
    pub updated: String,
    /// Body excerpt around the match (FTS5 snippet), empty for tag-only listing.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WikiStats {
    #[serde(rename = "totalFiles")]
    pub total_files: usize,
    #[serde(rename = "totalDirs")]
    pub total_dirs: usize,
    #[serde(rename = "byCategory")]
    pub by_category: Vec<CategoryStat>,
    #[serde(rename = "byTag")]
    pub by_tag: Vec<TagStat>,
    #[serde(rename = "recentFiles")]
    pub recent_files: Vec<RecentFile>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CategoryStat {
    pub dir: String,
    pub count: usize,
    #[serde(rename = "lastUpdated")]
    pub last_updated: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TagStat {
    pub tag: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecentFile {
    pub path: String,
    pub title: String,
    pub updated: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GitCommit {
    pub hash: String,
    pub date: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TagEntry {
    pub name: String,
    pub count: usize,
}
