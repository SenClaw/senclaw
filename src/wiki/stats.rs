//! Wiki statistics: category breakdown, tag distribution, recent files.

use std::collections::HashMap;

use anyhow::Result;

use super::manager::WikiManager;
use super::types::{CategoryStat, RecentFile, TagEntry, TagStat, WikiStats};

impl WikiManager {
    /// Aggregate stats: file count by category, tag distribution, recent files.
    pub fn get_stats(&self) -> Result<WikiStats> {
        let mut by_category: HashMap<String, (usize, String)> = HashMap::new();
        let mut by_tag: HashMap<String, usize> = HashMap::new();
        let mut all_files: Vec<(String, String, String)> = Vec::new();

        for (rel_path, content) in self.collect_md_files() {
            let (fm, _) = Self::parse_frontmatter(&content);
            let title = Self::extract_title(&content, &rel_path);
            let updated = fm.updated.clone();

            let top_dir = rel_path
                .split('/')
                .next()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "(root)".to_string());
            let cat = by_category.entry(top_dir).or_insert((0, String::new()));
            cat.0 += 1;
            if updated > cat.1 {
                cat.1 = updated.clone();
            }

            for tag in &fm.tags {
                *by_tag.entry(tag.clone()).or_insert(0) += 1;
            }

            all_files.push((rel_path, title, updated));
        }

        all_files.sort_by(|a, b| b.2.cmp(&a.2));

        let mut cat_stats: Vec<CategoryStat> = by_category
            .into_iter()
            .map(|(dir, (count, last_updated))| CategoryStat {
                dir,
                count,
                last_updated,
            })
            .collect();
        cat_stats.sort_by(|a, b| b.count.cmp(&a.count));

        let mut tag_stats: Vec<TagStat> = by_tag
            .into_iter()
            .map(|(tag, count)| TagStat { tag, count })
            .collect();
        tag_stats.sort_by(|a, b| b.count.cmp(&a.count));

        Ok(WikiStats {
            total_files: all_files.len(),
            total_dirs: self.count_dirs(&self.wiki_dir.clone()),
            by_category: cat_stats,
            by_tag: tag_stats,
            recent_files: all_files
                .into_iter()
                .take(10)
                .map(|(path, title, updated)| RecentFile {
                    path,
                    title,
                    updated,
                })
                .collect(),
        })
    }

    /// All tags with occurrence counts, sorted by frequency descending.
    pub fn get_tags(&self) -> Vec<TagEntry> {
        let mut by_tag: HashMap<String, usize> = HashMap::new();
        for (_rel_path, content) in self.collect_md_files() {
            let (fm, _) = Self::parse_frontmatter(&content);
            for tag in &fm.tags {
                *by_tag.entry(tag.clone()).or_insert(0) += 1;
            }
        }
        let mut entries: Vec<TagEntry> = by_tag
            .into_iter()
            .map(|(name, count)| TagEntry { name, count })
            .collect();
        entries.sort_by(|a, b| b.count.cmp(&a.count));
        entries
    }
}
