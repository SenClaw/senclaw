//! Memory system schema. Mirrors `src-old/memory/memory-schema.ts`.
//!
//! Tables created (idempotent):
//!   * `memory_files` — source file tracking (hash change detection)
//!   * `memory_chunks` — chunk index (text + optional embeddings)
//!   * `memory_chunks_fts` — FTS5 standalone tokenized index
//!   * `embedding_cache` — provider+model+hash dedupe cache
//!   * `memory_meta` — per-folder KV (incl. embedding_model switch detection)
//!
//! There is **no `memory_chunks_vec` table**: the sqlite-vec extension is not
//! loaded, a decision recorded in [docs/memory.md](../../docs/memory.md).
//! Vector search still works — `fts_search::blob_scan` does exact
//! nearest-neighbour over the embedding blobs — so `enable_vec` means
//! "run the embedding-model switch detection", not "vector search is on".
//! The schema-migration paths (legacy FTS rebuild, model-switch reset) are
//! ported 1:1 so existing DBs upgrade cleanly.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

const GLOBAL_FOLDER: &str = "__global__";

/// Build the embedding-model identity used by switch-detection.
/// Format: `provider:model:dimensions` (matches the TS `buildModelKey`).
pub fn build_model_key(provider: &str, model: &str, dimensions: u32) -> String {
    format!("{provider}:{model}:{dimensions}")
}

/// Apply the memory schema. Idempotent.
///
/// * `enable_vec` — run embedding-model switch detection. No vector table is
///   created; search over embeddings is an exact scan (see module docs).
/// * `dimensions` — vector dimensions, recorded for when an index lands.
/// * `model_key` — current embedding model key for switch detection. Pass `""`
///   when vec is disabled.
pub fn apply_memory_schema(
    conn: &mut Connection,
    enable_vec: bool,
    dimensions: u32,
    model_key: &str,
) -> Result<()> {
    migrate_fts_table_if_needed(conn).context("migrate_fts_table_if_needed")?;

    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS memory_files (
          path     TEXT NOT NULL,
          folder   TEXT NOT NULL,
          source   TEXT NOT NULL,
          hash     TEXT NOT NULL,
          mtime    INTEGER NOT NULL,
          size     INTEGER NOT NULL,
          PRIMARY KEY (path, folder)
        );

        CREATE TABLE IF NOT EXISTS memory_chunks (
          id         TEXT PRIMARY KEY,
          folder     TEXT NOT NULL,
          path       TEXT NOT NULL,
          source     TEXT NOT NULL,
          start_line INTEGER NOT NULL,
          end_line   INTEGER NOT NULL,
          hash       TEXT NOT NULL,
          text       TEXT NOT NULL,
          embedding  BLOB,
          model      TEXT,
          FOREIGN KEY (path, folder)
            REFERENCES memory_files(path, folder) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_chunks_folder ON memory_chunks(folder);
        CREATE INDEX IF NOT EXISTS idx_chunks_path   ON memory_chunks(path, folder);

        CREATE VIRTUAL TABLE IF NOT EXISTS memory_chunks_fts
          USING fts5(chunk_id UNINDEXED, text);

        CREATE TRIGGER IF NOT EXISTS memory_chunks_ad
        AFTER DELETE ON memory_chunks BEGIN
          DELETE FROM memory_chunks_fts WHERE chunk_id = old.id;
        END;

        CREATE TABLE IF NOT EXISTS embedding_cache (
          provider   TEXT NOT NULL,
          model      TEXT NOT NULL,
          hash       TEXT NOT NULL,
          embedding  BLOB NOT NULL,
          created_at INTEGER NOT NULL DEFAULT (unixepoch()),
          PRIMARY KEY (provider, model, hash)
        );

        CREATE TABLE IF NOT EXISTS memory_meta (
          folder TEXT NOT NULL,
          key    TEXT NOT NULL,
          value  TEXT,
          PRIMARY KEY (folder, key)
        );
        "#,
    )
    .context("create memory tables")?;

    if enable_vec {
        try_create_vec_table(conn, dimensions, model_key)?;
    }
    Ok(())
}

/// Detect the legacy external-content FTS table and rebuild it as the new
/// standalone (`chunk_id UNINDEXED, text`) form.
fn migrate_fts_table_if_needed(conn: &mut Connection) -> Result<()> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='memory_chunks_fts'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok();

    let Some(sql) = existing else { return Ok(()) };
    if sql.to_lowercase().contains("chunk_id") {
        return Ok(());
    }

    tracing::info!(
        "[MemorySchema] Migrating FTS table from external-content to independent mode..."
    );
    let tx = conn.transaction()?;
    tx.execute_batch(
        r#"
        DROP TRIGGER IF EXISTS memory_chunks_ai;
        DROP TRIGGER IF EXISTS memory_chunks_au;
        DROP TRIGGER IF EXISTS memory_chunks_ad;
        DROP TABLE IF EXISTS memory_chunks_fts;
        DELETE FROM memory_chunks;
        DELETE FROM memory_files;
        "#,
    )?;
    tx.commit()?;
    tracing::info!("[MemorySchema] FTS migration done. Files will be re-indexed on next startup.");
    Ok(())
}

/// Embedding-model switch detection, and the seam an index would slot into.
///
/// What it does today: when the configured embedding model differs from the
/// one the stored vectors were built with, clear those vectors so they are
/// rebuilt rather than compared against a model that never produced them.
///
/// What it deliberately does not do: create a `memory_chunks_vec` table.
/// Adopting sqlite-vec would mean `conn.load_extension`, a native dependency
/// on three CI targets, and a backfill — worth it at a corpus size this
/// install is nowhere near. The threshold and the measurement are in
/// docs/memory.md; `fts_search::vec_search` already prefers the table when
/// one exists, so landing it later changes no call site.
fn try_create_vec_table(conn: &mut Connection, dimensions: u32, model_key: &str) -> Result<()> {
    if !model_key.is_empty() {
        let stored: Option<String> = conn
            .query_row(
                "SELECT value FROM memory_meta WHERE folder = ?1 AND key = 'embedding_model'",
                params![GLOBAL_FOLDER],
                |r| r.get::<_, String>(0),
            )
            .ok();

        if let Some(prev) = stored.as_deref() {
            if !prev.is_empty() && prev != model_key {
                tracing::info!(
                    "[MemorySchema] Embedding model changed ({prev} → {model_key}), clearing old embeddings..."
                );
                let tx = conn.transaction()?;
                tx.execute_batch(
                    r#"
                    UPDATE memory_chunks SET embedding = NULL, model = NULL;
                    DELETE FROM memory_meta WHERE key = 'embedding_model' AND folder = '__global__';
                    DROP TABLE IF EXISTS memory_chunks_vec;
                    "#,
                )?;
                tx.commit()?;
            }
        }

        conn.execute(
            "INSERT OR REPLACE INTO memory_meta (folder, key, value) VALUES (?1, 'embedding_model', ?2)",
            params![GLOBAL_FOLDER, model_key],
        )?;
    }

    // Vector search is **not** disabled without sqlite-vec: `fts_search::
    // blob_scan` does exact nearest-neighbour over the stored embeddings in
    // Rust. The extension would make that a real index; the decision not to
    // adopt it yet, and what would change it, is in docs/memory.md.
    tracing::debug!(
        dimensions,
        "[MemorySchema] vector search runs as an exact scan over embedding blobs (no sqlite-vec extension)"
    );
    Ok(())
}

/// Drop cached embeddings. Mirrors `cleanupEmbeddingCache` in TS.
///
/// * `(Some(p), Some(m))` — narrow to provider+model
/// * `(Some(p), None)`    — narrow to provider
/// * `(None, _)`          — wipe everything (model alone is meaningless without provider)
pub fn cleanup_embedding_cache(
    conn: &Connection,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<usize> {
    let changed = match (provider, model) {
        (Some(p), Some(m)) => conn.execute(
            "DELETE FROM embedding_cache WHERE provider = ?1 AND model = ?2",
            params![p, m],
        )?,
        (Some(p), None) => conn.execute(
            "DELETE FROM embedding_cache WHERE provider = ?1",
            params![p],
        )?,
        (None, _) => conn.execute("DELETE FROM embedding_cache", [])?,
    };
    Ok(changed)
}

#[derive(Debug, Clone)]
pub struct EmbeddingCacheStat {
    pub provider: String,
    pub model: String,
    pub count: i64,
    pub total_size: i64,
}

pub fn get_embedding_cache_stats(conn: &Connection) -> Result<Vec<EmbeddingCacheStat>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT provider, model, COUNT(*) AS count, COALESCE(SUM(LENGTH(embedding)), 0) AS total_size
        FROM embedding_cache
        GROUP BY provider, model
        ORDER BY total_size DESC
        "#,
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(EmbeddingCacheStat {
                provider: r.get(0)?,
                model: r.get(1)?,
                count: r.get(2)?,
                total_size: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_in_memory() -> Connection {
        Connection::open_in_memory().unwrap()
    }

    #[test]
    fn build_model_key_format() {
        assert_eq!(build_model_key("openai", "x", 1536), "openai:x:1536");
    }

    #[test]
    fn apply_creates_tables_idempotently() {
        let mut c = open_in_memory();
        apply_memory_schema(&mut c, false, 1536, "").unwrap();
        // Second call must not error.
        apply_memory_schema(&mut c, false, 1536, "").unwrap();
        let tables: Vec<String> = c
            .prepare(
                "SELECT name FROM sqlite_master WHERE type IN ('table','virtual') ORDER BY name",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        for needed in [
            "memory_files",
            "memory_chunks",
            "memory_chunks_fts",
            "embedding_cache",
            "memory_meta",
        ] {
            assert!(tables.iter().any(|t| t == needed), "missing {needed}");
        }
    }

    #[test]
    fn enable_vec_logs_and_records_model_key() {
        let mut c = open_in_memory();
        apply_memory_schema(&mut c, true, 1536, "openai:foo:1536").unwrap();
        let stored: String = c
            .query_row(
                "SELECT value FROM memory_meta WHERE folder='__global__' AND key='embedding_model'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, "openai:foo:1536");
    }

    #[test]
    fn model_switch_clears_embeddings() {
        let mut c = open_in_memory();
        apply_memory_schema(&mut c, true, 1536, "openai:a:1536").unwrap();
        // Insert a fake chunk with a non-null embedding + model.
        c.execute(
            "INSERT INTO memory_files (path, folder, source, hash, mtime, size)
             VALUES ('/x', 'main', 'memory', 'h', 1, 1)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO memory_chunks (id, folder, path, source, start_line, end_line, hash, text, embedding, model)
             VALUES ('c1', 'main', '/x', 'memory', 1, 1, 'h', 't', x'01', 'openai:a:1536')",
            [],
        ).unwrap();

        // Re-apply with a different model key — embedding/model must be wiped.
        apply_memory_schema(&mut c, true, 1536, "openai:b:1536").unwrap();
        let (emb, model): (Option<Vec<u8>>, Option<String>) = c
            .query_row(
                "SELECT embedding, model FROM memory_chunks WHERE id='c1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(emb.is_none());
        assert!(model.is_none());

        // Stored key now reflects the new model.
        let stored: String = c
            .query_row(
                "SELECT value FROM memory_meta WHERE folder='__global__' AND key='embedding_model'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, "openai:b:1536");
    }

    #[test]
    fn cleanup_cache_filters() {
        let mut c = open_in_memory();
        apply_memory_schema(&mut c, false, 1536, "").unwrap();
        for (p, m, h) in [
            ("openai", "x", "h1"),
            ("openai", "y", "h2"),
            ("ollama", "x", "h3"),
        ] {
            c.execute(
                "INSERT INTO embedding_cache (provider, model, hash, embedding) VALUES (?1, ?2, ?3, x'00')",
                params![p, m, h],
            ).unwrap();
        }
        assert_eq!(
            cleanup_embedding_cache(&c, Some("openai"), Some("x")).unwrap(),
            1
        );
        assert_eq!(
            cleanup_embedding_cache(&c, Some("openai"), None).unwrap(),
            1
        );
        assert_eq!(cleanup_embedding_cache(&c, None, None).unwrap(), 1);
    }
}
