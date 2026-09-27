//! Memory system core manager. Mirrors `src-old/memory/MemoryManager.ts`.
//!
//! Responsibilities:
//!   1. File scan + incremental sync (hash → chunk → FTS/embedding index)
//!   2. Hybrid search (FTS5 + optional embeddings)
//!   3. File watching via periodic polling (auto re-index on changes)
//!   4. Provide search interface for AgentPool pre-retrieval

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use anyhow::Result;
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::db::Db;
use crate::memory::chunker::{chunk_text, ChunkerOptions};
use crate::memory::embedding::{create_embedding_provider, EmbeddingProvider};
use crate::memory::fts_search::{hybrid_search, SearchOptions, SearchResult};
use crate::memory::tokenizer::tokenize_optimized;

// ===== Singleton =====

static INSTANCE: Mutex<Option<Arc<MemoryManager>>> = Mutex::new(None);

pub fn init(db: Arc<Db>, config: &Config) -> Arc<MemoryManager> {
    let mut guard = INSTANCE.lock().unwrap();
    if let Some(ref existing) = *guard {
        return Arc::clone(existing);
    }
    let mgr = Arc::new(MemoryManager::new(db, config));
    *guard = Some(Arc::clone(&mgr));
    mgr
}

pub fn get_instance() -> Arc<MemoryManager> {
    INSTANCE
        .lock()
        .unwrap()
        .as_ref()
        .cloned()
        .expect("MemoryManager not initialized — call memory::manager::init() at daemon startup")
}

/// Non-panicking variant; returns None if not yet initialised.
pub fn try_get_instance() -> Option<Arc<MemoryManager>> {
    INSTANCE.lock().unwrap().as_ref().cloned()
}

// ===== Types =====

struct FileRecord {
    path: String,
    hash: String,
}

// ===== MemoryManager =====

pub struct MemoryManager {
    db: Arc<Db>,
    agents_dir: PathBuf,
    embedding_provider: Option<Box<dyn EmbeddingProvider>>,
    chunker_options: ChunkerOptions,
    /// folder → set of changed absolute paths (None = full resync needed)
    dirty: Mutex<HashMap<String, Option<HashSet<String>>>>,
    /// folder → CancellationToken for the polling watcher task
    watcher_tokens: Mutex<HashMap<String, CancellationToken>>,
    /// folder → custom memory directory (for cowork workspaces)
    custom_memory_dirs: Mutex<HashMap<String, PathBuf>>,
}

impl MemoryManager {
    fn new(db: Arc<Db>, config: &Config) -> Self {
        let emb = create_embedding_provider(config, Arc::clone(&db));
        Self {
            db,
            agents_dir: config.paths.agents_dir.clone(),
            embedding_provider: emb,
            chunker_options: ChunkerOptions::default(),
            dirty: Mutex::new(HashMap::new()),
            watcher_tokens: Mutex::new(HashMap::new()),
            custom_memory_dirs: Mutex::new(HashMap::new()),
        }
    }

    // ===== Public interface =====

    /// Register a custom memory directory for a folder (e.g., for cowork workspaces).
    pub fn register_custom_memory_dir(&self, folder: &str, custom_dir: PathBuf) {
        let mut dirs = self.custom_memory_dirs.lock().unwrap();
        dirs.insert(folder.to_string(), custom_dir);
        tracing::info!("[MemoryManager] Registered custom memory dir for {folder}");
    }

    /// Full scan + start polling watcher for an agent folder.
    pub async fn init_agent(self: &Arc<Self>, folder: &str) {
        // Purge stale conversation-summary entries from MEMORY.md left by the
        // old verbatim fallback (before the fix that skips the index for summaries).
        let base = self.get_memory_dir_for_folder(folder);
        if let Err(e) = crate::memory::curated::purge_summary_index_entries(&base) {
            tracing::warn!("[MemoryManager] purge_summary_index_entries failed: {e}");
        }
        tracing::info!("[MemoryManager] init_agent({folder}): sync starting...");
        self.sync_folder(folder).await;
        tracing::info!("[MemoryManager] init_agent({folder}): sync done, starting watcher");
        self.start_watching(folder);
        tracing::info!("[MemoryManager] init_agent({folder}): done");
    }

    /// Get the memory directory for a folder (custom if registered, otherwise default).
    fn get_memory_dir_for_folder(&self, folder: &str) -> PathBuf {
        let dirs = self.custom_memory_dirs.lock().unwrap();
        if let Some(custom_dir) = dirs.get(folder) {
            custom_dir.clone()
        } else {
            self.agents_dir.join(folder)
        }
    }

    /// Search memory for a folder.
    pub async fn search(
        &self,
        folder: &str,
        query: &str,
        options: Option<SearchOptions>,
    ) -> Result<Vec<SearchResult>> {
        // Process dirty files before search.
        // Collect work outside the critical section so MutexGuard doesn't
        // live across .await (required for Send).
        enum DirtyWork {
            None,
            FullRescan,
            SyncFiles(Vec<(String, String)>),
        }
        let dirty_work = {
            let mut dirty = self.dirty.lock().unwrap();
            match dirty.remove(folder) {
                Some(Some(changed_files)) => {
                    let files: Vec<_> = changed_files
                        .iter()
                        .map(|abs_path| {
                            let source = if abs_path.contains("/memory/") {
                                "memory"
                            } else {
                                "session"
                            };
                            (abs_path.clone(), source.to_string())
                        })
                        .collect();
                    DirtyWork::SyncFiles(files)
                }
                Some(None) => DirtyWork::FullRescan,
                None => DirtyWork::None,
            }
        };

        match dirty_work {
            DirtyWork::SyncFiles(files) => {
                for (abs_path, source) in &files {
                    let existing = self.get_file_record(abs_path, folder);
                    self.sync_file(abs_path, folder, source, existing.as_ref())
                        .await;
                }
            }
            DirtyWork::FullRescan => {
                self.sync_folder(folder).await;
            }
            DirtyWork::None => {}
        }

        let opts = options.unwrap_or_default();
        hybrid_search(
            &self.db,
            folder,
            query,
            self.embedding_provider.as_deref(),
            opts,
        )
        .await
    }

    /// Read a memory file fragment by path + line range.
    pub fn read_file(
        &self,
        folder: &str,
        relative_path: &str,
        start_line: Option<u32>,
        end_line: Option<u32>,
    ) -> Option<String> {
        let abs_path = self.resolve_memory_path(folder, relative_path)?;
        let content = fs::read_to_string(&abs_path).ok()?;
        let lines: Vec<&str> = content.lines().collect();
        let start = start_line.unwrap_or(1).saturating_sub(1) as usize;
        let end = end_line
            .map(|e| e.min(lines.len() as u32) as usize)
            .unwrap_or(lines.len());
        if start >= lines.len() {
            return Some(String::new());
        }
        Some(lines[start..end].join("\n"))
    }

    /// Mark a folder as dirty; re-sync before next search.
    pub fn mark_dirty(&self, folder: &str, changed_file: Option<&str>) {
        let mut dirty = self.dirty.lock().unwrap();
        let entry = dirty
            .entry(folder.to_string())
            .or_insert_with(|| Some(HashSet::new()));
        if let Some(ref mut files) = entry {
            if let Some(path) = changed_file {
                files.insert(path.to_string());
            } else {
                // Unknown changes → full rescan needed
                *entry = None;
            }
        }
    }

    /// Stop watcher and clean up state for a folder.
    pub fn destroy_agent(&self, folder: &str) {
        if let Some(token) = self.watcher_tokens.lock().unwrap().remove(folder) {
            token.cancel();
        }
        self.dirty.lock().unwrap().remove(folder);
        self.custom_memory_dirs.lock().unwrap().remove(folder);
        tracing::info!("[MemoryManager] destroy_agent({folder}): watcher cancelled");
    }

    /// Global cleanup.
    pub fn destroy(&self) {
        // No-op for now — watchers clean themselves.
    }

    // ===== Sync logic =====

    async fn sync_folder(&self, folder: &str) {
        let memory_base_dir = self.get_memory_dir_for_folder(folder);
        let memory_dir = memory_base_dir.join("memory");

        let mut disk_files: HashMap<String, (String, &str)> = HashMap::new();

        // MEMORY.md
        let memory_md = memory_base_dir.join("MEMORY.md");
        if memory_md.exists() {
            disk_files.insert(
                memory_md.to_string_lossy().to_string(),
                (memory_md.to_string_lossy().to_string(), "memory"),
            );
        }

        // memory/*.md
        if memory_dir.exists() {
            if let Ok(entries) = fs::read_dir(&memory_dir) {
                for entry in entries.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if path.extension().map(|e| e == "md").unwrap_or(false) {
                        let abs = path.to_string_lossy().to_string();
                        disk_files.insert(abs.clone(), (abs, "memory"));
                    }
                }
            }
        }

        tracing::info!(
            "[MemoryManager] sync_folder({folder}): {} disk files found",
            disk_files.len()
        );

        // Compare with DB
        let db_files = self.list_files(folder);
        let db_file_map: HashMap<&str, &FileRecord> =
            db_files.iter().map(|f| (f.path.as_str(), f)).collect();
        tracing::info!(
            "[MemoryManager] sync_folder({folder}): {} DB files, syncing...",
            db_files.len()
        );

        // Delete files no longer on disk
        for f in &db_files {
            if !disk_files.contains_key(&f.path) {
                self.remove_file(&f.path, folder);
            }
        }

        // Sync each file
        let mut synced = 0;
        for (abs_path, (_, source)) in &disk_files {
            let existing = db_file_map.get(abs_path.as_str()).copied();
            self.sync_file(abs_path, folder, source, existing).await;
            synced += 1;
        }
        tracing::info!("[MemoryManager] sync_folder({folder}): done, {synced} files synced");
    }

    async fn sync_file(
        &self,
        abs_path: &str,
        folder: &str,
        source: &str,
        existing: Option<&FileRecord>,
    ) {
        let stat = match fs::metadata(abs_path) {
            Ok(s) => s,
            Err(_) => {
                if existing.is_some() {
                    self.remove_file(abs_path, folder);
                }
                return;
            }
        };

        let content = match fs::read_to_string(abs_path) {
            Ok(c) => c,
            Err(_) => return,
        };

        let hash = format!("{:x}", Sha256::digest(content.as_bytes()));

        if let Some(existing) = existing {
            if existing.hash == hash {
                return;
            }
        }

        // Remove old chunks
        self.remove_chunks(abs_path, folder);

        // Chunk
        let raw_chunks = chunk_text(&content, self.chunker_options.clone());
        if raw_chunks.is_empty() {
            tracing::warn!(
                "[MemoryManager] sync_file: no chunks produced for {abs_path} (file may be empty)"
            );
            return;
        }

        // Generate embeddings (if provider exists)
        let mut embeddings: Option<Vec<Vec<f32>>> = None;
        if let Some(ref provider) = self.embedding_provider {
            let texts: Vec<String> = raw_chunks.iter().map(|c| c.text.clone()).collect();
            match provider.embed(&texts).await {
                Ok(emb) => embeddings = Some(emb),
                Err(e) => {
                    tracing::warn!("[MemoryManager] Embedding failed for {abs_path}, indexing without vectors: {e}");
                }
            }
        }

        // Batch write via transaction
        self.batch_write(
            abs_path,
            folder,
            source,
            &hash,
            stat.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
            stat.len() as i64,
            &raw_chunks,
            &embeddings,
        );
    }

    fn batch_write(
        &self,
        abs_path: &str,
        folder: &str,
        source: &str,
        hash: &str,
        mtime: i64,
        size: i64,
        chunks: &[crate::memory::chunker::Chunk],
        embeddings: &Option<Vec<Vec<f32>>>,
    ) {
        let model = self
            .embedding_provider
            .as_ref()
            .map(|p| p.model().to_string());

        let _ = self.db.with_conn_mut(|conn| {
            let tx = conn.transaction()?;
            
            // Upsert file record
            tx.execute(
                "INSERT INTO memory_files (path, folder, source, hash, mtime, size)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(path, folder) DO UPDATE SET
                   hash = excluded.hash, mtime = excluded.mtime, size = excluded.size",
                rusqlite::params![abs_path, folder, source, hash, mtime, size],
            )?;

            // Insert chunks
            for (i, chunk) in chunks.iter().enumerate() {
                let chunk_hash = format!("{:x}", Sha256::digest(chunk.text.as_bytes()));
                let id = uuid::Uuid::new_v4().to_string();

                let emb_blob: Option<Vec<u8>> = embeddings.as_ref().and_then(|embs| {
                    embs.get(i).map(|vec| {
                        vec.iter().flat_map(|f| f.to_le_bytes()).collect()
                    })
                });

                tx.execute(
                    "INSERT INTO memory_chunks (id, folder, path, source, start_line, end_line, hash, text, embedding, model)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    rusqlite::params![id, folder, abs_path, source, chunk.start_line, chunk.end_line, chunk_hash, chunk.text, emb_blob, model],
                )?;

                // FTS
                let tokenized = tokenize_optimized(&chunk.text, false).join(" ");
                tx.execute(
                    "INSERT INTO memory_chunks_fts (chunk_id, text) VALUES (?1, ?2)",
                    rusqlite::params![id, tokenized],
                )?;

                // sqlite-vec
                if let Some(ref blob) = emb_blob {
                    let _ = tx.execute(
                        "INSERT INTO memory_chunks_vec (chunk_id, embedding) VALUES (?1, ?2)",
                        rusqlite::params![id, blob],
                    );
                }
            }

            tx.commit()?;
            Ok(())
        });
    }

    fn remove_file(&self, abs_path: &str, folder: &str) {
        self.remove_chunks(abs_path, folder);
        let _ = self.db.with_conn(|conn| {
            conn.execute(
                "DELETE FROM memory_files WHERE path = ?1 AND folder = ?2",
                rusqlite::params![abs_path, folder],
            )?;
            Ok(())
        });
    }

    fn remove_chunks(&self, abs_path: &str, folder: &str) {
        let _ = self.db.with_conn(|conn| {
            // Get chunk IDs to clean up vec table
            let mut stmt =
                conn.prepare("SELECT id FROM memory_chunks WHERE path = ?1 AND folder = ?2")?;
            let ids: Vec<String> = stmt
                .query_map(rusqlite::params![abs_path, folder], |row| row.get(0))?
                .filter_map(|r| r.ok())
                .collect();

            // Delete from vec table in batches
            if !ids.is_empty() {
                for batch in ids.chunks(500) {
                    let placeholders: Vec<String> = batch.iter().map(|_| "?".to_string()).collect();
                    let sql = format!(
                        "DELETE FROM memory_chunks_vec WHERE chunk_id IN ({})",
                        placeholders.join(",")
                    );
                    let params: Vec<&dyn rusqlite::types::ToSql> = batch
                        .iter()
                        .map(|s| s as &dyn rusqlite::types::ToSql)
                        .collect();
                    let _ = conn.execute(&sql, params.as_slice());
                }
            }

            conn.execute(
                "DELETE FROM memory_chunks WHERE path = ?1 AND folder = ?2",
                rusqlite::params![abs_path, folder],
            )?;
            Ok(())
        });
    }

    fn list_files(&self, folder: &str) -> Vec<FileRecord> {
        self.db
            .with_conn(|conn| {
                let mut stmt = conn.prepare(
                "SELECT path, hash FROM memory_files WHERE folder = ?1"
            )?;
                let records = stmt
                    .query_map(rusqlite::params![folder], |row| {
                        Ok(FileRecord {
                            path: row.get(0)?,
                            hash: row.get(1)?,
                        })
                    })?
                    .filter_map(|r| r.ok())
                    .collect();
                Ok(records)
            })
            .unwrap_or_default()
    }

    fn get_file_record(&self, abs_path: &str, folder: &str) -> Option<FileRecord> {
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT path, hash FROM memory_files WHERE path = ?1 AND folder = ?2"
            )?;
            let result = stmt.query_row(rusqlite::params![abs_path, folder], |row| {
                Ok(FileRecord {
                    path: row.get(0)?,
                    hash: row.get(1)?,
                })
            }).optional().map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(result)
        }).unwrap_or(None)
    }

    // ===== File watching (poll-based) =====

    fn start_watching(self: &Arc<Self>, folder: &str) {
        let token = CancellationToken::new();
        self.watcher_tokens
            .lock()
            .unwrap()
            .insert(folder.to_string(), token.clone());

        let this = Arc::clone(self);
        let folder = folder.to_string();
        tokio::spawn(async move {
            // Poll every 1.5s (matches TS fs.watchFile interval)
            let mut interval = tokio::time::interval(Duration::from_millis(1500));
            let agent_dir = this.agents_dir.join(&folder);
            let memory_md = agent_dir.join("MEMORY.md");
            let memory_dir = agent_dir.join("memory");

            // Track last seen mtimes
            let mut last_mtimes: HashMap<PathBuf, i64> = HashMap::new();

            loop {
                tokio::select! {
                    _ = token.cancelled() => {
                        tracing::info!("[MemoryManager] watcher stopped for {folder}");
                        return;
                    }
                    _ = interval.tick() => {}
                }

                // Check MEMORY.md
                if let Ok(meta) = fs::metadata(&memory_md) {
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0);
                    let prev = last_mtimes.get(&memory_md).copied().unwrap_or(0);
                    if mtime != prev {
                        last_mtimes.insert(memory_md.clone(), mtime);
                        let source = "memory";
                        let existing = this.get_file_record(&memory_md.to_string_lossy(), &folder);
                        this.sync_file(
                            &memory_md.to_string_lossy(),
                            &folder,
                            source,
                            existing.as_ref(),
                        )
                        .await;
                    }
                }

                // Check memory/*.md
                if let Ok(entries) = fs::read_dir(&memory_dir) {
                    for entry in entries.filter_map(|e| e.ok()) {
                        let path = entry.path();
                        if path.extension().map(|e| e == "md").unwrap_or(false) {
                            if let Ok(meta) = fs::metadata(&path) {
                                let mtime = meta
                                    .modified()
                                    .ok()
                                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                    .map(|d| d.as_millis() as i64)
                                    .unwrap_or(0);
                                let prev = last_mtimes.get(&path).copied().unwrap_or(0);
                                if mtime != prev {
                                    last_mtimes.insert(path.clone(), mtime);
                                    let abs = path.to_string_lossy().to_string();
                                    let source = "memory";
                                    let existing = this.get_file_record(&abs, &folder);
                                    this.sync_file(&abs, &folder, source, existing.as_ref())
                                        .await;
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    // ===== Path resolution =====

    fn resolve_memory_path(&self, folder: &str, relative_path: &str) -> Option<PathBuf> {
        let memory_base_dir = self.get_memory_dir_for_folder(folder);

        // Canonicalise the base dir so symlinks / `..` components can't escape it.
        let base = memory_base_dir.canonicalize().ok()?;

        let is_safe = |p: &Path| -> bool {
            p.canonicalize()
                .ok()
                .map_or(false, |canon| canon.starts_with(&base))
        };

        // Try direct join
        let candidate = memory_base_dir.join(relative_path);
        if is_safe(&candidate) {
            return Some(candidate);
        }

        // Try memory/ subdirectory
        let candidate = memory_base_dir.join("memory").join(relative_path);
        if is_safe(&candidate) {
            return Some(candidate);
        }

        None
    }
}

// ===== Formatting =====

pub fn format_search_results(results: &[SearchResult]) -> String {
    if results.is_empty() {
        return String::new();
    }

    let mut lines = vec!["Relevant memories:".to_string()];
    for (i, r) in results.iter().enumerate() {
        let parts: Vec<&str> = r.path.split(&['/', '\\']).collect();
        let display_path = parts
            .iter()
            .rev()
            .take(2)
            .copied()
            .collect::<Vec<_>>()
            .join("/");
        lines.push(format!(
            "[{}] {}:{}-{} (score: {:.2})",
            i + 1,
            display_path,
            r.start_line,
            r.end_line,
            r.score
        ));
        let summary = if r.text.len() > 200 {
            format!(
                "{}...",
                crate::util::text::truncate_on_char_boundary(&r.text, 200)
            )
        } else {
            r.text.clone()
        };
        lines.push(summary);
        lines.push(String::new());
    }

    lines.join("\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use std::fs;

    fn test_db() -> Arc<Db> {
        Arc::new(Db::open_in_memory(&Config::from_env()).unwrap())
    }

    #[test]
    fn test_format_empty() {
        assert_eq!(format_search_results(&[]), "");
    }

    #[test]
    fn test_format_results() {
        let results = vec![SearchResult {
            id: "1".into(),
            path: "/agents/main/memory/test.md".into(),
            start_line: 1,
            end_line: 5,
            text: "hello world".into(),
            score: 0.85,
            source: "memory".into(),
        }];
        let out = format_search_results(&results);
        assert!(out.contains("Relevant memories:"));
        assert!(out.contains("test.md"));
        assert!(out.contains("0.85"));
    }

    #[test]
    fn test_read_file_missing() {
        let tmp = std::env::temp_dir().join(format!("test-mm-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&tmp).unwrap();
        let db = test_db();
        let mgr = MemoryManager::new(db, &Config::from_env());
        // Override agents_dir for test
        // read_file should return None for nonexistent path
        let result = mgr.read_file("test-folder", "nonexistent.md", None, None);
        assert!(result.is_none());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_format_results_truncates_long_text() {
        let long_text = "a".repeat(300);
        let results = vec![SearchResult {
            id: "1".into(),
            path: "/agents/main/memory/test.md".into(),
            start_line: 1,
            end_line: 5,
            text: long_text.clone(),
            score: 0.9,
            source: "memory".into(),
        }];
        let out = format_search_results(&results);
        assert!(!out.contains(&long_text));
        assert!(out.ends_with("..."));
    }
}
