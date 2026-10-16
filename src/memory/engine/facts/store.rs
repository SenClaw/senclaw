//! The fact store: self-contained statements with the time they happened, the
//! entities they mention, and a vector, in SQLite.
//!
//! A *fact* is one or two sentences that make sense alone ("Emily, the user's
//! roommate, moved to Hue in March 2026") — unlike a graph triplet, which
//! keeps who/what and loses why/when. Facts are scoped by `bank`, the agent
//! folder, the same scope the curated notes and the graph use.
//!
//! Vector search is an exact scan over the embedding BLOBs, as it is for
//! `memory_chunks` (docs/memory.md, "Vì sao chưa dùng sqlite-vec"): it stays
//! cheap well past the 50 000-row threshold that decision set, and it needs no
//! native extension. Revisit when `mem_facts` is that large.
//!
//! The tables are additive (`CREATE … IF NOT EXISTS`); nothing existing is
//! migrated.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;

use anyhow::{Context, Result};
use rusqlite::{params, params_from_iter, Connection};
use sha2::{Digest, Sha256};

use crate::db::Db;
use crate::memory::tokenizer::tokenize_optimized;

/// An entity shared by more facts than this is a hub ("user"): following it
/// links everything to everything and says nothing, so graph expansion skips it.
pub const HUB_ENTITY_FACTS: usize = 500;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS mem_facts (
  id             TEXT PRIMARY KEY,
  bank           TEXT NOT NULL,
  kind           TEXT NOT NULL CHECK (kind IN ('world', 'experience')),
  text           TEXT NOT NULL,
  occurred_start INTEGER,
  occurred_end   INTEGER,
  mentioned_at   INTEGER NOT NULL,
  created_at     INTEGER NOT NULL,
  content_hash   TEXT NOT NULL,
  embedding      BLOB,
  model          TEXT,
  source         TEXT NOT NULL DEFAULT '',
  proof_count    INTEGER NOT NULL DEFAULT 1,
  superseded_by  TEXT,
  superseded_at  INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_mem_facts_hash ON mem_facts(bank, content_hash);
CREATE INDEX IF NOT EXISTS idx_mem_facts_bank_time ON mem_facts(bank, occurred_start);

CREATE VIRTUAL TABLE IF NOT EXISTS mem_facts_fts USING fts5(fact_id UNINDEXED, text);

CREATE TABLE IF NOT EXISTS mem_fact_entities (
  fact_id TEXT NOT NULL REFERENCES mem_facts(id) ON DELETE CASCADE,
  entity  TEXT NOT NULL,
  PRIMARY KEY (fact_id, entity)
);
CREATE INDEX IF NOT EXISTS idx_mem_fact_entities_entity ON mem_fact_entities(entity);

CREATE TABLE IF NOT EXISTS mem_links (
  from_id   TEXT NOT NULL REFERENCES mem_facts(id) ON DELETE CASCADE,
  to_id     TEXT NOT NULL REFERENCES mem_facts(id) ON DELETE CASCADE,
  link_type TEXT NOT NULL CHECK (link_type IN ('semantic', 'temporal')),
  weight    REAL NOT NULL,
  PRIMARY KEY (from_id, to_id, link_type)
);
CREATE INDEX IF NOT EXISTS idx_mem_links_to ON mem_links(to_id);

CREATE TRIGGER IF NOT EXISTS mem_facts_ad AFTER DELETE ON mem_facts BEGIN
  DELETE FROM mem_facts_fts WHERE fact_id = old.id;
END;

-- Forgetting a fact that replaced others brings them back: the replacement was
-- the thing that was wrong.
CREATE TRIGGER IF NOT EXISTS mem_facts_restore AFTER DELETE ON mem_facts BEGIN
  UPDATE mem_facts SET superseded_by = NULL, superseded_at = NULL WHERE superseded_by = old.id;
END;
"#;

/// Columns added after the first release of the table. `CREATE TABLE IF NOT
/// EXISTS` does not touch an existing table, so each is added when missing.
const LATE_COLUMNS: &[(&str, &str)] = &[
    ("proof_count", "INTEGER NOT NULL DEFAULT 1"),
    ("superseded_by", "TEXT"),
    ("superseded_at", "INTEGER"),
];

/// Create the fact tables. Idempotent.
pub fn apply_fact_schema(conn: &Connection) -> Result<()> {
    // Columns first: the schema script creates an index and a trigger that name
    // them, and on an older table they must exist by then.
    add_missing_columns(conn)?;
    conn.execute_batch(SCHEMA).context("create mem_* tables")
}

fn add_missing_columns(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(mem_facts)")?;
    let have: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<std::result::Result<_, _>>()?;
    if have.is_empty() {
        return Ok(()); // no table yet: the script below creates it whole
    }
    for (name, ddl) in LATE_COLUMNS {
        if !have.iter().any(|c| c == name) {
            conn.execute(
                &format!("ALTER TABLE mem_facts ADD COLUMN {name} {ddl}"),
                [],
            )
            .with_context(|| format!("add mem_facts.{name}"))?;
        }
    }
    Ok(())
}

// ===== Types =====

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactKind {
    /// About the user, other people, places, things — including the user's
    /// own preferences and corrections, even when said to the assistant.
    World,
    /// What the assistant itself did, tried, decided or learned.
    Experience,
}

impl FactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FactKind::World => "world",
            FactKind::Experience => "experience",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "world" => Some(FactKind::World),
            "experience" | "assistant" => Some(FactKind::Experience),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewFact {
    pub text: String,
    pub kind: FactKind,
    /// Unix seconds, inclusive start / exclusive end, of when it happened.
    /// `None` for a standing fact or preference.
    pub occurred: Option<(i64, i64)>,
    /// When the source said it — how current the statement is.
    pub mentioned_at: i64,
    /// Entity keys (see [`entity_key`]).
    pub entities: Vec<String>,
    pub embedding: Option<Vec<f32>>,
    /// Which embedding model produced `embedding`; vectors from different
    /// models are not comparable.
    pub model: Option<String>,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredFact {
    pub id: String,
    pub kind: FactKind,
    pub text: String,
    pub occurred_start: Option<i64>,
    pub occurred_end: Option<i64>,
    pub mentioned_at: i64,
    pub entities: Vec<String>,
    /// How many times the same statement has been made. Said again, a fact
    /// counts for more.
    pub proof_count: u32,
    /// The fact that replaced this one, when the user changed their mind or
    /// corrected it. A replaced fact is kept, for history, and not recalled.
    pub superseded_by: Option<String>,
}

impl StoredFact {
    /// The instant that places the fact in time: when it happened, else when
    /// it was said.
    pub fn anchor(&self) -> i64 {
        self.occurred_start.unwrap_or(self.mentioned_at)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inserted {
    New(String),
    /// The same statement was already stored; its `mentioned_at` was brought
    /// forward instead of storing it twice.
    Existing(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkType {
    Semantic,
    Temporal,
}

impl LinkType {
    fn as_str(self) -> &'static str {
        match self {
            LinkType::Semantic => "semantic",
            LinkType::Temporal => "temporal",
        }
    }
}

/// A fact reached through a stored link.
#[derive(Debug, Clone, PartialEq)]
pub struct Linked {
    pub id: String,
    pub link_type: String,
    pub weight: f32,
}

// ===== Normalisation =====

/// Case-folded, whitespace-collapsed form of an entity name, the key facts are
/// joined on. Diacritics stay: "Hà Nội" and "Ha Noi" are the writer's choice,
/// and folding them would merge "Mai" with "Mải".
pub fn entity_key(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn statement_hash(text: &str) -> String {
    let normalised = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    hex::encode(Sha256::digest(normalised.as_bytes()))
}

pub fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn from_blob(b: &[u8]) -> Option<Vec<f32>> {
    if b.len() % 4 != 0 {
        return None;
    }
    Some(
        b.chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0f32, 0f32, 0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = (na * nb).sqrt();
    if denom <= f32::EPSILON {
        0.0
    } else {
        dot / denom
    }
}

// ===== Store =====

#[derive(Clone)]
pub struct FactStore {
    db: Arc<Db>,
}

/// Min-heap entry for the bounded top-k of a vector scan.
struct Scored(f32, String);

impl PartialEq for Scored {
    fn eq(&self, o: &Self) -> bool {
        self.0 == o.0
    }
}
impl Eq for Scored {}
impl PartialOrd for Scored {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Scored {
    fn cmp(&self, o: &Self) -> Ordering {
        // Reversed: `BinaryHeap` is a max-heap, we want to pop the lowest.
        o.0.total_cmp(&self.0)
    }
}

const FACT_COLUMNS: &str = "f.id, f.kind, f.text, f.occurred_start, f.occurred_end, f.mentioned_at, f.proof_count, f.superseded_by";

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

impl FactStore {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }

    /// Store a fact, or recognise it as one already stored (same normalised
    /// text in the same bank) and only refresh when it was last said.
    pub fn insert(&self, bank: &str, f: &NewFact) -> Result<Inserted> {
        self.db.with_conn_mut(|conn| {
            let tx = conn.transaction()?;
            let hash = statement_hash(&f.text);
            let existing: Option<String> = tx
                .query_row(
                    "SELECT id FROM mem_facts WHERE bank = ?1 AND content_hash = ?2",
                    params![bank, hash],
                    |r| r.get(0),
                )
                .ok();
            if let Some(id) = existing {
                tx.execute(
                    "UPDATE mem_facts SET mentioned_at = MAX(mentioned_at, ?1), proof_count = proof_count + 1 WHERE id = ?2",
                    params![f.mentioned_at, id],
                )?;
                tx.commit()?;
                return Ok(Inserted::Existing(id));
            }

            let id = uuid::Uuid::new_v4().to_string();
            tx.execute(
                "INSERT INTO mem_facts (id, bank, kind, text, occurred_start, occurred_end, \
                 mentioned_at, created_at, content_hash, embedding, model, source) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    id,
                    bank,
                    f.kind.as_str(),
                    f.text,
                    f.occurred.map(|o| o.0),
                    f.occurred.map(|o| o.1),
                    f.mentioned_at,
                    chrono::Utc::now().timestamp(),
                    hash,
                    f.embedding.as_deref().map(to_blob),
                    f.model,
                    f.source,
                ],
            )?;
            tx.execute(
                "INSERT INTO mem_facts_fts (fact_id, text) VALUES (?1, ?2)",
                params![id, tokenize_optimized(&f.text, false).join(" ")],
            )?;
            for entity in &f.entities {
                tx.execute(
                    "INSERT OR IGNORE INTO mem_fact_entities (fact_id, entity) VALUES (?1, ?2)",
                    params![id, entity],
                )?;
            }
            tx.commit()?;
            Ok(Inserted::New(id))
        })
    }

    /// Bring a fact's `mentioned_at` forward (never back) and count the repeat:
    /// it was said again.
    pub fn touch(&self, id: &str, mentioned_at: i64) -> Result<()> {
        self.db.with_conn(|c| {
            c.execute(
                "UPDATE mem_facts SET mentioned_at = MAX(mentioned_at, ?1), proof_count = proof_count + 1 WHERE id = ?2",
                params![mentioned_at, id],
            )?;
            Ok(())
        })
    }

    /// Mark `old` as replaced by `new`. Returns whether anything changed: a fact
    /// already replaced, one that does not exist, or a fact "replacing" itself
    /// is left alone. The replaced fact stays in the store.
    pub fn supersede(&self, bank: &str, old: &str, new: &str, at: i64) -> Result<bool> {
        if old == new {
            return Ok(false);
        }
        self.db.with_conn(|c| {
            Ok(c.execute(
                "UPDATE mem_facts SET superseded_by = ?1, superseded_at = ?2 \
                 WHERE id = ?3 AND bank = ?4 AND superseded_by IS NULL \
                   AND EXISTS (SELECT 1 FROM mem_facts WHERE id = ?1 AND bank = ?4)",
                params![new, at, old, bank],
            )? > 0)
        })
    }

    pub fn delete(&self, id: &str) -> Result<bool> {
        self.db
            .with_conn(|c| Ok(c.execute("DELETE FROM mem_facts WHERE id = ?1", params![id])? > 0))
    }

    /// Delete a fact only if it belongs to `bank`. A caller that names a bank
    /// must not be able to reach another agent's memory by guessing an id.
    pub fn delete_in_bank(&self, bank: &str, id: &str) -> Result<bool> {
        self.db.with_conn(|c| {
            Ok(c.execute(
                "DELETE FROM mem_facts WHERE id = ?1 AND bank = ?2",
                params![id, bank],
            )? > 0)
        })
    }

    /// The newest facts of the bank, by when they were last said. Facts that
    /// were replaced are left out unless `with_history` asks for them.
    pub fn recent(&self, bank: &str, limit: usize, with_history: bool) -> Result<Vec<StoredFact>> {
        self.db.with_conn(|c| {
            let sql = format!(
                "SELECT {FACT_COLUMNS} FROM mem_facts f WHERE f.bank = ?1 \
                   AND (?3 OR f.superseded_by IS NULL) \
                 ORDER BY f.mentioned_at DESC, f.created_at DESC, f.id LIMIT ?2"
            );
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt
                .query_map(params![bank, limit as i64, with_history], row_to_fact)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            hydrate_entities(c, rows)
        })
    }

    pub fn count(&self, bank: &str) -> Result<usize> {
        self.db.with_conn(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM mem_facts WHERE bank = ?1",
                params![bank],
                |r| r.get::<_, i64>(0),
            )? as usize)
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<StoredFact>> {
        self.db
            .with_conn(|c| Ok(load_facts(c, &[id.to_string()])?.into_iter().next()))
    }

    pub fn add_link(&self, from: &str, to: &str, kind: LinkType, weight: f32) -> Result<()> {
        if from == to {
            return Ok(());
        }
        self.db.with_conn(|c| {
            c.execute(
                "INSERT INTO mem_links (from_id, to_id, link_type, weight) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(from_id, to_id, link_type) DO UPDATE SET weight = MAX(weight, excluded.weight)",
                params![from, to, kind.as_str(), weight],
            )?;
            Ok(())
        })
    }

    /// Facts joined to any of `ids` by a stored link, in either direction.
    pub fn linked(&self, bank: &str, ids: &[String]) -> Result<Vec<Linked>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.db.with_conn(|c| {
            let ph = placeholders(ids.len());
            let sql = format!(
                "SELECT l.to_id, l.link_type, l.weight FROM mem_links l \
                   JOIN mem_facts f ON f.id = l.to_id AND f.bank = ? AND f.superseded_by IS NULL WHERE l.from_id IN ({ph}) \
                 UNION ALL \
                 SELECT l.from_id, l.link_type, l.weight FROM mem_links l \
                   JOIN mem_facts f ON f.id = l.from_id AND f.bank = ? AND f.superseded_by IS NULL WHERE l.to_id IN ({ph})"
            );
            let mut stmt = c.prepare(&sql)?;
            let args: Vec<&str> = std::iter::once(bank)
                .chain(ids.iter().map(String::as_str))
                .chain(std::iter::once(bank))
                .chain(ids.iter().map(String::as_str))
                .collect();
            let rows = stmt.query_map(params_from_iter(args), |r| {
                Ok(Linked {
                    id: r.get(0)?,
                    link_type: r.get(1)?,
                    weight: r.get::<_, f64>(2)? as f32,
                })
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Facts of the bank whose date lies within `hours` of `anchor`, nearest
    /// first, for temporal links.
    pub fn near_in_time(
        &self,
        bank: &str,
        anchor: i64,
        hours: i64,
        limit: usize,
        exclude: &str,
    ) -> Result<Vec<StoredFact>> {
        self.db.with_conn(|c| {
            let sql = format!(
                "SELECT {FACT_COLUMNS} FROM mem_facts f \
                 WHERE f.bank = ?1 AND f.superseded_by IS NULL AND f.occurred_start IS NOT NULL AND f.id != ?2 \
                   AND ABS(f.occurred_start - ?3) <= ?4 \
                 ORDER BY ABS(f.occurred_start - ?3) LIMIT ?5"
            );
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt
                .query_map(
                    params![bank, exclude, anchor, hours * 3600, limit as i64],
                    row_to_fact,
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            hydrate_entities(c, rows)
        })
    }

    /// Best matches by BM25, optionally only facts whose date overlaps
    /// `window` (`[start, end)` in unix seconds).
    pub fn search_keyword(
        &self,
        bank: &str,
        fts_match: &str,
        window: Option<(i64, i64)>,
        limit: usize,
    ) -> Result<Vec<StoredFact>> {
        self.db.with_conn(|c| {
            let (start, end) = window.unwrap_or((i64::MIN, i64::MAX));
            let sql = format!(
                "SELECT {FACT_COLUMNS} FROM mem_facts_fts x \
                   JOIN mem_facts f ON f.id = x.fact_id \
                 WHERE mem_facts_fts MATCH ?1 AND f.bank = ?2 AND f.superseded_by IS NULL \
                   AND COALESCE(f.occurred_end, f.occurred_start, f.mentioned_at) >= ?3 \
                   AND COALESCE(f.occurred_start, f.mentioned_at) < ?4 \
                 ORDER BY bm25(mem_facts_fts) LIMIT ?5"
            );
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt
                .query_map(
                    params![fts_match, bank, start, end, limit as i64],
                    row_to_fact,
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            hydrate_entities(c, rows)
        })
    }

    /// Nearest facts to `query` by cosine similarity, at least `min_sim`,
    /// among those embedded by `model`. An exact scan; see the module note.
    pub fn search_vector(
        &self,
        bank: &str,
        model: &str,
        query: &[f32],
        min_sim: f32,
        limit: usize,
    ) -> Result<Vec<(StoredFact, f32)>> {
        if limit == 0 || query.is_empty() {
            return Ok(Vec::new());
        }
        self.db.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, embedding FROM mem_facts \
                 WHERE bank = ?1 AND model = ?2 AND embedding IS NOT NULL AND superseded_by IS NULL",
            )?;
            let mut rows = stmt.query(params![bank, model])?;
            let mut heap: BinaryHeap<Scored> = BinaryHeap::with_capacity(limit + 1);
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let blob: Vec<u8> = row.get(1)?;
                let Some(v) = from_blob(&blob) else { continue };
                let sim = cosine(query, &v);
                if sim < min_sim {
                    continue;
                }
                heap.push(Scored(sim, id));
                if heap.len() > limit {
                    heap.pop();
                }
            }
            let mut best: Vec<Scored> = heap.into_vec();
            best.sort_by(|a, b| b.0.total_cmp(&a.0));
            let ids: Vec<String> = best.iter().map(|s| s.1.clone()).collect();
            let sims: HashMap<&str, f32> = best.iter().map(|s| (s.1.as_str(), s.0)).collect();
            let facts = load_facts(c, &ids)?;
            Ok(facts
                .into_iter()
                .map(|f| {
                    let s = sims.get(f.id.as_str()).copied().unwrap_or(0.0);
                    (f, s)
                })
                .collect())
        })
    }

    /// Facts sharing entities with `seeds`, most shared first, skipping hub
    /// entities. Returns `(fact id, number of shared entities)`.
    pub fn related_by_entity(
        &self,
        bank: &str,
        seeds: &[String],
        limit: usize,
    ) -> Result<Vec<(String, usize)>> {
        if seeds.is_empty() {
            return Ok(Vec::new());
        }
        self.db.with_conn(|c| {
            let ph = placeholders(seeds.len());
            let sql = format!(
                "SELECT e2.fact_id, COUNT(DISTINCT e2.entity) AS shared \
                   FROM mem_fact_entities e1 \
                   JOIN mem_fact_entities e2 ON e2.entity = e1.entity AND e2.fact_id != e1.fact_id \
                   JOIN mem_facts f ON f.id = e2.fact_id AND f.bank = ? AND f.superseded_by IS NULL \
                  WHERE e1.fact_id IN ({ph}) \
                    AND e1.entity NOT IN ( \
                        SELECT entity FROM mem_fact_entities GROUP BY entity HAVING COUNT(*) > {HUB_ENTITY_FACTS}) \
                    AND e2.fact_id NOT IN ({ph}) \
                  GROUP BY e2.fact_id ORDER BY shared DESC, e2.fact_id LIMIT {limit}"
            );
            let mut stmt = c.prepare(&sql)?;
            let args: Vec<&str> = std::iter::once(bank)
                .chain(seeds.iter().map(String::as_str))
                .chain(seeds.iter().map(String::as_str))
                .collect();
            let rows = stmt.query_map(params_from_iter(args), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Facts whose date overlaps `[start, end)`, oldest first.
    pub fn in_window(
        &self,
        bank: &str,
        window: (i64, i64),
        limit: usize,
    ) -> Result<Vec<StoredFact>> {
        self.db.with_conn(|c| {
            let sql = format!(
                "SELECT {FACT_COLUMNS} FROM mem_facts f WHERE f.bank = ?1 \
                   AND f.superseded_by IS NULL \
                   AND COALESCE(f.occurred_end, f.occurred_start, f.mentioned_at) >= ?2 \
                   AND COALESCE(f.occurred_start, f.mentioned_at) < ?3 \
                 ORDER BY COALESCE(f.occurred_start, f.mentioned_at) LIMIT ?4"
            );
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt
                .query_map(params![bank, window.0, window.1, limit as i64], row_to_fact)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            hydrate_entities(c, rows)
        })
    }

    /// Load facts by id, keeping the order of `ids`.
    pub fn get_many(&self, ids: &[String]) -> Result<Vec<StoredFact>> {
        self.db.with_conn(|c| load_facts(c, ids))
    }
}

fn row_to_fact(r: &rusqlite::Row<'_>) -> rusqlite::Result<StoredFact> {
    let kind: String = r.get(1)?;
    Ok(StoredFact {
        id: r.get(0)?,
        kind: FactKind::parse(&kind).unwrap_or(FactKind::World),
        text: r.get(2)?,
        occurred_start: r.get(3)?,
        occurred_end: r.get(4)?,
        mentioned_at: r.get(5)?,
        entities: Vec::new(),
        proof_count: r.get::<_, i64>(6)?.max(1) as u32,
        superseded_by: r.get(7)?,
    })
}

fn load_facts(conn: &Connection, ids: &[String]) -> Result<Vec<StoredFact>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {FACT_COLUMNS} FROM mem_facts f WHERE f.id IN ({})",
        placeholders(ids.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(ids.iter()), row_to_fact)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut by_id: HashMap<String, StoredFact> = hydrate_entities(conn, rows)?
        .into_iter()
        .map(|f| (f.id.clone(), f))
        .collect();
    Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
}

fn hydrate_entities(conn: &Connection, mut facts: Vec<StoredFact>) -> Result<Vec<StoredFact>> {
    if facts.is_empty() {
        return Ok(facts);
    }
    let sql = format!(
        "SELECT fact_id, entity FROM mem_fact_entities WHERE fact_id IN ({}) ORDER BY entity",
        placeholders(facts.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(facts.iter().map(|f| f.id.clone())), |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut by_fact: HashMap<String, Vec<String>> = HashMap::new();
    for row in rows {
        let (id, entity) = row?;
        by_fact.entry(id).or_default().push(entity);
    }
    for f in &mut facts {
        f.entities = by_fact.remove(&f.id).unwrap_or_default();
    }
    Ok(facts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn store() -> FactStore {
        let db = Arc::new(Db::open_in_memory(&Config::from_env()).unwrap());
        FactStore::new(db)
    }

    fn fact(text: &str, entities: &[&str]) -> NewFact {
        NewFact {
            text: text.into(),
            kind: FactKind::World,
            occurred: None,
            mentioned_at: 1_000,
            entities: entities.iter().map(|e| entity_key(e)).collect(),
            embedding: None,
            model: None,
            source: "test".into(),
        }
    }

    fn new_id(r: Inserted) -> String {
        match r {
            Inserted::New(id) => id,
            other => panic!("expected a new fact, got {other:?}"),
        }
    }

    #[test]
    fn the_schema_is_part_of_every_database() {
        let s = store();
        assert_eq!(s.count("main").unwrap(), 0);
    }

    #[test]
    fn a_fact_round_trips_with_its_entities() {
        let s = store();
        let mut f = fact("Emily moved to Hue in March 2026", &["Emily", "Hue"]);
        f.occurred = Some((1_772_323_200, 1_775_001_600));
        let id = new_id(s.insert("main", &f).unwrap());
        let got = s.get(&id).unwrap().unwrap();
        assert_eq!(got.text, "Emily moved to Hue in March 2026");
        assert_eq!(got.occurred_start, Some(1_772_323_200));
        assert_eq!(got.entities, vec!["emily", "hue"]);
        assert_eq!(got.anchor(), 1_772_323_200);
    }

    #[test]
    fn the_same_statement_is_stored_once_and_its_date_moves_forward() {
        let s = store();
        let first = new_id(
            s.insert("main", &fact("User prefers dark mode", &["user"]))
                .unwrap(),
        );
        let mut again = fact("  user prefers   DARK mode ", &["user"]);
        again.mentioned_at = 5_000;
        assert_eq!(
            s.insert("main", &again).unwrap(),
            Inserted::Existing(first.clone())
        );
        assert_eq!(s.count("main").unwrap(), 1);
        assert_eq!(s.get(&first).unwrap().unwrap().mentioned_at, 5_000);
        // An older repeat never moves it back.
        let mut older = fact("User prefers dark mode", &[]);
        older.mentioned_at = 10;
        s.insert("main", &older).unwrap();
        assert_eq!(s.get(&first).unwrap().unwrap().mentioned_at, 5_000);
    }

    #[test]
    fn banks_do_not_see_each_other() {
        let s = store();
        s.insert("a", &fact("Secret plan of agent a", &[])).unwrap();
        let hit = s.search_keyword("b", "plan", None, 5).unwrap();
        assert!(hit.is_empty());
        assert_eq!(s.count("b").unwrap(), 0);
        // The same statement may exist in both banks.
        assert!(matches!(
            s.insert("b", &fact("Secret plan of agent a", &[])).unwrap(),
            Inserted::New(_)
        ));
    }

    #[test]
    fn recent_lists_newest_first_and_only_the_banks_own() {
        let s = store();
        for (text, at) in [("oldest", 10), ("newest", 30), ("middle", 20)] {
            let mut f = fact(text, &[]);
            f.mentioned_at = at;
            s.insert("main", &f).unwrap();
        }
        s.insert("other", &fact("not mine", &[])).unwrap();
        let got: Vec<String> = s
            .recent("main", 10, false)
            .unwrap()
            .into_iter()
            .map(|f| f.text)
            .collect();
        assert_eq!(got, ["newest", "middle", "oldest"]);
        assert_eq!(s.recent("main", 2, false).unwrap().len(), 2);
    }

    #[test]
    fn delete_in_bank_refuses_another_banks_fact() {
        let s = store();
        let id = new_id(s.insert("a", &fact("agent a private fact", &[])).unwrap());
        assert!(
            !s.delete_in_bank("b", &id).unwrap(),
            "wrong bank must not delete"
        );
        assert_eq!(s.count("a").unwrap(), 1);
        assert!(s.delete_in_bank("a", &id).unwrap());
        assert_eq!(s.count("a").unwrap(), 0);
    }

    #[test]
    fn saying_it_again_counts_as_more_evidence() {
        let s = store();
        let id = new_id(
            s.insert("main", &fact("Sen prefers dark mode", &[]))
                .unwrap(),
        );
        assert_eq!(s.get(&id).unwrap().unwrap().proof_count, 1);
        s.insert("main", &fact("sen prefers  DARK mode", &[]))
            .unwrap();
        s.touch(&id, 50).unwrap();
        assert_eq!(s.get(&id).unwrap().unwrap().proof_count, 3);
    }

    #[test]
    fn a_replaced_fact_is_kept_but_never_recalled() {
        let s = store();
        let old = new_id(
            s.insert(
                "main",
                &fact("User likes React for frontend work", &["user", "react"]),
            )
            .unwrap(),
        );
        let new = new_id(
            s.insert(
                "main",
                &fact(
                    "User moved to Vue and dropped React",
                    &["user", "vue", "react"],
                ),
            )
            .unwrap(),
        );
        assert!(s.supersede("main", &old, &new, 100).unwrap());

        // Gone from every way of finding a fact …
        let kw = s.search_keyword("main", "react", None, 10).unwrap();
        assert_eq!(
            kw.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
            [new.as_str()]
        );
        assert_eq!(s.recent("main", 10, false).unwrap().len(), 1);
        assert!(s
            .in_window("main", (0, 1_000_000), 10)
            .unwrap()
            .iter()
            .all(|f| f.id != old));
        // … still there for history, and still addressable.
        let all = s.recent("main", 10, true).unwrap();
        assert_eq!(all.len(), 2);
        let kept = s.get(&old).unwrap().unwrap();
        assert_eq!(kept.superseded_by.as_deref(), Some(new.as_str()));
        assert_eq!(s.count("main").unwrap(), 2);
    }

    #[test]
    fn replaced_facts_drop_out_of_entity_expansion_links_and_vectors() {
        let s = store();
        let seed = new_id(
            s.insert(
                "main",
                &fact("Lan runs the Falcon project", &["lan", "falcon"]),
            )
            .unwrap(),
        );
        let mut vec_fact = fact("Lan is on leave until November", &["lan"]);
        vec_fact.embedding = Some(vec![1.0, 0.0]);
        vec_fact.model = Some("m".into());
        let leave = new_id(s.insert("main", &vec_fact).unwrap());
        let back = new_id(
            s.insert("main", &fact("Lan is back at work", &["lan"]))
                .unwrap(),
        );
        s.add_link(&seed, &leave, LinkType::Semantic, 0.9).unwrap();
        assert_eq!(
            s.related_by_entity("main", &[seed.clone()], 10)
                .unwrap()
                .len(),
            2
        );

        assert!(s.supersede("main", &leave, &back, 5).unwrap());
        let related: Vec<String> = s
            .related_by_entity("main", &[seed.clone()], 10)
            .unwrap()
            .into_iter()
            .map(|r| r.0)
            .collect();
        assert_eq!(related, [back.clone()]);
        assert!(s.linked("main", &[seed]).unwrap().is_empty());
        assert!(s
            .search_vector("main", "m", &[1.0, 0.0], 0.0, 5)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn supersede_refuses_what_makes_no_sense() {
        let s = store();
        let a = new_id(s.insert("main", &fact("fact a", &[])).unwrap());
        let b = new_id(s.insert("main", &fact("fact b", &[])).unwrap());
        let other = new_id(
            s.insert("other", &fact("fact in another bank", &[]))
                .unwrap(),
        );
        assert!(!s.supersede("main", &a, &a, 1).unwrap(), "not itself");
        assert!(
            !s.supersede("main", &a, "no-such-fact", 1).unwrap(),
            "not by something that does not exist"
        );
        assert!(!s.supersede("main", "no-such-fact", &b, 1).unwrap());
        assert!(
            !s.supersede("main", &a, &other, 1).unwrap(),
            "not by another bank's fact"
        );
        assert!(
            !s.supersede("other", &a, &other, 1).unwrap(),
            "not another bank's fact"
        );
        assert!(s.supersede("main", &a, &b, 1).unwrap());
        assert!(!s.supersede("main", &a, &b, 2).unwrap(), "already replaced");
    }

    #[test]
    fn forgetting_the_replacement_brings_the_replaced_fact_back() {
        let s = store();
        let old = new_id(
            s.insert("main", &fact("Alice works at Google", &["alice"]))
                .unwrap(),
        );
        let wrong = new_id(
            s.insert("main", &fact("Alice works at Meta", &["alice"]))
                .unwrap(),
        );
        s.supersede("main", &old, &wrong, 1).unwrap();
        assert_eq!(s.search_keyword("main", "alice", None, 5).unwrap().len(), 1);

        assert!(s.delete(&wrong).unwrap());
        let back = s.search_keyword("main", "alice", None, 5).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id, old);
        assert_eq!(back[0].superseded_by, None);
    }

    #[test]
    fn a_table_from_before_the_new_columns_is_upgraded_in_place() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE mem_facts (
               id TEXT PRIMARY KEY, bank TEXT NOT NULL,
               kind TEXT NOT NULL CHECK (kind IN ('world', 'experience')),
               text TEXT NOT NULL, occurred_start INTEGER, occurred_end INTEGER,
               mentioned_at INTEGER NOT NULL, created_at INTEGER NOT NULL,
               content_hash TEXT NOT NULL, embedding BLOB, model TEXT,
               source TEXT NOT NULL DEFAULT '');
             INSERT INTO mem_facts (id, bank, kind, text, mentioned_at, created_at, content_hash)
               VALUES ('old1', 'main', 'world', 'an old fact', 1, 1, 'h');",
        )
        .unwrap();
        apply_fact_schema(&conn).unwrap();
        apply_fact_schema(&conn).unwrap(); // idempotent
        let (proof, by): (i64, Option<String>) = conn
            .query_row(
                "SELECT proof_count, superseded_by FROM mem_facts WHERE id = 'old1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((proof, by), (1, None), "existing rows read as unrevised");
    }

    #[test]
    fn keyword_search_ranks_and_respects_the_date_window() {
        let s = store();
        let mut old = fact("Alice deployed the billing service", &["alice"]);
        old.occurred = Some((100, 200));
        let mut recent = fact("Alice deployed the search service", &["alice"]);
        recent.occurred = Some((1_000, 1_100));
        s.insert("main", &old).unwrap();
        s.insert("main", &recent).unwrap();

        let all = s
            .search_keyword("main", "alice OR deployed", None, 10)
            .unwrap();
        assert_eq!(all.len(), 2);
        let windowed = s
            .search_keyword("main", "alice OR deployed", Some((900, 1_200)), 10)
            .unwrap();
        assert_eq!(windowed.len(), 1);
        assert!(windowed[0].text.contains("search"));
    }

    #[test]
    fn a_fact_without_a_date_is_placed_by_when_it_was_said() {
        let s = store();
        let mut f = fact("User likes green tea", &[]);
        f.mentioned_at = 5_000;
        s.insert("main", &f).unwrap();
        assert_eq!(s.in_window("main", (4_000, 6_000), 10).unwrap().len(), 1);
        assert!(s.in_window("main", (0, 4_000), 10).unwrap().is_empty());
    }

    #[test]
    fn vector_search_is_exact_and_ignores_other_models() {
        let s = store();
        let mut near = fact("near fact", &[]);
        near.embedding = Some(vec![1.0, 0.0, 0.0]);
        near.model = Some("m1".into());
        let mut far = fact("far fact", &[]);
        far.embedding = Some(vec![0.0, 1.0, 0.0]);
        far.model = Some("m1".into());
        let mut other_model = fact("other model fact", &[]);
        other_model.embedding = Some(vec![1.0, 0.0, 0.0]);
        other_model.model = Some("m2".into());
        for f in [&near, &far, &other_model] {
            s.insert("main", f).unwrap();
        }

        let got = s
            .search_vector("main", "m1", &[0.9, 0.1, 0.0], 0.0, 5)
            .unwrap();
        assert_eq!(got.len(), 2, "the m2 vector is not comparable");
        assert_eq!(got[0].0.text, "near fact");
        assert!(got[0].1 > got[1].1);
        // A floor drops the far one.
        let floor = s
            .search_vector("main", "m1", &[0.9, 0.1, 0.0], 0.5, 5)
            .unwrap();
        assert_eq!(floor.len(), 1);
        // The bound keeps only the best.
        let top1 = s
            .search_vector("main", "m1", &[0.0, 1.0, 0.0], 0.0, 1)
            .unwrap();
        assert_eq!(top1[0].0.text, "far fact");
    }

    #[test]
    fn entity_expansion_counts_shared_entities_and_skips_the_seeds() {
        let s = store();
        let a = new_id(
            s.insert(
                "main",
                &fact("Alice joined Acme in 2024", &["alice", "acme"]),
            )
            .unwrap(),
        );
        let b = new_id(
            s.insert(
                "main",
                &fact("Acme ships the Falcon product", &["acme", "falcon"]),
            )
            .unwrap(),
        );
        let c = new_id(
            s.insert(
                "main",
                &fact(
                    "Alice leads the Falcon team at Acme",
                    &["alice", "falcon", "acme"],
                ),
            )
            .unwrap(),
        );
        let _unrelated = s.insert("main", &fact("Bob likes tea", &["bob"])).unwrap();

        let related = s.related_by_entity("main", &[a.clone()], 10).unwrap();
        assert_eq!(related, vec![(c.clone(), 2), (b.clone(), 1)]);
        assert!(related.iter().all(|(id, _)| *id != a));
    }

    #[test]
    fn a_hub_entity_does_not_connect_everything() {
        let s = store();
        let seed = new_id(
            s.insert("main", &fact("seed fact about user", &["user"]))
                .unwrap(),
        );
        for i in 0..(HUB_ENTITY_FACTS + 1) {
            s.insert(
                "main",
                &fact(&format!("fact number {i} about user"), &["user"]),
            )
            .unwrap();
        }
        assert!(s.related_by_entity("main", &[seed], 10).unwrap().is_empty());
    }

    #[test]
    fn links_are_followed_in_both_directions_and_never_self_link() {
        let s = store();
        let a = new_id(s.insert("main", &fact("fact a", &[])).unwrap());
        let b = new_id(s.insert("main", &fact("fact b", &[])).unwrap());
        s.add_link(&a, &b, LinkType::Semantic, 0.8).unwrap();
        s.add_link(&a, &a, LinkType::Semantic, 1.0).unwrap();
        let from_a = s.linked("main", &[a.clone()]).unwrap();
        assert_eq!(from_a.len(), 1);
        assert_eq!(from_a[0].id, b);
        let from_b = s.linked("main", &[b.clone()]).unwrap();
        assert_eq!(from_b[0].id, a);
        // Re-adding keeps the stronger weight.
        s.add_link(&a, &b, LinkType::Semantic, 0.6).unwrap();
        assert!((s.linked("main", &[a]).unwrap()[0].weight - 0.8).abs() < 1e-6);
    }

    #[test]
    fn deleting_a_fact_removes_its_search_row_entities_and_links() {
        let s = store();
        let a = new_id(
            s.insert("main", &fact("Alice owns the keys", &["alice"]))
                .unwrap(),
        );
        let b = new_id(
            s.insert("main", &fact("Bob owns the lock", &["bob"]))
                .unwrap(),
        );
        s.add_link(&a, &b, LinkType::Temporal, 0.5).unwrap();
        assert!(s.delete(&a).unwrap());
        assert!(!s.delete(&a).unwrap());
        assert!(s
            .search_keyword("main", "alice", None, 5)
            .unwrap()
            .is_empty());
        assert!(s.linked("main", &[b]).unwrap().is_empty());
    }

    #[test]
    fn near_in_time_orders_by_distance_and_skips_undated_facts() {
        let s = store();
        let day = 86_400;
        let mut f1 = fact("event one", &[]);
        f1.occurred = Some((10 * day, 11 * day));
        let mut f2 = fact("event two", &[]);
        f2.occurred = Some((10 * day + 3600, 11 * day));
        let mut f3 = fact("event far away", &[]);
        f3.occurred = Some((100 * day, 101 * day));
        let id1 = new_id(s.insert("main", &f1).unwrap());
        s.insert("main", &f2).unwrap();
        s.insert("main", &f3).unwrap();
        s.insert("main", &fact("undated", &[])).unwrap();
        let near = s.near_in_time("main", 10 * day, 24, 5, &id1).unwrap();
        assert_eq!(near.len(), 1);
        assert_eq!(near[0].text, "event two");
    }

    #[test]
    fn entity_keys_fold_case_and_spacing_but_not_diacritics() {
        assert_eq!(entity_key("  Hà   Nội "), "hà nội");
        assert_ne!(entity_key("Mai"), entity_key("Mải"));
    }
}
