use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// SQLite store for the Kanban app. Modeled on Hermes Agent's board:
/// `boards` → `columns` (workflow stages, each with a `role`) → `cards` (tasks,
/// with assignee/priority/tenant). `card_links` records parent→child dependencies
/// and `card_comments` the durable per-card note thread. A per-board chat history
/// backs the AI assistant panel.
pub struct Db {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS boards (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  title         TEXT NOT NULL,
  description   TEXT NOT NULL DEFAULT '',
  workspace_dir TEXT,
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS columns (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  board_id   INTEGER NOT NULL,
  title      TEXT NOT NULL,
  role       TEXT NOT NULL DEFAULT 'custom',
  color      TEXT,
  wip_limit  INTEGER,
  ord        INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_columns_board ON columns(board_id);
CREATE TABLE IF NOT EXISTS cards (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  board_id    INTEGER NOT NULL,
  column_id   INTEGER NOT NULL,
  title       TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  priority    TEXT,
  assignee    TEXT,
  tenant      TEXT,
  labels      TEXT,
  due_date    INTEGER,
  done        INTEGER NOT NULL DEFAULT 0,
  ord         INTEGER NOT NULL DEFAULT 0,
  claimed_by  TEXT,
  lease_until INTEGER,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_cards_board  ON cards(board_id);
CREATE INDEX IF NOT EXISTS idx_cards_column ON cards(column_id);

CREATE TABLE IF NOT EXISTS card_links (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  board_id   INTEGER NOT NULL,
  parent_id  INTEGER NOT NULL,
  child_id   INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_links_parent ON card_links(parent_id);
CREATE INDEX IF NOT EXISTS idx_links_child  ON card_links(child_id);

CREATE TABLE IF NOT EXISTS card_comments (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  card_id    INTEGER NOT NULL,
  author     TEXT NOT NULL DEFAULT '',
  body       TEXT NOT NULL,
  kind       TEXT NOT NULL DEFAULT 'comment',
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_comments_card ON card_comments(card_id);

CREATE TABLE IF NOT EXISTS column_templates (
  id           TEXT PRIMARY KEY,
  name         TEXT NOT NULL,
  description  TEXT NOT NULL DEFAULT '',
  columns_json TEXT NOT NULL,
  created_at   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS chat_sessions (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  board_id   INTEGER NOT NULL,
  title      TEXT NOT NULL DEFAULT 'Hội thoại',
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS chat_messages (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id INTEGER NOT NULL,
  role       TEXT NOT NULL,
  content    TEXT NOT NULL,
  model      TEXT,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sessions_board ON chat_sessions(board_id);
CREATE INDEX IF NOT EXISTS idx_messages_sess  ON chat_messages(session_id);
"#;

/// Columns added after v1 — applied to pre-existing DBs (errors on already-present
/// columns are ignored).
const MIGRATIONS: &[&str] = &[
    "ALTER TABLE columns ADD COLUMN role TEXT NOT NULL DEFAULT 'custom'",
    "ALTER TABLE cards   ADD COLUMN tenant TEXT",
    // Dispatch claim/lease — set when a worker is dispatched to a card.
    "ALTER TABLE cards   ADD COLUMN claimed_by TEXT",
    "ALTER TABLE cards   ADD COLUMN lease_until INTEGER",
    // Per-board working directory — dispatched workers run here (outputs land
    // in this folder) instead of a throwaway scratch dir.
    "ALTER TABLE boards  ADD COLUMN workspace_dir TEXT",
];

/// The Hermes-style default workflow: (title, role, color).
pub const DEFAULT_WORKFLOW: &[(&str, &str, &str)] = &[
    ("Triage", "triage", "#a855f7"),
    ("Todo", "todo", "#64748b"),
    ("Ready", "ready", "#0ea5e9"),
    ("In Progress", "in_progress", "#3b82f6"),
    ("Blocked", "blocked", "#ef4444"),
    ("Done", "done", "#22c55e"),
];

/// A board's metadata (list view).
#[derive(Serialize)]
pub struct BoardMeta {
    pub id: i64,
    pub title: String,
    pub description: String,
    /// Where dispatched workers run for this board (outputs land here).
    /// None = throwaway scratch dir per task.
    pub workspace_dir: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub column_count: i64,
    pub card_count: i64,
}

/// A column (workflow stage) as stored.
#[derive(Serialize, Clone)]
pub struct Column {
    pub id: i64,
    pub board_id: i64,
    pub title: String,
    /// Workflow role: `triage|todo|ready|in_progress|blocked|done|custom`.
    pub role: String,
    pub color: Option<String>,
    pub wip_limit: Option<i64>,
    pub ord: i64,
}

/// A card (task) as stored, plus computed counts used by the board view.
#[derive(Serialize, Clone)]
pub struct Card {
    pub id: i64,
    pub board_id: i64,
    pub column_id: i64,
    pub title: String,
    pub description: String,
    /// `low` | `medium` | `high` | `urgent`.
    pub priority: Option<String>,
    /// The assignee / worker profile a card is routed to (drives worker lanes).
    pub assignee: Option<String>,
    /// Optional tenant namespace for multi-tenant isolation.
    pub tenant: Option<String>,
    /// JSON array of label strings, stored as text.
    pub labels: Option<String>,
    pub due_date: Option<i64>,
    pub done: bool,
    pub ord: i64,
    pub created_at: i64,
    pub updated_at: i64,
    // ---- computed (not stored) ----
    #[serde(default)]
    pub comment_count: i64,
    /// Number of dependency parents that are not yet done (this card is blocked
    /// while > 0).
    #[serde(default)]
    pub open_deps: i64,
    /// Child-task progress (for cards that have dependents): total & done.
    #[serde(default)]
    pub child_total: i64,
    #[serde(default)]
    pub child_done: i64,
}

/// A column with its cards nested (board view + MCP get).
#[derive(Serialize)]
pub struct ColumnWithCards {
    #[serde(flatten)]
    pub column: Column,
    pub cards: Vec<Card>,
}

/// One dependency edge (parent must finish before child).
#[derive(Serialize)]
pub struct Link {
    pub id: i64,
    pub parent_id: i64,
    pub child_id: i64,
    pub parent_title: String,
    pub child_title: String,
    pub parent_done: bool,
    pub child_done: bool,
}

/// One durable card comment / note.
#[derive(Serialize)]
pub struct Comment {
    pub id: i64,
    pub author: String,
    pub body: String,
    /// `comment` | `complete` | `block` | `unblock` | `system`.
    pub kind: String,
    pub created_at: i64,
}

/// A chat session bound to a board.
#[derive(Serialize)]
pub struct ChatSession {
    pub id: i64,
    pub board_id: i64,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub message_count: i64,
}

/// One stored chat message.
#[derive(Serialize)]
pub struct ChatMessageRow {
    pub id: i64,
    pub role: String,
    pub content: String,
    pub model: Option<String>,
    pub created_at: i64,
}

/// A generated column from the LLM or a template (with its cards).
#[derive(Deserialize, Clone)]
pub struct GenColumn {
    pub title: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub wip_limit: Option<i64>,
    #[serde(default)]
    pub cards: Vec<GenCard>,
}

/// A generated card from the LLM or a template.
#[derive(Deserialize, Clone)]
pub struct GenCard {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        for m in MIGRATIONS {
            let _ = conn.execute(m, []); // ignore "duplicate column" on existing DBs
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().unwrap();
        f(&conn)
    }

    /// Crate-visible connection access for sibling modules (templates.rs).
    pub(crate) fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.with(f)
    }

    fn touch(c: &Connection, board_id: i64, now: i64) -> Result<()> {
        c.execute(
            "UPDATE boards SET updated_at=?2 WHERE id=?1",
            params![board_id, now],
        )?;
        Ok(())
    }

    // ---- boards ----

    pub fn list_boards(&self) -> Result<Vec<BoardMeta>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT b.id, b.title, b.description, b.workspace_dir, b.created_at, b.updated_at,
                        (SELECT COUNT(*) FROM columns k WHERE k.board_id = b.id),
                        (SELECT COUNT(*) FROM cards d WHERE d.board_id = b.id)
                 FROM boards b ORDER BY b.updated_at DESC",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(BoardMeta {
                        id: r.get(0)?,
                        title: r.get(1)?,
                        description: r.get(2)?,
                        workspace_dir: r.get(3)?,
                        created_at: r.get(4)?,
                        updated_at: r.get(5)?,
                        column_count: r.get(6)?,
                        card_count: r.get(7)?,
                    })
                })?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        })
    }

    /// Create a board. If `with_defaults`, seed the Hermes workflow columns
    /// (Triage → Todo → Ready → In Progress → Blocked → Done). `workspace_dir`
    /// is where dispatched workers run (task outputs land there). Returns the id.
    pub fn create_board(
        &self,
        title: &str,
        description: &str,
        with_defaults: bool,
        workspace_dir: Option<&str>,
        now: i64,
    ) -> Result<i64> {
        self.with(|c| {
            c.execute(
                "INSERT INTO boards(title, description, workspace_dir, created_at, updated_at) VALUES(?1,?2,?3,?4,?4)",
                params![title, description, workspace_dir, now],
            )?;
            let board_id = c.last_insert_rowid();
            if with_defaults {
                for (i, (name, role, color)) in DEFAULT_WORKFLOW.iter().enumerate() {
                    c.execute(
                        "INSERT INTO columns(board_id, title, role, color, ord, created_at) VALUES(?1,?2,?3,?4,?5,?6)",
                        params![board_id, name, role, color, i as i64, now],
                    )?;
                }
            }
            Ok(board_id)
        })
    }

    pub fn rename_board(
        &self,
        board_id: i64,
        title: &str,
        description: &str,
        now: i64,
    ) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE boards SET title=?2, description=?3, updated_at=?4 WHERE id=?1",
                params![board_id, title, description, now],
            )?;
            Ok(())
        })
    }

    pub fn delete_board(&self, board_id: i64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "DELETE FROM card_comments WHERE card_id IN (SELECT id FROM cards WHERE board_id=?1)",
                params![board_id],
            )?;
            c.execute("DELETE FROM card_links WHERE board_id=?1", params![board_id])?;
            c.execute(
                "DELETE FROM chat_messages WHERE session_id IN (SELECT id FROM chat_sessions WHERE board_id=?1)",
                params![board_id],
            )?;
            c.execute("DELETE FROM chat_sessions WHERE board_id=?1", params![board_id])?;
            c.execute("DELETE FROM cards   WHERE board_id=?1", params![board_id])?;
            c.execute("DELETE FROM columns WHERE board_id=?1", params![board_id])?;
            c.execute("DELETE FROM boards  WHERE id=?1", params![board_id])?;
            Ok(())
        })
    }

    pub fn board_meta(&self, board_id: i64) -> Result<Option<BoardMeta>> {
        self.with(|c| {
            let row = c
                .query_row(
                    "SELECT id, title, description, workspace_dir, created_at, updated_at,
                            (SELECT COUNT(*) FROM columns k WHERE k.board_id = boards.id),
                            (SELECT COUNT(*) FROM cards d WHERE d.board_id = boards.id)
                     FROM boards WHERE id=?1",
                    params![board_id],
                    |r| {
                        Ok(BoardMeta {
                            id: r.get(0)?,
                            title: r.get(1)?,
                            description: r.get(2)?,
                            workspace_dir: r.get(3)?,
                            created_at: r.get(4)?,
                            updated_at: r.get(5)?,
                            column_count: r.get(6)?,
                            card_count: r.get(7)?,
                        })
                    },
                )
                .optional()?;
            Ok(row)
        })
    }

    // ---- columns ----

    fn columns_of_conn(c: &Connection, board_id: i64) -> Result<Vec<Column>> {
        let mut stmt = c.prepare(
            "SELECT id, board_id, title, role, color, wip_limit, ord
             FROM columns WHERE board_id=?1 ORDER BY ord, id",
        )?;
        let rows = stmt
            .query_map(params![board_id], |r| {
                Ok(Column {
                    id: r.get(0)?,
                    board_id: r.get(1)?,
                    title: r.get(2)?,
                    role: r.get(3)?,
                    color: r.get(4)?,
                    wip_limit: r.get(5)?,
                    ord: r.get(6)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    fn cards_of_conn(c: &Connection, board_id: i64) -> Result<Vec<Card>> {
        let mut stmt = c.prepare(
            "SELECT id, board_id, column_id, title, description, priority, assignee, tenant, labels,
                    due_date, done, ord, created_at, updated_at
             FROM cards WHERE board_id=?1 ORDER BY ord, id",
        )?;
        let rows = stmt
            .query_map(params![board_id], |r| {
                Ok(Card {
                    id: r.get(0)?,
                    board_id: r.get(1)?,
                    column_id: r.get(2)?,
                    title: r.get(3)?,
                    description: r.get(4)?,
                    priority: r.get(5)?,
                    assignee: r.get(6)?,
                    tenant: r.get(7)?,
                    labels: r.get(8)?,
                    due_date: r.get(9)?,
                    done: r.get::<_, i64>(10)? != 0,
                    ord: r.get(11)?,
                    created_at: r.get(12)?,
                    updated_at: r.get(13)?,
                    comment_count: 0,
                    open_deps: 0,
                    child_total: 0,
                    child_done: 0,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// The full board: its columns, each with its cards nested and ordered. Fills
    /// each card's computed counts (comments, open dependencies, child progress).
    pub fn board_full(&self, board_id: i64) -> Result<Vec<ColumnWithCards>> {
        self.with(|c| {
            let columns = Self::columns_of_conn(c, board_id)?;
            let mut cards = Self::cards_of_conn(c, board_id)?;
            let done: std::collections::HashMap<i64, bool> =
                cards.iter().map(|d| (d.id, d.done)).collect();
            let links = Self::links_of_board(c, board_id)?;
            for card in &mut cards {
                card.comment_count = c
                    .query_row(
                        "SELECT COUNT(*) FROM card_comments WHERE card_id=?1",
                        params![card.id],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                // Parents of this card (this card is the child) → open deps.
                card.open_deps = links
                    .iter()
                    .filter(|l| l.1 == card.id)
                    .filter(|l| !done.get(&l.0).copied().unwrap_or(false))
                    .count() as i64;
                // Children of this card (this card is the parent) → progress.
                let kids: Vec<i64> = links
                    .iter()
                    .filter(|l| l.0 == card.id)
                    .map(|l| l.1)
                    .collect();
                card.child_total = kids.len() as i64;
                card.child_done = kids
                    .iter()
                    .filter(|id| done.get(id).copied().unwrap_or(false))
                    .count() as i64;
            }
            Ok(columns
                .into_iter()
                .map(|col| {
                    let mut cs: Vec<Card> = cards
                        .iter()
                        .filter(|d| d.column_id == col.id)
                        .cloned()
                        .collect();
                    cs.sort_by_key(|d| (d.ord, d.id));
                    ColumnWithCards {
                        column: col,
                        cards: cs,
                    }
                })
                .collect())
        })
    }

    /// Raw (parent_id, child_id) edges for a board.
    fn links_of_board(c: &Connection, board_id: i64) -> Result<Vec<(i64, i64)>> {
        let mut stmt = c.prepare("SELECT parent_id, child_id FROM card_links WHERE board_id=?1")?;
        let rows = stmt
            .query_map(params![board_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    fn board_of_column(c: &Connection, column_id: i64) -> Result<i64> {
        c.query_row(
            "SELECT board_id FROM columns WHERE id=?1",
            params![column_id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| anyhow!("column {column_id} not found"))
    }

    fn board_of_card(c: &Connection, card_id: i64) -> Result<i64> {
        c.query_row(
            "SELECT board_id FROM cards WHERE id=?1",
            params![card_id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| anyhow!("card {card_id} not found"))
    }

    fn next_column_ord(c: &Connection, board_id: i64) -> Result<i64> {
        Ok(c.query_row(
            "SELECT COALESCE(MAX(ord), -1) + 1 FROM columns WHERE board_id=?1",
            params![board_id],
            |r| r.get(0),
        )
        .unwrap_or(0))
    }

    fn next_card_ord(c: &Connection, column_id: i64) -> Result<i64> {
        Ok(c.query_row(
            "SELECT COALESCE(MAX(ord), -1) + 1 FROM cards WHERE column_id=?1",
            params![column_id],
            |r| r.get(0),
        )
        .unwrap_or(0))
    }

    /// The first column on a board whose role matches (e.g. `done`, `blocked`).
    pub fn column_by_role(&self, board_id: i64, role: &str) -> Result<Option<i64>> {
        self.with(|c| {
            let id = c
                .query_row(
                    "SELECT id FROM columns WHERE board_id=?1 AND role=?2 ORDER BY ord LIMIT 1",
                    params![board_id, role],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(id)
        })
    }

    /// Add a column to a board. Returns the new column id.
    pub fn add_column(
        &self,
        board_id: i64,
        title: &str,
        role: &str,
        color: Option<&str>,
        wip_limit: Option<i64>,
        now: i64,
    ) -> Result<i64> {
        self.with(|c| {
            let ord = Self::next_column_ord(c, board_id)?;
            c.execute(
                "INSERT INTO columns(board_id, title, role, color, wip_limit, ord, created_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![board_id, title, role, color, wip_limit, ord, now],
            )?;
            let id = c.last_insert_rowid();
            Self::touch(c, board_id, now)?;
            Ok(id)
        })
    }

    /// Update a column's fields (None = leave unchanged; inner None clears the column).
    pub fn update_column(
        &self,
        column_id: i64,
        title: Option<&str>,
        color: Option<Option<&str>>,
        wip_limit: Option<Option<i64>>,
        now: i64,
    ) -> Result<()> {
        self.with(|c| {
            let board_id = Self::board_of_column(c, column_id)?;
            if let Some(t) = title {
                c.execute(
                    "UPDATE columns SET title=?2 WHERE id=?1",
                    params![column_id, t],
                )?;
            }
            if let Some(col) = color {
                c.execute(
                    "UPDATE columns SET color=?2 WHERE id=?1",
                    params![column_id, col],
                )?;
            }
            if let Some(w) = wip_limit {
                c.execute(
                    "UPDATE columns SET wip_limit=?2 WHERE id=?1",
                    params![column_id, w],
                )?;
            }
            Self::touch(c, board_id, now)?;
            Ok(())
        })
    }

    /// Delete a column and all of its cards.
    pub fn delete_column(&self, column_id: i64, now: i64) -> Result<()> {
        self.with(|c| {
            let board_id = Self::board_of_column(c, column_id)?;
            c.execute(
                "DELETE FROM card_comments WHERE card_id IN (SELECT id FROM cards WHERE column_id=?1)",
                params![column_id],
            )?;
            c.execute(
                "DELETE FROM card_links WHERE parent_id IN (SELECT id FROM cards WHERE column_id=?1)
                 OR child_id IN (SELECT id FROM cards WHERE column_id=?1)",
                params![column_id],
            )?;
            c.execute("DELETE FROM cards WHERE column_id=?1", params![column_id])?;
            c.execute("DELETE FROM columns WHERE id=?1", params![column_id])?;
            Self::touch(c, board_id, now)?;
            Ok(())
        })
    }

    /// Reorder columns on a board to match the given id order.
    pub fn reorder_columns(&self, board_id: i64, ids: &[i64], now: i64) -> Result<()> {
        self.with(|c| {
            for (i, id) in ids.iter().enumerate() {
                c.execute(
                    "UPDATE columns SET ord=?2 WHERE id=?1 AND board_id=?3",
                    params![id, i as i64, board_id],
                )?;
            }
            Self::touch(c, board_id, now)?;
            Ok(())
        })
    }

    // ---- cards ----

    /// Add a card to a column. Returns the new card id.
    #[allow(clippy::too_many_arguments)]
    pub fn add_card(
        &self,
        column_id: i64,
        title: &str,
        description: &str,
        priority: Option<&str>,
        assignee: Option<&str>,
        tenant: Option<&str>,
        labels: Option<&str>,
        due_date: Option<i64>,
        now: i64,
    ) -> Result<i64> {
        self.with(|c| {
            let board_id = Self::board_of_column(c, column_id)?;
            let ord = Self::next_card_ord(c, column_id)?;
            c.execute(
                "INSERT INTO cards(board_id, column_id, title, description, priority, assignee, tenant, labels,
                                   due_date, done, ord, created_at, updated_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,0,?10,?11,?11)",
                params![board_id, column_id, title, description, priority, assignee, tenant, labels, due_date, ord, now],
            )?;
            let id = c.last_insert_rowid();
            Self::touch(c, board_id, now)?;
            Ok(id)
        })
    }

    /// Update any subset of a card's fields.
    #[allow(clippy::too_many_arguments)]
    pub fn update_card(
        &self,
        card_id: i64,
        title: Option<&str>,
        description: Option<&str>,
        priority: Option<Option<&str>>,
        assignee: Option<Option<&str>>,
        tenant: Option<Option<&str>>,
        labels: Option<Option<&str>>,
        due_date: Option<Option<i64>>,
        done: Option<bool>,
        now: i64,
    ) -> Result<()> {
        self.with(|c| {
            let board_id = Self::board_of_card(c, card_id)?;
            if let Some(t) = title {
                c.execute("UPDATE cards SET title=?2 WHERE id=?1", params![card_id, t])?;
            }
            if let Some(d) = description {
                c.execute(
                    "UPDATE cards SET description=?2 WHERE id=?1",
                    params![card_id, d],
                )?;
            }
            if let Some(p) = priority {
                c.execute(
                    "UPDATE cards SET priority=?2 WHERE id=?1",
                    params![card_id, p],
                )?;
            }
            if let Some(a) = assignee {
                c.execute(
                    "UPDATE cards SET assignee=?2 WHERE id=?1",
                    params![card_id, a],
                )?;
            }
            if let Some(t) = tenant {
                c.execute(
                    "UPDATE cards SET tenant=?2 WHERE id=?1",
                    params![card_id, t],
                )?;
            }
            if let Some(l) = labels {
                c.execute(
                    "UPDATE cards SET labels=?2 WHERE id=?1",
                    params![card_id, l],
                )?;
            }
            if let Some(dd) = due_date {
                c.execute(
                    "UPDATE cards SET due_date=?2 WHERE id=?1",
                    params![card_id, dd],
                )?;
            }
            if let Some(dn) = done {
                c.execute(
                    "UPDATE cards SET done=?2 WHERE id=?1",
                    params![card_id, dn as i64],
                )?;
            }
            c.execute(
                "UPDATE cards SET updated_at=?2 WHERE id=?1",
                params![card_id, now],
            )?;
            Self::touch(c, board_id, now)?;
            Ok(())
        })
    }

    /// Move a card to `new_column` at position `index` (0-based). Cards in the
    /// target column are renumbered so the moved card lands at `index`.
    pub fn move_card(&self, card_id: i64, new_column: i64, index: i64, now: i64) -> Result<()> {
        self.with(|c| {
            let board_id = Self::board_of_card(c, card_id)?;
            let col_board = Self::board_of_column(c, new_column)?;
            if board_id != col_board {
                return Err(anyhow!("cannot move a card across boards"));
            }
            let mut ids: Vec<i64> = {
                let mut stmt = c.prepare(
                    "SELECT id FROM cards WHERE column_id=?1 AND id<>?2 ORDER BY ord, id",
                )?;
                let v: Vec<i64> = stmt
                    .query_map(params![new_column, card_id], |r| r.get(0))?
                    .filter_map(|r| r.ok())
                    .collect();
                v
            };
            let idx = index.clamp(0, ids.len() as i64) as usize;
            ids.insert(idx, card_id);
            c.execute(
                "UPDATE cards SET column_id=?2, updated_at=?3 WHERE id=?1",
                params![card_id, new_column, now],
            )?;
            for (i, id) in ids.iter().enumerate() {
                c.execute("UPDATE cards SET ord=?2 WHERE id=?1", params![id, i as i64])?;
            }
            // Moving into a `done` column marks the card done; out of it un-marks.
            let role: Option<String> = c
                .query_row(
                    "SELECT role FROM columns WHERE id=?1",
                    params![new_column],
                    |r| r.get(0),
                )
                .optional()?;
            if role.as_deref() == Some("done") {
                c.execute("UPDATE cards SET done=1 WHERE id=?1", params![card_id])?;
            } else {
                c.execute("UPDATE cards SET done=0 WHERE id=?1", params![card_id])?;
            }
            Self::touch(c, board_id, now)?;
            Ok(())
        })
    }

    pub fn delete_card(&self, card_id: i64, now: i64) -> Result<()> {
        self.with(|c| {
            let board_id = Self::board_of_card(c, card_id)?;
            c.execute(
                "DELETE FROM card_comments WHERE card_id=?1",
                params![card_id],
            )?;
            c.execute(
                "DELETE FROM card_links WHERE parent_id=?1 OR child_id=?1",
                params![card_id],
            )?;
            c.execute("DELETE FROM cards WHERE id=?1", params![card_id])?;
            Self::touch(c, board_id, now)?;
            Ok(())
        })
    }

    /// (title, description, column_id, board_id) for a card.
    pub fn card_detail(&self, card_id: i64) -> Result<(String, String, i64, i64)> {
        self.with(|c| {
            c.query_row(
                "SELECT title, description, column_id, board_id FROM cards WHERE id=?1",
                params![card_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(|e| anyhow!(e))
        })
    }

    /// A single card with its full detail (for MCP `kanban_show`).
    pub fn card_row(&self, card_id: i64) -> Result<Option<Card>> {
        self.with(|c| {
            let row = c
                .query_row(
                    "SELECT id, board_id, column_id, title, description, priority, assignee, tenant, labels,
                            due_date, done, ord, created_at, updated_at
                     FROM cards WHERE id=?1",
                    params![card_id],
                    |r| {
                        Ok(Card {
                            id: r.get(0)?,
                            board_id: r.get(1)?,
                            column_id: r.get(2)?,
                            title: r.get(3)?,
                            description: r.get(4)?,
                            priority: r.get(5)?,
                            assignee: r.get(6)?,
                            tenant: r.get(7)?,
                            labels: r.get(8)?,
                            due_date: r.get(9)?,
                            done: r.get::<_, i64>(10)? != 0,
                            ord: r.get(11)?,
                            created_at: r.get(12)?,
                            updated_at: r.get(13)?,
                            comment_count: 0,
                            open_deps: 0,
                            child_total: 0,
                            child_done: 0,
                        })
                    },
                )
                .optional()?;
            Ok(row)
        })
    }

    /// List cards on a board with optional filters (column role, assignee, tenant).
    pub fn list_cards(
        &self,
        board_id: i64,
        role: Option<&str>,
        assignee: Option<&str>,
        tenant: Option<&str>,
    ) -> Result<Vec<Card>> {
        let cols = self.board_full(board_id)?;
        let mut out = Vec::new();
        for col in cols {
            if let Some(rr) = role {
                if col.column.role != rr {
                    continue;
                }
            }
            for card in col.cards {
                if let Some(a) = assignee {
                    if card.assignee.as_deref() != Some(a) {
                        continue;
                    }
                }
                if let Some(t) = tenant {
                    if card.tenant.as_deref() != Some(t) {
                        continue;
                    }
                }
                out.push(card);
            }
        }
        Ok(out)
    }

    /// The set of distinct assignees on a board (worker profiles), for lanes.
    pub fn assignees(&self, board_id: i64) -> Result<Vec<String>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT DISTINCT assignee FROM cards WHERE board_id=?1 AND assignee IS NOT NULL AND assignee<>'' ORDER BY assignee",
            )?;
            let rows = stmt
                .query_map(params![board_id], |r| r.get::<_, String>(0))?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        })
    }

    // ---- dependency links ----

    pub fn add_link(&self, parent_id: i64, child_id: i64, now: i64) -> Result<i64> {
        self.with(|c| {
            if parent_id == child_id {
                return Err(anyhow!("a card cannot depend on itself"));
            }
            let pb = Self::board_of_card(c, parent_id)?;
            let cb = Self::board_of_card(c, child_id)?;
            if pb != cb {
                return Err(anyhow!("cannot link cards across boards"));
            }
            // Reject if it would create a direct cycle (child already parent of parent).
            let reverse: Option<i64> = c
                .query_row(
                    "SELECT id FROM card_links WHERE parent_id=?1 AND child_id=?2",
                    params![child_id, parent_id],
                    |r| r.get(0),
                )
                .optional()?;
            if reverse.is_some() {
                return Err(anyhow!("that link would create a cycle"));
            }
            // Idempotent: skip if the edge already exists.
            let existing: Option<i64> = c
                .query_row(
                    "SELECT id FROM card_links WHERE parent_id=?1 AND child_id=?2",
                    params![parent_id, child_id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = existing {
                return Ok(id);
            }
            c.execute(
                "INSERT INTO card_links(board_id, parent_id, child_id, created_at) VALUES(?1,?2,?3,?4)",
                params![pb, parent_id, child_id, now],
            )?;
            let id = c.last_insert_rowid();
            Self::touch(c, pb, now)?;
            Ok(id)
        })
    }

    pub fn remove_link(&self, parent_id: i64, child_id: i64, now: i64) -> Result<()> {
        self.with(|c| {
            let board_id = Self::board_of_card(c, child_id).ok();
            c.execute(
                "DELETE FROM card_links WHERE parent_id=?1 AND child_id=?2",
                params![parent_id, child_id],
            )?;
            if let Some(b) = board_id {
                Self::touch(c, b, now)?;
            }
            Ok(())
        })
    }

    /// All links touching a card, joined with the other card's title/done flag.
    pub fn links_of_card(&self, card_id: i64) -> Result<Vec<Link>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT l.id, l.parent_id, l.child_id, p.title, ch.title, p.done, ch.done
                 FROM card_links l
                 JOIN cards p  ON p.id  = l.parent_id
                 JOIN cards ch ON ch.id = l.child_id
                 WHERE l.parent_id=?1 OR l.child_id=?1
                 ORDER BY l.id",
            )?;
            let rows = stmt
                .query_map(params![card_id], |r| {
                    Ok(Link {
                        id: r.get(0)?,
                        parent_id: r.get(1)?,
                        child_id: r.get(2)?,
                        parent_title: r.get(3)?,
                        child_title: r.get(4)?,
                        parent_done: r.get::<_, i64>(5)? != 0,
                        child_done: r.get::<_, i64>(6)? != 0,
                    })
                })?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        })
    }

    // ---- comments ----

    pub fn add_comment(
        &self,
        card_id: i64,
        author: &str,
        body: &str,
        kind: &str,
        now: i64,
    ) -> Result<i64> {
        self.with(|c| {
            let board_id = Self::board_of_card(c, card_id)?;
            c.execute(
                "INSERT INTO card_comments(card_id, author, body, kind, created_at) VALUES(?1,?2,?3,?4,?5)",
                params![card_id, author, body, kind, now],
            )?;
            let id = c.last_insert_rowid();
            Self::touch(c, board_id, now)?;
            Ok(id)
        })
    }

    pub fn comments_of_card(&self, card_id: i64) -> Result<Vec<Comment>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, author, body, kind, created_at FROM card_comments WHERE card_id=?1 ORDER BY id",
            )?;
            let rows = stmt
                .query_map(params![card_id], |r| {
                    Ok(Comment {
                        id: r.get(0)?,
                        author: r.get(1)?,
                        body: r.get(2)?,
                        kind: r.get(3)?,
                        created_at: r.get(4)?,
                    })
                })?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        })
    }

    // ---- bulk insert (AI generation / templates) ----

    /// Insert whole columns (with their cards) into a board. Returns
    /// (columns_added, cards_added).
    pub fn insert_columns(
        &self,
        board_id: i64,
        cols: &[GenColumn],
        now: i64,
    ) -> Result<(usize, usize)> {
        self.with(|c| {
            let mut col_n = 0usize;
            let mut card_n = 0usize;
            let mut ord = Self::next_column_ord(c, board_id)?;
            for gc in cols {
                let title = gc.title.trim();
                if title.is_empty() {
                    continue;
                }
                let role = gc.role.as_deref().unwrap_or("custom");
                c.execute(
                    "INSERT INTO columns(board_id, title, role, color, wip_limit, ord, created_at)
                     VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![board_id, title, role, gc.color, gc.wip_limit, ord, now],
                )?;
                let column_id = c.last_insert_rowid();
                col_n += 1;
                ord += 1;
                let mut card_ord = 0i64;
                for gd in &gc.cards {
                    let ctitle = gd.title.trim();
                    if ctitle.is_empty() {
                        continue;
                    }
                    let labels = gd.labels.as_ref().and_then(|v| serde_json::to_string(v).ok());
                    c.execute(
                        "INSERT INTO cards(board_id, column_id, title, description, priority, assignee, labels,
                                           done, ord, created_at, updated_at)
                         VALUES(?1,?2,?3,?4,?5,?6,?7,0,?8,?9,?9)",
                        params![board_id, column_id, ctitle, gd.description.trim(), gd.priority, gd.assignee, labels, card_ord, now],
                    )?;
                    card_n += 1;
                    card_ord += 1;
                }
            }
            Self::touch(c, board_id, now)?;
            Ok((col_n, card_n))
        })
    }

    /// Insert a batch of cards into one column (AI card breakdown). Returns count.
    pub fn insert_cards(&self, column_id: i64, cards: &[GenCard], now: i64) -> Result<usize> {
        self.with(|c| {
            let board_id = Self::board_of_column(c, column_id)?;
            let mut ord = Self::next_card_ord(c, column_id)?;
            let mut n = 0usize;
            for gd in cards {
                let title = gd.title.trim();
                if title.is_empty() {
                    continue;
                }
                let labels = gd.labels.as_ref().and_then(|v| serde_json::to_string(v).ok());
                c.execute(
                    "INSERT INTO cards(board_id, column_id, title, description, priority, assignee, labels,
                                       done, ord, created_at, updated_at)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,0,?8,?9,?9)",
                    params![board_id, column_id, title, gd.description.trim(), gd.priority, gd.assignee, labels, ord, now],
                )?;
                n += 1;
                ord += 1;
            }
            Self::touch(c, board_id, now)?;
            Ok(n)
        })
    }

    /// A compact text outline of a board (for grounding the chat / breakdown).
    pub fn board_outline(&self, board_id: i64) -> Result<String> {
        let cols = self.board_full(board_id)?;
        let mut out = String::new();
        for col in &cols {
            out.push_str(&format!("## {}\n", col.column.title));
            for card in &col.cards {
                let done = if card.done { "[x]" } else { "[ ]" };
                let pri = card
                    .priority
                    .as_deref()
                    .map(|p| format!(" ({p})"))
                    .unwrap_or_default();
                let who = card
                    .assignee
                    .as_deref()
                    .map(|a| format!(" @{a}"))
                    .unwrap_or_default();
                out.push_str(&format!("- {done} {}{}{}\n", card.title, pri, who));
            }
            out.push('\n');
        }
        Ok(out)
    }

    // ---- chat sessions ----

    pub fn list_sessions(&self, board_id: i64) -> Result<Vec<ChatSession>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT s.id, s.board_id, s.title, s.created_at, s.updated_at,
                        (SELECT COUNT(*) FROM chat_messages m WHERE m.session_id = s.id)
                 FROM chat_sessions s WHERE s.board_id=?1 ORDER BY s.updated_at DESC",
            )?;
            let rows = stmt
                .query_map(params![board_id], |r| {
                    Ok(ChatSession {
                        id: r.get(0)?,
                        board_id: r.get(1)?,
                        title: r.get(2)?,
                        created_at: r.get(3)?,
                        updated_at: r.get(4)?,
                        message_count: r.get(5)?,
                    })
                })?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        })
    }

    pub fn create_session(&self, board_id: i64, title: &str, now: i64) -> Result<i64> {
        self.with(|c| {
            c.execute(
                "INSERT INTO chat_sessions(board_id, title, created_at, updated_at) VALUES(?1,?2,?3,?3)",
                params![board_id, title, now],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn rename_session(&self, id: i64, title: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE chat_sessions SET title=?2 WHERE id=?1",
                params![id, title],
            )?;
            Ok(())
        })
    }

    pub fn delete_session(&self, id: i64) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM chat_messages WHERE session_id=?1", params![id])?;
            c.execute("DELETE FROM chat_sessions WHERE id=?1", params![id])?;
            Ok(())
        })
    }

    pub fn session_messages(&self, session_id: i64) -> Result<Vec<ChatMessageRow>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, role, content, model, created_at FROM chat_messages
                 WHERE session_id=?1 ORDER BY id",
            )?;
            let rows = stmt
                .query_map(params![session_id], |r| {
                    Ok(ChatMessageRow {
                        id: r.get(0)?,
                        role: r.get(1)?,
                        content: r.get(2)?,
                        model: r.get(3)?,
                        created_at: r.get(4)?,
                    })
                })?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        })
    }

    pub fn add_message(
        &self,
        session_id: i64,
        role: &str,
        content: &str,
        model: Option<&str>,
        now: i64,
    ) -> Result<i64> {
        self.with(|c| {
            c.execute(
                "INSERT INTO chat_messages(session_id, role, content, model, created_at) VALUES(?1,?2,?3,?4,?5)",
                params![session_id, role, content, model, now],
            )?;
            c.execute("UPDATE chat_sessions SET updated_at=?2 WHERE id=?1", params![session_id, now])?;
            Ok(c.last_insert_rowid())
        })
    }

    // ---- dispatch (claim / lease / reclaim) ----

    /// Atomically claim up to `total` ready cards across all boards (deps
    /// satisfied, under `per_assignee` in-progress limit), moving each into its
    /// board's `in_progress` column with a lease. Returns the claimed cards.
    pub fn dispatch_claim(
        &self,
        total: usize,
        per_assignee: usize,
        lease_secs: i64,
        now: i64,
    ) -> Result<Vec<ClaimedCard>> {
        let mut out: Vec<ClaimedCard> = Vec::new();
        if total == 0 {
            return Ok(out);
        }
        for board in self.list_boards()? {
            if out.len() >= total {
                break;
            }
            let cols = self.board_full(board.id)?;
            let ready = cols.iter().find(|c| c.column.role == "ready");
            let inprog = cols.iter().find(|c| c.column.role == "in_progress");
            let (ready, inprog) = match (ready, inprog) {
                (Some(r), Some(p)) => (r, p),
                _ => continue, // board without the standard workflow — skip
            };
            let inprog_id = inprog.column.id;
            // Current per-assignee load in the in-progress column.
            let mut load: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            for card in &inprog.cards {
                if let Some(a) = &card.assignee {
                    *load.entry(a.clone()).or_default() += 1;
                }
            }
            // Ready candidates with no open dependency, highest priority first.
            let mut cands: Vec<&Card> = ready.cards.iter().filter(|c| c.open_deps == 0).collect();
            cands.sort_by(|a, b| {
                prio_rank(&b.priority)
                    .cmp(&prio_rank(&a.priority))
                    .then(a.ord.cmp(&b.ord))
            });
            for card in cands {
                if out.len() >= total {
                    break;
                }
                if per_assignee > 0 {
                    if let Some(a) = &card.assignee {
                        if load.get(a).copied().unwrap_or(0) >= per_assignee {
                            continue;
                        }
                    }
                }
                // Move to In Progress + stamp the lease.
                self.move_card(card.id, inprog_id, 0, now)?;
                self.with(|c| {
                    c.execute(
                        "UPDATE cards SET claimed_by='dispatcher', lease_until=?2 WHERE id=?1",
                        params![card.id, now + lease_secs],
                    )?;
                    Ok(())
                })?;
                if let Some(a) = &card.assignee {
                    *load.entry(a.clone()).or_default() += 1;
                }
                out.push(ClaimedCard {
                    id: card.id,
                    assignee: card.assignee.clone(),
                    title: card.title.clone(),
                    description: card.description.clone(),
                    priority: card.priority.clone(),
                    workspace_dir: board.workspace_dir.clone(),
                    labels: card
                        .labels
                        .as_deref()
                        .and_then(|l| serde_json::from_str::<Vec<String>>(l).ok())
                        .unwrap_or_default(),
                });
            }
        }
        Ok(out)
    }

    /// Extend a claimed card's lease.
    pub fn dispatch_heartbeat(&self, card_id: i64, lease_secs: i64, now: i64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE cards SET lease_until=?2 WHERE id=?1",
                params![card_id, now + lease_secs],
            )?;
            Ok(())
        })
    }

    /// Return cards whose lease expired (worker died) to the `ready` column, clear
    /// the claim, and note it. Returns the reclaimed card ids.
    pub fn dispatch_reclaim(&self, now: i64) -> Result<Vec<i64>> {
        let expired: Vec<(i64, i64)> = self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, board_id FROM cards
                 WHERE claimed_by IS NOT NULL AND lease_until IS NOT NULL AND lease_until < ?1 AND done=0",
            )?;
            let v = stmt
                .query_map(params![now], |r| Ok((r.get(0)?, r.get(1)?)))?
                .filter_map(|r| r.ok())
                .collect();
            Ok(v)
        })?;
        let mut ids = Vec::new();
        for (card_id, board_id) in expired {
            if let Some(ready) = self.column_by_role(board_id, "ready")? {
                self.move_card(card_id, ready, 0, now)?;
                self.clear_claim(card_id)?;
                let _ = self.add_comment(
                    card_id,
                    "dispatcher",
                    "stale: worker lease expired, returned to Ready",
                    "system",
                    now,
                );
                ids.push(card_id);
            }
        }
        Ok(ids)
    }

    /// Hermes-style promotion: move `todo` cards whose dependencies are all done
    /// (`open_deps == 0`) into the board's `ready` column, so the dispatcher can
    /// claim them. `Triage` is deliberately NOT promoted — it stays a human
    /// review stage. Returns the promoted card ids.
    pub fn dispatch_promote(&self, now: i64) -> Result<Vec<i64>> {
        let mut promoted = Vec::new();
        for board in self.list_boards()? {
            let cols = self.board_full(board.id)?;
            let todo = cols.iter().find(|c| c.column.role == "todo");
            let ready = cols.iter().find(|c| c.column.role == "ready");
            let (todo, ready) = match (todo, ready) {
                (Some(t), Some(r)) => (t, r.column.id),
                _ => continue,
            };
            for card in &todo.cards {
                if card.open_deps == 0 && !card.done {
                    self.move_card(card.id, ready, i64::MAX, now)?; // append at bottom
                    promoted.push(card.id);
                }
            }
        }
        Ok(promoted)
    }

    /// A cheap change signature per board: `(id, boards.updated_at, card_count)`.
    /// The daemon's kanban WS watcher polls this to detect changes made by ANY
    /// writer of the shared SQLite file (REST, in-process dispatcher, or the
    /// separate `kanban-server` stdio MCP process) and pushes `kanban:update`.
    pub fn change_signature(&self) -> Result<Vec<(i64, i64, i64)>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT b.id, b.updated_at,
                        (SELECT COUNT(*) FROM cards d WHERE d.board_id = b.id)
                 FROM boards b ORDER BY b.id",
            )?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        })
    }

    /// Cards currently being worked (claimed, in the in_progress column) — the
    /// board's live "running tasks" list for the activity drawer.
    pub fn activity_running(&self, board_id: i64) -> Result<Vec<Card>> {
        let cols = self.board_full(board_id)?;
        Ok(cols
            .into_iter()
            .filter(|c| c.column.role == "in_progress")
            .flat_map(|c| c.cards)
            .collect())
    }

    /// The board's most recent comments (worker summaries, block reasons, notes),
    /// newest first, joined with each card's title.
    pub fn activity_recent(&self, board_id: i64, limit: i64) -> Result<Vec<ActivityItem>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT m.card_id, d.title, m.author, m.body, m.kind, m.created_at
                 FROM card_comments m JOIN cards d ON d.id = m.card_id
                 WHERE d.board_id = ?1
                 ORDER BY m.id DESC LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![board_id, limit], |r| {
                    Ok(ActivityItem {
                        card_id: r.get(0)?,
                        card_title: r.get(1)?,
                        author: r.get(2)?,
                        body: r.get(3)?,
                        kind: r.get(4)?,
                        created_at: r.get(5)?,
                    })
                })?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        })
    }

    /// Clear a card's claim/lease (called on finalize).
    pub fn clear_claim(&self, card_id: i64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE cards SET claimed_by=NULL, lease_until=NULL WHERE id=?1",
                params![card_id],
            )?;
            Ok(())
        })
    }

    /// The role of the column a card currently sits in.
    pub fn card_role(&self, card_id: i64) -> Result<Option<String>> {
        self.with(|c| {
            let r = c
                .query_row(
                    "SELECT k.role FROM cards d JOIN columns k ON k.id = d.column_id WHERE d.id=?1",
                    params![card_id],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(r)
        })
    }
}

/// Priority ordering for claim (higher runs first).
fn prio_rank(p: &Option<String>) -> i32 {
    match p.as_deref() {
        Some("urgent") => 3,
        Some("high") => 2,
        Some("medium") => 1,
        _ => 0,
    }
}

/// A card claimed by the dispatcher, with the fields needed to build a work item.
pub struct ClaimedCard {
    pub id: i64,
    pub assignee: Option<String>,
    pub title: String,
    pub description: String,
    pub priority: Option<String>,
    /// The board's working directory (worker runs there; None = scratch).
    pub workspace_dir: Option<String>,
    /// Card labels; `worktree` asks for an isolated git worktree.
    pub labels: Vec<String>,
}

/// One row in the board's recent-activity feed (a comment joined with its card).
#[derive(Serialize)]
pub struct ActivityItem {
    pub card_id: i64,
    pub card_title: String,
    pub author: String,
    pub body: String,
    pub kind: String,
    pub created_at: i64,
}

/// Per-app data dir, e.g. `~/.senclaw/space-apps/kanban/`.
pub fn default_data_dir(app: &str) -> PathBuf {
    let base = std::env::var("SENCLAW_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".senclaw")
        });
    base.join("space-apps").join(app)
}
