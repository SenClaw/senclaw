//! Cognitive memory schema — graph of nodes + Hebbian edges.
//!
//! Combines:
//!   * **cognee** — DataPoint nodes, triplet edges, NodeSet scoping,
//!     ingestion via `add → cognify → search`.
//!   * **shodh-memory** — Hebbian strengthen/decay on edges, multi-tier
//!     consolidation (L1 Working / L2 Episodic / L3 Semantic), LTP states
//!     (None / Burst / Weekly / Full), endpoint selectivity, salience.
//!
//! Tables:
//!   * `cog_nodes`      — entities, chunks, summaries (port of DataPoint)
//!   * `cog_edges`      — typed relationships with Hebbian dynamics
//!   * `cog_node_sets`  — per-(group/persona/cowork) scope tags
//!   * `cog_node_tags`  — N:N join nodes ↔ node_sets
//!   * `cog_vec`        — sqlite-vec virtual table for node embeddings
//!                        (created only when `enable_vec=true`)
//!
//! All timestamps are unix-seconds (i64) to match shodh-memory and the rest of
//! the senclaw schema (chrono-free at the storage layer).

use anyhow::Result;
use rusqlite::Connection;

/// Apply cognitive schema. Idempotent.
///
/// `cog_vec` (sqlite-vec) is intentionally **not created here** — it is added
/// by [`apply_cognitive_vec_schema`] once dimensions are known (P2 / MLX
/// embedder), mirroring how `apply_memory_schema` defers `memory_chunks_vec`.
/// ALTER TABLE for existing DBs that pre-date the extraction_state /
/// extracted_at columns. Safe to call on fresh DBs (the CREATE TABLE
/// already includes them; ALTER will error → swallowed).
fn migrate_extraction_state(conn: &Connection) -> Result<()> {
    // Use `column_names` pattern from src/db/schema.rs: check before alter.
    let exists = |name: &str| -> bool {
        let mut stmt = match conn.prepare(&format!("PRAGMA table_info({name})")) {
            Ok(s) => s,
            Err(_) => return false,
        };
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default();
        !cols.is_empty()
    };
    if !exists("cog_nodes") {
        return Ok(());
    }
    let mut stmt = conn.prepare("PRAGMA table_info(cog_nodes)")?;
    let cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<_>>()?;
    if !cols.iter().any(|c| c == "extraction_state") {
        conn.execute(
            "ALTER TABLE cog_nodes ADD COLUMN extraction_state INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
        // Back-fill: every chunk that already has outgoing edges (other
        // than MENTIONS, which are auto-generated) was extracted before
        // this migration ran — mark them `done` so the dedupe-skip gate
        // works on existing data without re-LLMing the whole graph.
        conn.execute(
            "UPDATE cog_nodes SET extraction_state = 1
             WHERE kind = 'chunk'
               AND id IN (SELECT DISTINCT src FROM cog_edges WHERE predicate <> 'MENTIONS')",
            [],
        )?;
    }
    if !cols.iter().any(|c| c == "extracted_at") {
        conn.execute("ALTER TABLE cog_nodes ADD COLUMN extracted_at INTEGER", [])?;
    }
    Ok(())
}

/// Give `cog_edges.valid_to` its documented meaning back.
///
/// Until this migration, `valid_to` carried **two** meanings: the schema
/// called it temporal validity, but the only writer was the decay sweep,
/// which used it as the "dormant" marker (96% of edges on a real database).
/// Any attempt to also use it for "this fact was superseded" would have been
/// undone by `strengthen`, which cleared it on every re-mention — a false
/// fact would come back to life just by being mentioned again.
///
/// So: move every existing marker into the new `archived_at` column and
/// leave `valid_to` empty for world-time only. Unambiguous precisely because
/// `RelationshipEdge::archive` was the sole writer — there is no other kind
/// of value in there to misclassify.
///
/// Idempotent: gated on the column not existing yet.
fn migrate_edge_temporal(conn: &Connection) -> Result<()> {
    let mut stmt = match conn.prepare("PRAGMA table_info(cog_edges)") {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };
    let cols: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<_>>()?;
    if cols.is_empty() {
        return Ok(()); // fresh DB — CREATE TABLE already has the columns
    }
    if !cols.iter().any(|c| c == "archived_at") {
        conn.execute("ALTER TABLE cog_edges ADD COLUMN archived_at INTEGER", [])?;
    }
    if !cols.iter().any(|c| c == "invalidated_by") {
        conn.execute("ALTER TABLE cog_edges ADD COLUMN invalidated_by BLOB", [])?;
    }

    // Repair pass, run on EVERY boot rather than once.
    //
    // The one-shot version was not enough: a daemon built before this split
    // keeps writing archive markers into `valid_to`, and there is always a
    // window where the old binary is still running while a new one starts —
    // an in-place desktop update, a stray `cargo test`, an MCP subprocess
    // from the previous bundle. Those rows would then read as "superseded
    // facts" and vanish from every present-tense recall.
    //
    // Telling them apart is exact, not heuristic: supersession always writes
    // `invalidated_by` alongside `valid_to` (see `RelationshipEdge::invalidate`),
    // so a closed interval with no culprit can only have come from decay.
    let repaired = conn.execute(
        "UPDATE cog_edges
            SET archived_at = COALESCE(archived_at, valid_to),
                valid_to    = NULL
          WHERE valid_to IS NOT NULL AND invalidated_by IS NULL",
        [],
    )?;
    if repaired > 0 {
        tracing::info!(
            edges = repaired,
            "[cognitive] moved decay archive markers out of valid_to into archived_at"
        );
    }
    Ok(())
}

/// Predicates whose object replaces the previous one. Measured against a real
/// graph (docs/temporal-graph-research.md): prices and status readings churn
/// constantly and every reading used to sit in the graph as an equally valid
/// "current" fact.
const SEED_SINGLE: &[&str] = &[
    // measurements / readings
    "price",
    "sell_price",
    "buy_price",
    "cost",
    "exchange_rate",
    "rate",
    "has_uptime",
    "has_used_ram",
    "has_cpu",
    "temperature",
    // state
    "has_status",
    "status",
    "state",
    "version",
    "current_version",
    // identity / attributes
    "name",
    "full_name",
    "age",
    "birthday",
    "born_in",
    "lives_in",
    "located_in",
    "address",
    "email",
    "phone",
    "works_at",
    "works_for",
    "job_title",
    "role",
    "owner",
    "owned_by",
    "capital_of",
];

/// Predicates that are legitimately many-valued. Unknown predicates already
/// default to multi, so these are here to make the intent explicit in the
/// table (and to stop a future classifier from "fixing" them).
const SEED_MULTI: &[&str] = &[
    "MENTIONS",
    "is_a",
    "ASSOCIATED_WITH",
    "includes",
    "contains",
    "has_task",
    "uses",
    "requires",
    "has_topic",
    "authored",
    "commented_on",
    "related_to",
    "likes",
    "knows",
];

fn seed_predicate_meta(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO cog_predicate_meta (predicate, cardinality, source, updated_at)
         VALUES (?1, ?2, 'seed', 0)",
    )?;
    for p in SEED_SINGLE {
        stmt.execute(rusqlite::params![p, "single"])?;
    }
    for p in SEED_MULTI {
        stmt.execute(rusqlite::params![p, "multi"])?;
    }
    Ok(())
}

pub fn apply_cognitive_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        -- ============================================================
        -- Nodes (DataPoint in cognee; EntityNode in shodh-memory)
        -- ============================================================
        CREATE TABLE IF NOT EXISTS cog_nodes (
            id              BLOB    PRIMARY KEY,           -- UUIDv4 bytes (16)
            kind            TEXT    NOT NULL,              -- entity | chunk | summary | custom
            type_name       TEXT    NOT NULL DEFAULT '',   -- user-defined DataPoint type
            name            TEXT    NOT NULL DEFAULT '',
            summary         TEXT    NOT NULL DEFAULT '',
            content_hash    TEXT,                          -- dedupe (shodh content-hash)
            props_json      TEXT    NOT NULL DEFAULT '{}',
            -- embedding: little-endian f32 blob; used as fallback when
            -- sqlite-vec is unavailable. Authoritative copy lives here.
            embedding       BLOB,
            embedding_model TEXT,
            -- shodh dynamics
            salience        REAL    NOT NULL DEFAULT 0.5,
            mention_count   INTEGER NOT NULL DEFAULT 1,
            is_proper_noun  INTEGER NOT NULL DEFAULT 0,
            selectivity     REAL,
            -- Triplet-extraction state machine (chunk nodes only — others
            -- are derived from extraction so they're implicitly "done"):
            --   0 = pending           — needs LLM extraction
            --   1 = done              — extraction completed; do not re-run
            --   2 = skipped_no_llm    — LLM was dormant; retry when one is up
            --   3 = skipped_no_facts  — LLM ran but returned 0 useful triplets
            -- Encoded as INTEGER (not TEXT) for cheap WHERE filtering.
            extraction_state INTEGER NOT NULL DEFAULT 0,
            extracted_at     INTEGER,
            -- tier / lifecycle
            created_at      INTEGER NOT NULL,
            updated_at      INTEGER NOT NULL,
            last_seen_at    INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_cog_nodes_kind
            ON cog_nodes(kind, last_seen_at DESC);
        CREATE INDEX IF NOT EXISTS idx_cog_nodes_content_hash
            ON cog_nodes(content_hash) WHERE content_hash IS NOT NULL;
        CREATE INDEX IF NOT EXISTS idx_cog_nodes_name
            ON cog_nodes(name) WHERE name <> '';

        -- ============================================================
        -- Edges — Hebbian, tier-aware, LTP-protected
        -- ============================================================
        --   tier:        0=L1Working, 1=L2Episodic, 2=L3Semantic
        --   ltp_status:  0=None, 1=Burst, 2=Weekly, 3=Full
        --   activation_timestamps: JSON array (ring buffer, max 32) of i64
        CREATE TABLE IF NOT EXISTS cog_edges (
            src                   BLOB    NOT NULL,
            dst                   BLOB    NOT NULL,
            predicate             TEXT    NOT NULL,
            props_json            TEXT    NOT NULL DEFAULT '{}',
            -- ── World time: when the FACT is true ────────────────────────
            -- valid_from: asserted from. valid_to: superseded at (NULL =
            -- still the current fact). Written only by contradiction
            -- resolution (see `predicate_meta` + `cognify::upsert_triplet`),
            -- never by decay — a fact nobody mentions lately is dormant,
            -- not false. Keep these two columns free of any other meaning:
            -- that conflation is exactly the bug the archived_at split
            -- below exists to undo.
            valid_from            INTEGER NOT NULL,
            valid_to              INTEGER,
            invalidated_by        BLOB,    -- chunk/episode that superseded it
            -- ── System time: how the store treats the row ────────────────
            -- archived_at: decay consolidated it to dormant (frozen, still
            -- retrievable at floor weight). Revived by `strengthen`.
            archived_at           INTEGER,
            -- shodh Hebbian / LTP dynamics
            strength              REAL    NOT NULL DEFAULT 0.1,
            tier                  INTEGER NOT NULL DEFAULT 0,
            activation_count      INTEGER NOT NULL DEFAULT 0,
            last_activated        INTEGER NOT NULL,
            ltp_status            INTEGER NOT NULL DEFAULT 0,
            ltp_detected_at       INTEGER,
            entity_confidence     REAL,
            endpoint_selectivity  REAL,
            forman_curvature      REAL,
            activation_timestamps TEXT NOT NULL DEFAULT '[]',
            -- provenance
            source_episode_id     BLOB,
            context               TEXT NOT NULL DEFAULT '',
            created_at            INTEGER NOT NULL,
            PRIMARY KEY (src, dst, predicate),
            FOREIGN KEY (src) REFERENCES cog_nodes(id) ON DELETE CASCADE,
            FOREIGN KEY (dst) REFERENCES cog_nodes(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_cog_edges_src
            ON cog_edges(src, last_activated DESC);
        CREATE INDEX IF NOT EXISTS idx_cog_edges_dst
            ON cog_edges(dst, last_activated DESC);
        CREATE INDEX IF NOT EXISTS idx_cog_edges_tier_strength
            ON cog_edges(tier, strength DESC);
        CREATE INDEX IF NOT EXISTS idx_cog_edges_last_activated
            ON cog_edges(last_activated);
        -- NOTE: the temporal indexes are created *after* migration, not here
        -- — on a pre-existing DB `archived_at` does not exist until
        -- `migrate_edge_temporal` adds it, and a failed statement would abort
        -- this whole batch.

        -- ============================================================
        -- Predicate metadata — how many objects a predicate may hold
        -- ============================================================
        -- `single`: one object per subject at a time, so a new object
        --           supersedes the old one (`sell_price`, `has_status`).
        -- `multi` : legitimately many (`includes`, `has_task`, `is_a`).
        -- Unknown predicates default to `multi` at the call site: keeping a
        -- stale fact is recoverable, silently killing a valid one is not.
        CREATE TABLE IF NOT EXISTS cog_predicate_meta (
            predicate   TEXT PRIMARY KEY,
            cardinality TEXT    NOT NULL,
            source      TEXT    NOT NULL DEFAULT 'seed',   -- seed | user | llm
            updated_at  INTEGER NOT NULL DEFAULT 0
        );

        -- ============================================================
        -- NodeSets — scope tagging (group / persona / cowork)
        -- ============================================================
        --   scope_kind:
        --     'group'    → scope_id = group jid
        --     'persona'  → scope_id = persona slug
        --     'cowork'   → scope_id = workspace id
        --     'global'   → scope_id = ''
        --     'custom'   → free-form tag (advanced)
        CREATE TABLE IF NOT EXISTS cog_node_sets (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            scope_kind  TEXT    NOT NULL,
            scope_id    TEXT    NOT NULL DEFAULT '',
            tag         TEXT    NOT NULL,
            created_at  INTEGER NOT NULL,
            UNIQUE (scope_kind, scope_id, tag)
        );

        CREATE TABLE IF NOT EXISTS cog_node_tags (
            node_id     BLOB    NOT NULL,
            node_set_id INTEGER NOT NULL,
            PRIMARY KEY (node_id, node_set_id),
            FOREIGN KEY (node_id)     REFERENCES cog_nodes(id)     ON DELETE CASCADE,
            FOREIGN KEY (node_set_id) REFERENCES cog_node_sets(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_cog_node_tags_set
            ON cog_node_tags(node_set_id);

        -- ============================================================
        -- Decay log — bookkeeping for periodic decay_tick runs
        -- ============================================================
        CREATE TABLE IF NOT EXISTS cog_decay_log (
            run_at         INTEGER PRIMARY KEY,
            edges_scanned  INTEGER NOT NULL DEFAULT 0,
            edges_pruned   INTEGER NOT NULL DEFAULT 0,
            edges_promoted INTEGER NOT NULL DEFAULT 0,
            duration_ms    INTEGER NOT NULL DEFAULT 0
        );

        -- ============================================================
        -- Full-text index over node name + summary (FTS5 / BM25)
        -- ============================================================
        -- Zero-embedding retrieval path (`SearchType::Fts` / `Hybrid`),
        -- mirroring `memory_chunks_fts`. Standalone (non-external-content)
        -- so DELETE-by-`node_id` is cheap; `node_id` stores `hex(id)` of the
        -- node UUID for join-back (no `unhex()` dependency — callers decode
        -- the hex in Rust). The AFTER triggers keep the index in lockstep
        -- with every write path (cognify, merge, re-extract, forget) so no
        -- ingest code needs to know FTS exists.
        CREATE VIRTUAL TABLE IF NOT EXISTS cog_nodes_fts USING fts5(
            node_id UNINDEXED,
            text
        );

        CREATE TRIGGER IF NOT EXISTS cog_nodes_fts_ai
        AFTER INSERT ON cog_nodes BEGIN
            INSERT INTO cog_nodes_fts(node_id, text)
            VALUES (hex(new.id), TRIM(new.name || ' ' || new.summary));
        END;

        CREATE TRIGGER IF NOT EXISTS cog_nodes_fts_ad
        AFTER DELETE ON cog_nodes BEGIN
            DELETE FROM cog_nodes_fts WHERE node_id = hex(old.id);
        END;

        -- Fires on upsert-as-update too (ON CONFLICT DO UPDATE SET summary=…),
        -- but only when name/summary are assigned — salience/last_seen churn
        -- does not re-index.
        CREATE TRIGGER IF NOT EXISTS cog_nodes_fts_au
        AFTER UPDATE OF name, summary ON cog_nodes BEGIN
            DELETE FROM cog_nodes_fts WHERE node_id = hex(old.id);
            INSERT INTO cog_nodes_fts(node_id, text)
            VALUES (hex(new.id), TRIM(new.name || ' ' || new.summary));
        END;
        "#,
    )?;
    // Migrations for older cog_nodes that pre-date extraction_state cols.
    migrate_extraction_state(conn)?;
    // Split the decay archive marker out of `valid_to`, then index the two
    // temporal questions. Order matters: the columns must exist first.
    migrate_edge_temporal(conn)?;
    conn.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS idx_cog_edges_current
            ON cog_edges(src, predicate) WHERE valid_to IS NULL;
        CREATE INDEX IF NOT EXISTS idx_cog_edges_active
            ON cog_edges(last_activated) WHERE archived_at IS NULL;
        "#,
    )?;
    seed_predicate_meta(conn)?;
    // Backfill the FTS index for DBs created before `cog_nodes_fts` existed.
    // Only when the index is empty but nodes are present (fresh table on an
    // upgrade) — on a clean DB both counts are 0 and this is a no-op.
    backfill_nodes_fts(conn)?;
    Ok(())
}

/// One-time backfill of `cog_nodes_fts` from existing `cog_nodes` rows. Safe
/// to call on every boot: it indexes nothing once the FTS table is populated.
fn backfill_nodes_fts(conn: &Connection) -> Result<()> {
    let fts_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM cog_nodes_fts", [], |r| r.get(0))
        .unwrap_or(0);
    if fts_count > 0 {
        return Ok(());
    }
    let node_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM cog_nodes", [], |r| r.get(0))
        .unwrap_or(0);
    if node_count == 0 {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO cog_nodes_fts(node_id, text)
         SELECT hex(id), TRIM(name || ' ' || summary) FROM cog_nodes",
        [],
    )?;
    Ok(())
}

/// Create the `cog_vec` virtual table once embedder dimensions are known.
/// Called separately from [`apply_cognitive_schema`] because dimensions
/// depend on the configured embedding provider (see [`crate::memory::schema`]).
///
/// Returns `Ok(false)` and logs if `sqlite-vec` is not loaded (mirrors
/// `apply_memory_schema` behaviour).
pub fn apply_cognitive_vec_schema(conn: &Connection, dimensions: u32) -> Result<bool> {
    let sql = format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS cog_vec USING vec0(
            node_id BLOB PRIMARY KEY,
            embedding float[{dimensions}]
         );"
    );
    match conn.execute_batch(&sql) {
        Ok(()) => Ok(true),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "[cognitive] sqlite-vec not available; cog_vec table skipped"
            );
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn schema_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        apply_cognitive_schema(&conn).unwrap();
        // applying twice must not error
        apply_cognitive_schema(&conn).unwrap();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'cog_%'")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(tables.iter().any(|t| t == "cog_nodes"));
        assert!(tables.iter().any(|t| t == "cog_edges"));
        assert!(tables.iter().any(|t| t == "cog_node_sets"));
        assert!(tables.iter().any(|t| t == "cog_node_tags"));
        assert!(tables.iter().any(|t| t == "cog_decay_log"));
        assert!(tables.iter().any(|t| t == "cog_nodes_fts"));
    }

    /// The pre-split schema: `valid_to` is where decay wrote its archive
    /// marker, and neither new column exists. Reproduced literally so the
    /// migration is tested against the shape it will actually meet — on the
    /// author's own database that was 20,282 of 21,112 edges.
    fn legacy_edges_table(conn: &Connection) {
        conn.execute_batch(
            r#"
            CREATE TABLE cog_edges (
                src BLOB NOT NULL, dst BLOB NOT NULL, predicate TEXT NOT NULL,
                props_json TEXT NOT NULL DEFAULT '{}',
                valid_from INTEGER NOT NULL, valid_to INTEGER,
                strength REAL NOT NULL DEFAULT 0.1, tier INTEGER NOT NULL DEFAULT 0,
                activation_count INTEGER NOT NULL DEFAULT 0,
                last_activated INTEGER NOT NULL,
                ltp_status INTEGER NOT NULL DEFAULT 0, ltp_detected_at INTEGER,
                entity_confidence REAL, endpoint_selectivity REAL, forman_curvature REAL,
                activation_timestamps TEXT NOT NULL DEFAULT '[]',
                source_episode_id BLOB, context TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL,
                PRIMARY KEY (src, dst, predicate)
            );
            CREATE TABLE cog_nodes (
                id BLOB PRIMARY KEY, kind TEXT NOT NULL, type_name TEXT NOT NULL DEFAULT '',
                name TEXT NOT NULL DEFAULT '', summary TEXT NOT NULL DEFAULT '',
                content_hash TEXT, props_json TEXT NOT NULL DEFAULT '{}',
                embedding BLOB, embedding_model TEXT,
                salience REAL NOT NULL DEFAULT 0.5, mention_count INTEGER NOT NULL DEFAULT 1,
                is_proper_noun INTEGER NOT NULL DEFAULT 0, selectivity REAL,
                created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL
            );
            INSERT INTO cog_edges (src, dst, predicate, valid_from, valid_to, last_activated, created_at)
            VALUES
              (X'01', X'02', 'dormant',  100, 900, 100, 100),   -- decay-archived
              (X'01', X'03', 'live',     100, NULL, 100, 100);  -- still active
            "#,
        )
        .unwrap();
    }

    #[test]
    fn migration_moves_archive_markers_out_of_valid_to() {
        let conn = Connection::open_in_memory().unwrap();
        legacy_edges_table(&conn);
        apply_cognitive_schema(&conn).unwrap();

        let (archived, valid_to): (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT archived_at, valid_to FROM cog_edges WHERE predicate = 'dormant'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(archived, Some(900), "the marker keeps its timestamp");
        assert_eq!(
            valid_to, None,
            "world time must be handed back empty — a dormant fact is not a false one"
        );

        // The untouched row stays untouched.
        let (a2, v2): (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT archived_at, valid_to FROM cog_edges WHERE predicate = 'live'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((a2, v2), (None, None));

        // Idempotent: a second boot must not re-run the move and wipe
        // `archived_at` back into `valid_to`.
        apply_cognitive_schema(&conn).unwrap();
        let again: Option<i64> = conn
            .query_row(
                "SELECT archived_at FROM cog_edges WHERE predicate = 'dormant'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(again, Some(900));
    }

    // A pre-split daemon left running (or restarted from an old bundle) keeps
    // writing archive markers into `valid_to` after the migration. Those rows
    // must be repaired on the next boot, not read as superseded facts —
    // otherwise dormant knowledge disappears from every present-tense recall.
    #[test]
    fn later_writes_from_an_old_daemon_are_repaired_too() {
        let conn = Connection::open_in_memory().unwrap();
        legacy_edges_table(&conn);
        apply_cognitive_schema(&conn).unwrap();

        // Old binary archives another edge the only way it knows how.
        conn.execute(
            "UPDATE cog_edges SET valid_to = 1234 WHERE predicate = 'live'",
            [],
        )
        .unwrap();
        // Meanwhile the new code supersedes a fact properly — with a culprit.
        conn.execute(
            "INSERT INTO cog_edges (src, dst, predicate, valid_from, valid_to, invalidated_by,
                                    last_activated, created_at)
             VALUES (X'01', X'04', 'superseded', 100, 500, X'AA', 100, 100)",
            [],
        )
        .unwrap();

        apply_cognitive_schema(&conn).unwrap();

        let (a, v): (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT archived_at, valid_to FROM cog_edges WHERE predicate = 'live'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (a, v),
            (Some(1234), None),
            "an old daemon's marker must be moved, not read as a superseded fact"
        );

        let still: Option<i64> = conn
            .query_row(
                "SELECT valid_to FROM cog_edges WHERE predicate = 'superseded'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            still,
            Some(500),
            "a genuine supersession carries invalidated_by and must survive untouched"
        );
    }

    #[test]
    fn predicate_meta_is_seeded_once_and_respects_edits() {
        let conn = Connection::open_in_memory().unwrap();
        apply_cognitive_schema(&conn).unwrap();
        let card = |p: &str| -> Option<String> {
            conn.query_row(
                "SELECT cardinality FROM cog_predicate_meta WHERE predicate = ?1",
                rusqlite::params![p],
                |r| r.get(0),
            )
            .ok()
        };
        assert_eq!(card("sell_price").as_deref(), Some("single"));
        assert_eq!(card("has_task").as_deref(), Some("multi"));

        // A user correction survives the next boot's re-seed.
        conn.execute(
            "UPDATE cog_predicate_meta SET cardinality='multi', source='user' WHERE predicate='name'",
            [],
        )
        .unwrap();
        apply_cognitive_schema(&conn).unwrap();
        assert_eq!(card("name").as_deref(), Some("multi"));
    }

    /// Run the migration against a real database and report what moved.
    /// Ignored by default — it needs a path and mutates the file it is given,
    /// so point it at a COPY:
    ///
    /// ```text
    /// cp ~/.senclaw/senclaw_cognitive.db /tmp/cog.db
    /// SENCLAW_TEST_COG_DB=/tmp/cog.db cargo test --lib migration_on_a_real_database -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs SENCLAW_TEST_COG_DB pointing at a copy of a real graph"]
    fn migration_on_a_real_database() {
        let Ok(path) = std::env::var("SENCLAW_TEST_COG_DB") else {
            return;
        };
        let conn = Connection::open(&path).unwrap();
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        let before_valid_to = count("SELECT COUNT(*) FROM cog_edges WHERE valid_to IS NOT NULL");
        let total = count("SELECT COUNT(*) FROM cog_edges");

        apply_cognitive_schema(&conn).unwrap();

        let after_valid_to = count("SELECT COUNT(*) FROM cog_edges WHERE valid_to IS NOT NULL");
        let archived = count("SELECT COUNT(*) FROM cog_edges WHERE archived_at IS NOT NULL");
        println!(
            "edges={total} valid_to before={before_valid_to} after={after_valid_to} archived_at={archived}"
        );
        assert_eq!(
            after_valid_to, 0,
            "every marker must leave valid_to — none of them is a superseded fact"
        );
        assert!(
            archived >= before_valid_to,
            "markers must arrive in archived_at, not evaporate"
        );
        // Applying twice must not move anything a second time.
        apply_cognitive_schema(&conn).unwrap();
        assert_eq!(
            count("SELECT COUNT(*) FROM cog_edges WHERE archived_at IS NOT NULL"),
            archived
        );
        assert_eq!(count("SELECT COUNT(*) FROM cog_edges"), total, "no row lost");
    }

    #[test]
    fn nodes_fts_trigger_syncs_on_insert_and_delete() {
        let conn = Connection::open_in_memory().unwrap();
        apply_cognitive_schema(&conn).unwrap();

        let now = 100i64;
        let id = uuid::Uuid::new_v4();
        let id_blob = id.as_bytes().to_vec();
        conn.execute(
            "INSERT INTO cog_nodes
                (id, kind, name, summary, created_at, updated_at, last_seen_at)
             VALUES (?1, 'entity', 'Ada Lovelace', 'first programmer', ?2, ?2, ?2)",
            rusqlite::params![id_blob, now],
        )
        .unwrap();

        // AFTER INSERT trigger indexed name + summary.
        let found: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM cog_nodes_fts WHERE text MATCH 'lovelace'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(found, 1, "insert trigger should index the node");

        // node_id round-trips as uppercase hex of the UUID bytes.
        let nid: String = conn
            .query_row(
                "SELECT node_id FROM cog_nodes_fts WHERE text MATCH 'lovelace'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(nid, hex::encode_upper(id.as_bytes()));

        // AFTER DELETE trigger removed it from the index.
        conn.execute(
            "DELETE FROM cog_nodes WHERE id = ?1",
            rusqlite::params![id.as_bytes().to_vec()],
        )
        .unwrap();
        let after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM cog_nodes_fts WHERE text MATCH 'lovelace'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after, 0, "delete trigger should remove the node");
    }
}
