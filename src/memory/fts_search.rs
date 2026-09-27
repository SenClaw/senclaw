//! FTS5 + hybrid (vector+FTS) search. Mirrors `src-old/memory/fts-search.ts`.
//!
//! Retrieval strategy (progressive fallback):
//!   1. Embedding available → hybrid (vector 0.7 + FTS 0.3)
//!   2. No embedding → FTS5 (BM25)
//!   3. No FTS results → keyword substring fallback

use anyhow::{Context, Result};
use std::collections::BinaryHeap;

use rusqlite::{params, Connection};

use crate::db::Db;
use crate::memory::embedding::EmbeddingProvider;
use crate::memory::query_rewrite::{expand_query_tokens, smart_rewrite_query};
use crate::memory::tokenizer::{generate_2gram, tokenize_optimized};

// ===== Types =====

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub id: String,
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub text: String,
    pub score: f32,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub max_results: usize,
    pub min_score: f32,
    pub source: Option<String>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            max_results: 6,
            min_score: 0.25,
            source: None,
        }
    }
}

// ===== Main entry =====

pub async fn hybrid_search(
    db: &Db,
    folder: &str,
    query: &str,
    embedding_provider: Option<&dyn EmbeddingProvider>,
    options: SearchOptions,
) -> Result<Vec<SearchResult>> {
    let max_results = options.max_results;
    let min_score = options.min_score;
    let source_filter = options.source.as_deref().unwrap_or("all");

    if let Some(provider) = embedding_provider {
        match mixed_search(db, folder, query, provider, source_filter, max_results).await {
            Ok(results) if !results.is_empty() => {
                return Ok(results
                    .into_iter()
                    .filter(|r| r.score >= min_score)
                    .take(max_results)
                    .collect());
            }
            Err(e) => {
                tracing::warn!("[MemorySearch] Embedding search failed, falling back to FTS: {e}");
            }
            _ => {}
        }
    }

    let fts = db.with_conn(|c| fts_search(c, folder, query, source_filter, max_results * 2))?;
    if !fts.is_empty() {
        return Ok(fts.into_iter().take(max_results).collect());
    }

    db.with_conn(|c| keyword_fallback(c, folder, query, source_filter, max_results))
}

// ===== FTS5 search =====

fn fts_search(
    conn: &Connection,
    folder: &str,
    query: &str,
    source_filter: &str,
    limit: usize,
) -> Result<Vec<SearchResult>> {
    let rewritten = smart_rewrite_query(query);
    let tokens = tokenize_optimized(&rewritten, true);
    if tokens.is_empty() {
        return Ok(vec![]);
    }

    let expanded = expand_query_tokens(&tokens);
    let sanitize = |t: &str| -> String {
        t.chars()
            .filter(|c| !matches!(c, '"' | '\'' | '`' | '(' | ')' | '*' | '^' | '-'))
            .collect()
    };
    let fts_query = expanded
        .iter()
        .map(|t| sanitize(t))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" OR ");
    if fts_query.is_empty() {
        return Ok(vec![]);
    }

    let rows: Vec<FtsRow> = if source_filter != "all" {
        let mut stmt = conn.prepare(
            "SELECT c.id, c.path, c.start_line, c.end_line, c.text, c.source, \
             bm25(memory_chunks_fts) AS rank \
             FROM memory_chunks_fts f JOIN memory_chunks c ON c.id = f.chunk_id \
             WHERE f.text MATCH ?1 AND c.folder = ?2 AND c.source = ?3 \
             ORDER BY rank LIMIT ?4",
        )?;
        let mapped = stmt.query_map(
            params![fts_query, folder, source_filter, limit as i64],
            row_to_fts,
        )?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        let mut stmt = conn.prepare(
            "SELECT c.id, c.path, c.start_line, c.end_line, c.text, c.source, \
             bm25(memory_chunks_fts) AS rank \
             FROM memory_chunks_fts f JOIN memory_chunks c ON c.id = f.chunk_id \
             WHERE f.text MATCH ?1 AND c.folder = ?2 \
             ORDER BY rank LIMIT ?3",
        )?;
        let mapped = stmt.query_map(params![fts_query, folder, limit as i64], row_to_fts)?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };

    if rows.is_empty() {
        return Ok(vec![]);
    }

    let ranks: Vec<f64> = rows.iter().map(|r| r.rank).collect();
    let min_rank = ranks.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_rank = ranks.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let range = max_rank - min_rank;

    Ok(rows
        .into_iter()
        .map(|r| SearchResult {
            id: r.id,
            path: r.path,
            start_line: r.start_line,
            end_line: r.end_line,
            text: r.text,
            source: r.source,
            score: if range == 0.0 {
                1.0
            } else {
                ((max_rank - r.rank) / range) as f32
            },
        })
        .collect())
}

fn row_to_fts(row: &rusqlite::Row<'_>) -> rusqlite::Result<FtsRow> {
    Ok(FtsRow {
        id: row.get(0)?,
        path: row.get(1)?,
        start_line: row.get(2)?,
        end_line: row.get(3)?,
        text: row.get(4)?,
        source: row.get(5)?,
        rank: row.get(6)?,
    })
}

struct FtsRow {
    id: String,
    path: String,
    start_line: u32,
    end_line: u32,
    text: String,
    source: String,
    rank: f64,
}

// ===== Keyword substring fallback =====

fn keyword_fallback(
    conn: &Connection,
    folder: &str,
    query: &str,
    source_filter: &str,
    limit: usize,
) -> Result<Vec<SearchResult>> {
    let rewritten = smart_rewrite_query(query);
    let tokens = tokenize_optimized(&rewritten, true);
    let ngrams = generate_2gram(&rewritten);
    let all_tokens: Vec<String> = tokens.into_iter().chain(ngrams).collect();
    if all_tokens.is_empty() {
        return Ok(vec![]);
    }

    let rows: Vec<ChunkRow> = if source_filter != "all" {
        let mut stmt = conn.prepare(
            "SELECT id, path, start_line, end_line, text, source FROM memory_chunks WHERE folder = ?1 AND source = ?2",
        )?;
        let mapped = stmt.query_map(params![folder, source_filter], row_to_chunk)?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, path, start_line, end_line, text, source FROM memory_chunks WHERE folder = ?1",
        )?;
        let mapped = stmt.query_map(params![folder], row_to_chunk)?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut results: Vec<SearchResult> = Vec::new();
    for row in &rows {
        let text_lower = row.text.to_lowercase();
        let row_tokens: std::collections::HashSet<String> = tokenize_optimized(&row.text, false)
            .into_iter()
            .chain(generate_2gram(&row.text))
            .collect();
        let match_count = all_tokens
            .iter()
            .filter(|t| row_tokens.contains(t.as_str()) || text_lower.contains(&t.to_lowercase()))
            .count();
        if match_count > 0 {
            results.push(SearchResult {
                id: row.id.clone(),
                path: row.path.clone(),
                start_line: row.start_line,
                end_line: row.end_line,
                text: row.text.clone(),
                source: row.source.clone(),
                score: match_count as f32 / all_tokens.len() as f32,
            });
        }
    }
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(results.into_iter().take(limit).collect())
}

fn row_to_chunk(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChunkRow> {
    Ok(ChunkRow {
        id: row.get(0)?,
        path: row.get(1)?,
        start_line: row.get(2)?,
        end_line: row.get(3)?,
        text: row.get(4)?,
        source: row.get(5)?,
    })
}

struct ChunkRow {
    id: String,
    path: String,
    start_line: u32,
    end_line: u32,
    text: String,
    source: String,
}

// ===== Mixed/hybrid search =====

async fn mixed_search(
    db: &Db,
    folder: &str,
    query: &str,
    provider: &dyn EmbeddingProvider,
    source_filter: &str,
    limit: usize,
) -> Result<Vec<SearchResult>> {
    let emb = provider
        .embed(&[query.to_string()])
        .await
        .context("embed query")?;
    let q_emb = emb.into_iter().next().context("embed() returned empty")?;

    let vec_results = db.with_conn(|c| vec_search(c, folder, &q_emb, source_filter, limit * 2))?;
    if vec_results.is_empty() {
        return Ok(vec![]);
    }

    let fts_results = db.with_conn(|c| fts_search(c, folder, query, source_filter, limit * 2))?;

    let mut combined: std::collections::HashMap<String, SearchResult> =
        std::collections::HashMap::new();
    for r in vec_results {
        combined.insert(
            r.id.clone(),
            SearchResult {
                score: r.score * 0.7,
                ..r
            },
        );
    }
    for r in fts_results {
        if let Some(e) = combined.get_mut(&r.id) {
            // chunk found by both vector AND fts — combine weights
            e.score += r.score * 0.3;
        } else {
            // fts-only result gets 0.3 weight (not the vector weight of 0.7)
            combined.insert(
                r.id.clone(),
                SearchResult {
                    score: r.score * 0.3,
                    ..r
                },
            );
        }
    }
    let mut merged: Vec<SearchResult> = combined.into_values().collect();
    merged.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(merged.into_iter().take(limit).collect())
}

// ===== Vector search =====
//
// Two paths, in order: the `memory_chunks_vec` virtual table when the
// sqlite-vec extension created one, and otherwise an exact scan of the
// embedding blobs in Rust. The scan is what actually runs today — see
// [docs/memory.md](../../docs/memory.md) for why the extension has not been
// adopted and what would change the answer.

fn vec_search(
    conn: &Connection,
    folder: &str,
    query_embedding: &[f32],
    source_filter: &str,
    limit: usize,
) -> Result<Vec<SearchResult>> {
    if vec0_table_exists(conn) {
        if let Ok(results) = try_vec0(conn, folder, query_embedding, source_filter, limit) {
            if !results.is_empty() {
                return Ok(results);
            }
        }
    }
    blob_scan(conn, folder, query_embedding, source_filter, limit)
}

/// Whether the sqlite-vec virtual table exists on this connection.
///
/// Without this check every single query prepared a statement against a
/// missing table, failed, and fell through — a wasted parse per search on
/// every install that has no extension, which is all of them today.
fn vec0_table_exists(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type IN ('table','view') AND name = 'memory_chunks_vec'",
        [],
        |_| Ok(()),
    )
    .is_ok()
}

fn try_vec0(
    conn: &Connection,
    folder: &str,
    query_embedding: &[f32],
    source_filter: &str,
    limit: usize,
) -> Result<Vec<SearchResult>> {
    let query_buf: Vec<u8> = query_embedding
        .iter()
        .flat_map(|f| f.to_le_bytes())
        .collect();
    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_chunks_vec", [], |r| r.get(0))
        .unwrap_or(0);
    let k = total.max((limit * 2) as i64);

    let rows: Vec<VecDistanceRow> = if source_filter != "all" {
        let mut stmt = conn.prepare(
            "SELECT v.chunk_id, c.path, c.start_line, c.end_line, c.text, c.source, v.distance \
             FROM memory_chunks_vec v JOIN memory_chunks c ON c.id = v.chunk_id \
             WHERE v.embedding MATCH ?1 AND k = ?2 AND c.folder = ?3 AND c.source = ?4",
        )?;
        let mapped = stmt.query_map(
            params![query_buf, k, folder, source_filter],
            row_to_vec_dist,
        )?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        let mut stmt = conn.prepare(
            "SELECT v.chunk_id, c.path, c.start_line, c.end_line, c.text, c.source, v.distance \
             FROM memory_chunks_vec v JOIN memory_chunks c ON c.id = v.chunk_id \
             WHERE v.embedding MATCH ?1 AND k = ?2 AND c.folder = ?3",
        )?;
        let mapped = stmt.query_map(params![query_buf, k, folder], row_to_vec_dist)?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };
    Ok(normalize_vec_results(rows))
}

fn row_to_vec_dist(row: &rusqlite::Row<'_>) -> rusqlite::Result<VecDistanceRow> {
    Ok(VecDistanceRow {
        id: row.get(0)?,
        path: row.get(1)?,
        start_line: row.get(2)?,
        end_line: row.get(3)?,
        text: row.get(4)?,
        source: row.get(5)?,
        distance: row.get(6)?,
    })
}

/// Exact nearest-neighbour scan over the stored embedding blobs.
///
/// Two things keep the cost down without changing a single result:
///
/// * **Only `id` and `embedding` are read while scanning.** The earlier
///   version selected each chunk's `text` too, so a search over a folder of
///   prose pulled every chunk's full body out of SQLite to then discard all
///   but a handful. The winners' rows are fetched afterwards, by id.
/// * **The top-K is kept in a bounded heap**, so memory is the K results
///   rather than a distance for every chunk in the folder.
///
/// It is still O(n·d) in the number of embedded chunks — exact KNN over
/// blobs is — and that is a deliberate choice recorded in `docs/memory.md`.
fn blob_scan(
    conn: &Connection,
    folder: &str,
    query_embedding: &[f32],
    source_filter: &str,
    limit: usize,
) -> Result<Vec<SearchResult>> {
    let keep = (limit * 2).max(1);
    // BinaryHeap is a max-heap, so the root is the *worst* kept candidate and
    // popping it is what makes room for a better one.
    let mut best: BinaryHeap<Scored> = BinaryHeap::with_capacity(keep + 1);

    let mut consider = |id: String, embedding: Option<Vec<u8>>| {
        let Some(emb) = embedding else { return };
        let Some(distance) = cosine_distance(query_embedding, &emb) else {
            return;
        };
        if best.len() < keep {
            best.push(Scored { distance, id });
        } else if best.peek().is_some_and(|w| distance < w.distance) {
            best.pop();
            best.push(Scored { distance, id });
        }
    };

    if source_filter != "all" {
        let mut stmt = conn.prepare(
            "SELECT id, embedding FROM memory_chunks \
             WHERE folder = ?1 AND source = ?2 AND embedding IS NOT NULL",
        )?;
        let mut rows = stmt.query(params![folder, source_filter])?;
        while let Some(row) = rows.next()? {
            consider(row.get(0)?, row.get(1)?);
        }
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, embedding FROM memory_chunks \
             WHERE folder = ?1 AND embedding IS NOT NULL",
        )?;
        let mut rows = stmt.query(params![folder])?;
        while let Some(row) = rows.next()? {
            consider(row.get(0)?, row.get(1)?);
        }
    }

    let mut winners = best.into_sorted_vec();
    winners.reverse(); // into_sorted_vec is worst-last for a max-heap
    winners.sort_by(|a, b| {
        a.distance
            .partial_cmp(&b.distance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut with_dist = Vec::with_capacity(winners.len());
    let mut stmt = conn.prepare(
        "SELECT id, path, start_line, end_line, text, source FROM memory_chunks WHERE id = ?1",
    )?;
    for w in winners {
        let row = stmt.query_row(params![w.id], |row| {
            Ok(VecDistanceRow {
                id: row.get(0)?,
                path: row.get(1)?,
                start_line: row.get(2)?,
                end_line: row.get(3)?,
                text: row.get(4)?,
                source: row.get(5)?,
                distance: w.distance,
            })
        });
        // A chunk deleted between the scan and the fetch is simply gone; the
        // rest of the results are still correct.
        if let Ok(r) = row {
            with_dist.push(r);
        }
    }
    Ok(normalize_vec_results(with_dist))
}

/// One candidate in the bounded top-K heap. Ordered by distance so the heap's
/// root is the worst kept candidate; ties break on id to keep the order
/// total and the results stable.
struct Scored {
    distance: f32,
    id: String,
}

impl PartialEq for Scored {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for Scored {}
impl PartialOrd for Scored {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Scored {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.distance
            // NaN would make the heap's ordering inconsistent, which can panic
            // inside BinaryHeap; treat it as the worst possible distance.
            .partial_cmp(&other.distance)
            .unwrap_or_else(|| match (self.distance.is_nan(), other.distance.is_nan()) {
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                _ => std::cmp::Ordering::Equal,
            })
            .then_with(|| self.id.cmp(&other.id))
    }
}

fn cosine_distance(a: &[f32], b_blob: &[u8]) -> Option<f32> {
    if b_blob.len() % 4 != 0 {
        return None;
    }
    let b: Vec<f32> = b_blob
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    if a.len() != b.len() {
        return None;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return Some(1.0);
    }
    Some(1.0 - dot / (na * nb))
}

fn normalize_vec_results(rows: Vec<VecDistanceRow>) -> Vec<SearchResult> {
    if rows.is_empty() {
        return vec![];
    }
    let distances: Vec<f32> = rows.iter().map(|r| r.distance).collect();
    let min_dist = distances.iter().cloned().fold(f32::INFINITY, f32::min);
    let max_dist = distances.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let dist_range = max_dist - min_dist;
    if dist_range < 0.05 || min_dist > 0.6 {
        return vec![];
    }
    rows.into_iter()
        .map(|r| SearchResult {
            id: r.id,
            path: r.path,
            start_line: r.start_line,
            end_line: r.end_line,
            text: r.text,
            source: r.source,
            score: (max_dist - r.distance) / dist_range,
        })
        .collect()
}

struct VecDistanceRow {
    id: String,
    path: String,
    start_line: u32,
    end_line: u32,
    text: String,
    source: String,
    distance: f32,
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_distance_identical() {
        let a: Vec<f32> = vec![1.0, 0.0, 0.0];
        let b: Vec<u8> = a.iter().flat_map(|f| f.to_le_bytes()).collect();
        let d = cosine_distance(&a, &b).unwrap();
        assert!(d < 0.001, "distance {d}");
    }

    /// Build an in-memory chunk table with `n` embeddings, where chunk `i`
    /// points a little further from the query vector than chunk `i-1`.
    fn seeded_conn(n: usize) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE memory_chunks (
               id TEXT PRIMARY KEY, folder TEXT, path TEXT, source TEXT,
               start_line INTEGER, end_line INTEGER, hash TEXT, text TEXT,
               embedding BLOB, model TEXT);",
        )
        .unwrap();
        for i in 0..n {
            // Tilt each chunk a little further off the query vector, without
            // ever wrapping past a right angle: a sweep that circles back
            // lands later chunks on top of earlier ones, and
            // `normalize_vec_results` then sees no distance range and
            // deliberately reports nothing.
            let emb: Vec<f32> = vec![1.0, (i as f32) * 0.2];
            let blob: Vec<u8> = emb.iter().flat_map(|f| f.to_le_bytes()).collect();
            conn.execute(
                "INSERT INTO memory_chunks
                 (id, folder, path, source, start_line, end_line, hash, text, embedding)
                 VALUES (?1, 'main', 'notes.md', 'memory', 1, 2, 'h', ?2, ?3)",
                params![format!("c{i:04}"), format!("chunk {i}"), blob],
            )
            .unwrap();
        }
        conn
    }

    #[test]
    fn blob_scan_returns_the_nearest_chunks_in_order() {
        let conn = seeded_conn(200);
        let results = blob_scan(&conn, "main", &[1.0, 0.0], "all", 5).unwrap();
        assert!(!results.is_empty());
        // Chunk 0 sits exactly on the query vector, so it must come first and
        // the rest must follow by increasing angle.
        assert_eq!(results[0].id, "c0000");
        let ids: Vec<&str> = results.iter().map(|r| r.id.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "results should already run nearest-first");
        // The winners carry the row data, which the scan itself never loaded.
        assert_eq!(results[0].text, "chunk 0");
        assert_eq!(results[0].path, "notes.md");
    }

    #[test]
    fn blob_scan_keeps_only_the_bounded_top_k() {
        // The heap holds 2*limit candidates; a folder of 200 chunks must not
        // come back as 200 results.
        let conn = seeded_conn(200);
        let results = blob_scan(&conn, "main", &[1.0, 0.0], "all", 3).unwrap();
        assert!(results.len() <= 6, "got {} results", results.len());
    }

    #[test]
    fn blob_scan_respects_the_source_filter() {
        let conn = seeded_conn(10);
        conn.execute(
            "UPDATE memory_chunks SET source = 'session' WHERE id = 'c0000'",
            [],
        )
        .unwrap();
        let results = blob_scan(&conn, "main", &[1.0, 0.0], "memory", 5).unwrap();
        assert!(
            results.iter().all(|r| r.id != "c0000"),
            "the reclassified chunk must not come back under source=memory"
        );
    }

    #[test]
    fn vec0_is_skipped_when_its_table_is_absent() {
        // Every install today is in this state; probing it per query cost a
        // failed statement prepare each time.
        let conn = seeded_conn(1);
        assert!(!vec0_table_exists(&conn));
        conn.execute_batch(
            "CREATE TABLE memory_chunks_vec (chunk_id TEXT, embedding BLOB, distance REAL);",
        )
        .unwrap();
        assert!(vec0_table_exists(&conn));
    }

    #[test]
    fn cosine_distance_orthogonal() {
        let a = vec![1.0, 0.0];
        let b: Vec<u8> = vec![0.0f32, 1.0f32]
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        let d = cosine_distance(&a, &b).unwrap();
        assert!((d - 1.0).abs() < 0.001, "distance {d}");
    }
}
