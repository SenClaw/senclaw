//! Binding lifecycle management.
//! Bindings link a chat JID to an Agent on a Channel (N:N cardinality).

use std::sync::Mutex;

use anyhow::Result;

use crate::db::Db;
use crate::types::{Binding, BindingWithRelations};

pub struct BindingManager {
    on_changed: Mutex<Option<Box<dyn Fn() + Send + 'static>>>,
}

impl BindingManager {
    pub fn new() -> Self {
        Self {
            on_changed: Mutex::new(None),
        }
    }

    pub fn set_on_changed(&self, cb: Box<dyn Fn() + Send + 'static>) {
        if let Ok(mut guard) = self.on_changed.lock() {
            *guard = Some(cb);
        }
    }

    fn fire_changed(&self) {
        if let Ok(guard) = self.on_changed.lock() {
            if let Some(ref cb) = *guard {
                cb();
            }
        }
    }

    /// `senclaw` channels allow multiple bindings; all other platforms are
    /// exclusive — one channel, one agent.
    pub fn create(
        &self,
        db: &Db,
        jid: Option<&str>,
        agent_id: i64,
        channel_id: i64,
        bot_token_override: Option<&str>,
        max_messages: Option<u32>,
        now: &str,
    ) -> Result<Binding> {
        // Enforce exclusivity for non-senclaw channels.
        if let Some(ch) = db.get_channel(channel_id)? {
            if ch.platform_type != "senclaw" {
                let count = db.count_bindings_for_channel(channel_id)?;
                if count > 0 {
                    anyhow::bail!(
                        "Channel '{}' (platform: {}) already has an agent bound. \
                         Only Senclaw Connector channels support multiple bindings.",
                        ch.name,
                        ch.platform_type
                    );
                }
            }
        }
        let id = db.insert_binding(
            jid,
            agent_id,
            channel_id,
            bot_token_override,
            max_messages,
            now,
        )?;
        self.fire_changed();
        db.get_binding(id)?
            .ok_or_else(|| anyhow::anyhow!("Binding {id} not found after insert"))
    }

    pub fn get(&self, db: &Db, id: i64) -> Result<Option<Binding>> {
        db.get_binding(id)
    }

    pub fn get_by_jid(&self, db: &Db, jid: &str) -> Result<Option<Binding>> {
        db.get_binding_by_jid(jid)
    }

    /// Get binding + agent + channel in one JOIN (the primary lookup for message routing).
    pub fn get_with_relations(&self, db: &Db, jid: &str) -> Result<Option<BindingWithRelations>> {
        db.get_binding_with_relations(jid)
    }

    pub fn list(&self, db: &Db) -> Result<Vec<Binding>> {
        db.list_bindings()
    }

    pub fn list_with_relations(&self, db: &Db) -> Result<Vec<BindingWithRelations>> {
        db.list_bindings_with_relations()
    }

    pub fn delete(&self, db: &Db, id: i64) -> Result<()> {
        db.delete_binding(id)?;
        self.fire_changed();
        Ok(())
    }

    pub fn update(
        &self,
        db: &Db,
        id: i64,
        jid: Option<&str>,
        bot_token_override: Option<&str>,
        max_messages: Option<u32>,
        agent_id: Option<i64>,
    ) -> Result<()> {
        db.update_binding(id, jid, bot_token_override, max_messages, agent_id)?;
        self.fire_changed();
        Ok(())
    }

    /// Complete a pending binding (jid=NULL) when the first message arrives.
    /// Returns the now-complete Binding if found and updated.
    pub fn complete_pending(&self, db: &Db, channel_id: i64, jid: &str) -> Result<Option<Binding>> {
        let pending = db.get_pending_bindings_for_channel(channel_id)?;
        if let Some(b) = pending.into_iter().next() {
            db.complete_pending_binding(b.id, jid)?;
            self.fire_changed();
            return db.get_binding(b.id);
        }
        Ok(None)
    }

    /// Complete all pending (jid=NULL) bindings for a channel by assigning the real JID.
    /// Returns the number of rows updated.
    pub fn complete_pending_new_model(&self, db: &Db, channel_id: i64, jid: &str) -> Result<usize> {
        let pending = db.get_pending_bindings_for_channel(channel_id)?;
        let mut count = 0;
        for b in pending {
            db.complete_pending_binding(b.id, jid)?;
            count += 1;
        }
        if count > 0 {
            self.fire_changed();
        }
        Ok(count)
    }

    pub fn touch_active(&self, db: &Db, jid: &str, timestamp: &str) -> Result<()> {
        db.touch_binding_active(jid, timestamp)
    }
}

impl Default for BindingManager {
    fn default() -> Self {
        Self::new()
    }
}
