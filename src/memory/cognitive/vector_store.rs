//! VectorStore — dense embedding storage for cognitive nodes.
//!
//! Two backends, chosen at runtime by [`SqliteVectorStore::search`]:
//!
//! 1. **sqlite-vec** — if `cog_vec` virtual table was created during schema
//!    init (extension loaded), use KNN via `vec_distance_cosine`.
//! 2. **BLOB fallback** — brute-force cosine over `cog_nodes.embedding`
//!    BLOB column. O(N) but fine for graphs up to ~50k nodes.
//!
//! Either way the BLOB on `cog_nodes` is the authoritative copy; `cog_vec`
//! is just an index. `upsert` writes both transparently.

use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use std::sync::Arc;
use uuid::Uuid;

use crate::db::Db;

/// (node_id, distance) — lower distance = more similar.
#[derive(Debug, Clone)]
pub struct VectorHit {
    pub node_id: Uuid,
    pub distance: f32,
}

pub trait VectorStore: Send + Sync {
    /// Store an embedding for `node_id`. Idempotent — replaces existing.
    fn upsert(&self, node_id: Uuid, embedding: &[f32], model: &str) -> Result<()>;
    fn delete(&self, node_id: Uuid) -> Result<()>;
    /// k-NN cosine search. Returns hits sorted ascending by distance.
    fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>>;
    /// True if the sqlite-vec extension is loaded — gives callers visibility
    /// into which backend they'll hit.
    fn has_vec_index(&self) -> bool;
}

pub struct SqliteVectorStore {
    db: Arc<Db>,
}

impl SqliteVectorStore {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }
}

fn floats_to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

pub(crate) fn blob_to_floats(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

pub(crate) fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return f32::INFINITY;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = (na.sqrt() * nb.sqrt()).max(1e-12);
    1.0 - dot / denom
}

fn vec_table_exists(conn: &rusqlite::Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type IN ('table','view') AND name = 'cog_vec'",
        [],
        |_| Ok(()),
    )
    .optional()
    .ok()
    .flatten()
    .is_some()
}

impl VectorStore for SqliteVectorStore {
    fn upsert(&self, node_id: Uuid, embedding: &[f32], model: &str) -> Result<()> {
        let id_blob = node_id.as_bytes().to_vec();
        let emb_blob = floats_to_blob(embedding);
        self.db.with_cog_conn(|conn| {
            // Authoritative copy on cog_nodes.embedding
            conn.execute(
                "UPDATE cog_nodes SET embedding = ?1, embedding_model = ?2 WHERE id = ?3",
                params![emb_blob, model, id_blob],
            )?;
            // Best-effort sync into cog_vec when available
            if vec_table_exists(conn) {
                let _ = conn.execute(
                    "INSERT OR REPLACE INTO cog_vec (node_id, embedding) VALUES (?1, ?2)",
                    params![id_blob, emb_blob],
                );
            }
            Ok(())
        })
    }

    fn delete(&self, node_id: Uuid) -> Result<()> {
        let id_blob = node_id.as_bytes().to_vec();
        self.db.with_cog_conn(|conn| {
            conn.execute(
                "UPDATE cog_nodes SET embedding = NULL, embedding_model = NULL WHERE id = ?1",
                params![id_blob],
            )?;
            if vec_table_exists(conn) {
                let _ = conn.execute("DELETE FROM cog_vec WHERE node_id = ?1", params![id_blob]);
            }
            Ok(())
        })
    }

    fn search(&self, query: &[f32], k: usize) -> Result<Vec<VectorHit>> {
        let q_blob = floats_to_blob(query);
        self.db.with_cog_conn(|conn| {
            // Try sqlite-vec path
            if vec_table_exists(conn) {
                let sql = "SELECT node_id, distance FROM cog_vec
                           WHERE embedding MATCH ?1
                           ORDER BY distance
                           LIMIT ?2";
                let hits: rusqlite::Result<Vec<VectorHit>> =
                    conn.prepare(sql).and_then(|mut stmt| {
                        stmt.query_map(params![q_blob, k as i64], |row| {
                            let id_blob: Vec<u8> = row.get(0)?;
                            let dist: f64 = row.get(1)?;
                            let arr: [u8; 16] = id_blob.as_slice().try_into().map_err(|_| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    0,
                                    rusqlite::types::Type::Blob,
                                    "uuid must be 16 bytes".into(),
                                )
                            })?;
                            Ok(VectorHit {
                                node_id: Uuid::from_bytes(arr),
                                distance: dist as f32,
                            })
                        })?
                        .collect()
                    });
                if let Ok(rows) = hits {
                    return Ok(rows);
                }
                // fall through to BLOB scan on error
            }
            // Brute-force fallback
            let mut stmt =
                conn.prepare("SELECT id, embedding FROM cog_nodes WHERE embedding IS NOT NULL")?;
            let mut heap: Vec<VectorHit> = stmt
                .query_map([], |row| {
                    let id_blob: Vec<u8> = row.get(0)?;
                    let emb_blob: Vec<u8> = row.get(1)?;
                    let arr: [u8; 16] = id_blob.as_slice().try_into().map_err(|_| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Blob,
                            "uuid must be 16 bytes".into(),
                        )
                    })?;
                    let emb = blob_to_floats(&emb_blob);
                    Ok(VectorHit {
                        node_id: Uuid::from_bytes(arr),
                        distance: cosine_distance(query, &emb),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            heap.sort_by(|a, b| {
                a.distance
                    .partial_cmp(&b.distance)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            heap.truncate(k);
            Ok(heap)
        })
    }

    fn has_vec_index(&self) -> bool {
        self.db
            .with_conn(|conn| Ok(vec_table_exists(conn)))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use crate::memory::cognitive::data_point::DataPoint;
    use crate::memory::cognitive::graph_store::{GraphStore, SqliteGraphStore};

    fn test_db() -> Arc<Db> {
        let cfg = Config::from_env();
        Arc::new(Db::open_in_memory(&cfg).expect("open in-memory db"))
    }

    #[test]
    fn cosine_distance_is_zero_for_identical() {
        let a = vec![1.0, 0.0, 0.0];
        assert!(cosine_distance(&a, &a).abs() < 1e-5);
    }

    #[test]
    fn upsert_then_search_blob_fallback() {
        let db = test_db();
        let g = SqliteGraphStore::new(Arc::clone(&db));
        let v = SqliteVectorStore::new(Arc::clone(&db));

        let n1 = DataPoint::chunk("apple", None, 1);
        let n2 = DataPoint::chunk("banana", None, 1);
        let n3 = DataPoint::chunk("cherry", None, 1);
        g.upsert_node(&n1).unwrap();
        g.upsert_node(&n2).unwrap();
        g.upsert_node(&n3).unwrap();

        v.upsert(n1.id, &[1.0, 0.0, 0.0], "test").unwrap();
        v.upsert(n2.id, &[0.9, 0.1, 0.0], "test").unwrap();
        v.upsert(n3.id, &[0.0, 1.0, 0.0], "test").unwrap();

        let hits = v.search(&[1.0, 0.0, 0.0], 2).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].node_id, n1.id, "exact match wins");
        assert_eq!(hits[1].node_id, n2.id, "near match second");
        assert!(hits[0].distance <= hits[1].distance);
    }

    #[test]
    fn delete_removes_from_search() {
        let db = test_db();
        let g = SqliteGraphStore::new(Arc::clone(&db));
        let v = SqliteVectorStore::new(Arc::clone(&db));
        let n = DataPoint::chunk("solo", None, 1);
        g.upsert_node(&n).unwrap();
        v.upsert(n.id, &[1.0, 0.0], "test").unwrap();
        assert_eq!(v.search(&[1.0, 0.0], 5).unwrap().len(), 1);
        v.delete(n.id).unwrap();
        assert_eq!(v.search(&[1.0, 0.0], 5).unwrap().len(), 0);
    }
}
