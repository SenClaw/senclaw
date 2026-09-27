use anyhow::Result;
use rusqlite::{params, OptionalExtension};

impl super::Db {
    // ============================================================
    // Embedding cache
    // ============================================================

    pub fn get_cached_embedding(
        &self,
        provider: &str,
        model: &str,
        hash: &str,
    ) -> Result<Option<Vec<u8>>> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT embedding FROM embedding_cache WHERE provider = ?1 AND model = ?2 AND hash = ?3",
                params![provider, model, hash],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()?)
        })
    }

    pub fn insert_cached_embedding(
        &self,
        provider: &str,
        model: &str,
        hash: &str,
        embedding: &[u8],
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT OR IGNORE INTO embedding_cache (provider, model, hash, embedding) VALUES (?1, ?2, ?3, ?4)",
                params![provider, model, hash, embedding],
            )?;
            Ok(())
        })
    }
}
