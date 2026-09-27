//! SQLite handle. Mirrors `src-old/db/db.ts`.
//!
//! Tables owned here:
//!   * `groups`            — GroupBinding registry (legacy, being phased out)
//!   * `channels`          — platform connections (Telegram bot, Feishu, WeChat, etc.)
//!   * `agents`            — AI agent definitions (folder, name, tools, model, etc.)
//!   * `bindings`          — N:N join: agent ↔ channel + per-chat JID
//!   * `channel_pairings`  — unknown chats asking to be let in (code + approval)
//!   * `channel_messages`  — raw platform messages (incoming from Telegram/Feishu/etc.)
//!   * `group_messages`    — conversation history (user messages + bot responses)
//!   * `scheduled_tasks`   — scheduler entries (the *user's* schedule)
//!   * `task_run_logs`     — task execution log
//!   * `background_tasks`  — autonomous work SenClaw runs by itself (no chat)
//!   * `background_runs`   — one row per background session
//!   * `background_activity` — background-session transcripts
//!   * `router_state`      — KV cursor (e.g. lastAgentTimestamp)
//!
//! Memory tables live in [`crate::memory::schema`] and are applied during
//! [`Db::open`] alongside the schema here.
//!
//! The handle wraps a single [`rusqlite::Connection`] under a [`Mutex`].
//! That matches the TS one-process model and keeps this layer simple. If we
//! ever need real concurrency, swap to `tokio_rusqlite` or a connection pool.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::config::Config;

mod helpers;
mod rows;
mod schema;

mod agent_todos;
mod agents;
pub mod background;
mod bindings;
mod channels;
mod chat_events;
mod checkpoints;
mod chat_widgets;
pub mod code_artifacts;
pub mod cowork_tasks;
pub mod cowork_teams;
pub mod cowork_templates;
mod dispatch_activity;
mod embedding;
pub mod failures;
pub(crate) mod event_notifications;
mod groups;
mod messages;
pub mod pairings;
pub mod plans;
mod router_state;
mod scheduled_tasks;
pub mod skill_route;
pub mod tool_aliases;
pub mod tool_gate;
mod tool_executions;
mod tool_rules;
pub mod usage;

#[cfg(test)]
mod tests;

pub struct Db {
    conn: Mutex<Connection>,
    /// Separate SQLite file for the cognitive memory layer. Kept apart
    /// from the main DB so:
    ///   * `rm senclaw_cognitive.db` is a safe full reset of the graph,
    ///     with no risk of clobbering chat history / schedules / channels.
    ///   * Backup cadence can differ — the cognitive graph is rebuildable
    ///     from SOUL.md + user messages; the main DB isn't.
    ///   * Heavy cog_edges churn from Hebbian write-back doesn't bloat
    ///     the main DB's WAL.
    /// In-memory tests share the same fate: both connections are `:memory:`
    /// so test fixtures don't leak files.
    cog_conn: Mutex<Connection>,
}

impl Db {
    /// Open (or create) the SQLite database, ensure pragmas, schema, and
    /// memory tables are in place. Idempotent.
    pub fn open(config: &Config) -> Result<Self> {
        if let Some(parent) = config.paths.db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create dir {}", parent.display()))?;
        }
        if let Some(parent) = config.paths.cognitive_db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create dir {}", parent.display()))?;
        }
        Self::open_at(
            &config.paths.db_path,
            &config.paths.cognitive_db_path,
            config,
        )
    }

    /// Open a DB at explicit paths — used by tests + when callers want to
    /// override the configured location. The two paths can equal each
    /// other (single-file mode); we open separate `Connection`s either
    /// way so locking is well-behaved.
    pub fn open_at(main_path: &Path, cog_path: &Path, config: &Config) -> Result<Self> {
        let mut conn = Connection::open(main_path)
            .with_context(|| format!("open sqlite {}", main_path.display()))?;
        Self::apply_pragmas_and_main_schema(&mut conn, config)?;

        let mut cog_conn = Connection::open(cog_path)
            .with_context(|| format!("open cognitive sqlite {}", cog_path.display()))?;
        Self::apply_pragmas_and_cog_schema(&mut cog_conn, config)?;

        // Owner-only, after the pragmas: WAL mode creates the `-wal`/`-shm`
        // sidecars on first use, so restricting earlier would miss them. These
        // files hold channel bot tokens, Space-App access tokens and the whole
        // conversation history — they were created 0644 under the default umask.
        crate::util::file_perms::restrict_sqlite(main_path);
        crate::util::file_perms::restrict_sqlite(cog_path);

        Ok(Self {
            conn: Mutex::new(conn),
            cog_conn: Mutex::new(cog_conn),
        })
    }

    /// In-memory DB. Used by integration tests and CLI dry-runs.
    /// Both the main and cognitive connections point at separate
    /// `:memory:` instances — fixtures stay self-contained.
    pub fn open_in_memory(config: &Config) -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        Self::apply_pragmas_and_main_schema(&mut conn, config)?;
        let mut cog_conn = Connection::open_in_memory()?;
        Self::apply_pragmas_and_cog_schema(&mut cog_conn, config)?;
        Ok(Self {
            conn: Mutex::new(conn),
            cog_conn: Mutex::new(cog_conn),
        })
    }

    fn apply_pragmas_and_main_schema(conn: &mut Connection, config: &Config) -> Result<()> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        schema::apply_schema(conn)?;
        schema::apply_memory_tables(conn, config)?;
        schema::apply_space_tables(conn)?;
        schema::apply_marketplace_tables(conn)?;
        Ok(())
    }

    fn apply_pragmas_and_cog_schema(conn: &mut Connection, config: &Config) -> Result<()> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        crate::memory::cognitive::schema::apply_cognitive_schema(conn)?;
        // cog_vec is best-effort: if sqlite-vec extension is loaded, the
        // virtual table is created; otherwise we fall back to the BLOB
        // column on cog_nodes (see graph_store / vector_store).
        let provider = config.memory.embedding_provider;
        if provider != crate::config::EmbeddingProvider::None {
            let dims = crate::config::Config::resolve_dimensions(
                provider,
                config.memory.embedding_dimensions,
            );
            let _ = crate::memory::cognitive::schema::apply_cognitive_vec_schema(conn, dims);
        }
        Ok(())
    }

    pub(crate) fn with_conn<R>(&self, f: impl FnOnce(&Connection) -> Result<R>) -> Result<R> {
        let guard = self.conn.lock().expect("db mutex poisoned");
        f(&guard)
    }

    /// Cognitive-DB accessor. SqliteGraphStore + SqliteVectorStore route
    /// every query through here. Kept `pub(crate)` so external callers
    /// can't bypass the cognitive abstractions and write to cog_* tables
    /// directly.
    pub(crate) fn with_cog_conn<R>(&self, f: impl FnOnce(&Connection) -> Result<R>) -> Result<R> {
        let guard = self.cog_conn.lock().expect("cognitive db mutex poisoned");
        f(&guard)
    }

    pub(crate) fn with_conn_mut<R>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<R>,
    ) -> Result<R> {
        let mut guard = self.conn.lock().expect("db mutex poisoned");
        f(&mut guard)
    }
}
