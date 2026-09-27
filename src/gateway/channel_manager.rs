//! Channel lifecycle management.
//! Channels represent messaging platform connections (Telegram bot, Feishu app, etc.).

use std::sync::Mutex;

use anyhow::Result;

use crate::db::Db;
use crate::types::Channel;

pub struct ChannelManager {
    on_changed: Mutex<Option<Box<dyn Fn() + Send + 'static>>>,
}

impl ChannelManager {
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

    pub fn create(
        &self,
        db: &Db,
        platform_type: &str,
        name: &str,
        credentials_json: &str,
        now: &str,
    ) -> Result<Channel> {
        let id = db.insert_channel(platform_type, name, credentials_json, now)?;
        self.fire_changed();
        db.get_channel(id)?
            .ok_or_else(|| anyhow::anyhow!("Channel {id} not found after insert"))
    }

    pub fn get(&self, db: &Db, id: i64) -> Result<Option<Channel>> {
        db.get_channel(id)
    }

    pub fn list(&self, db: &Db) -> Result<Vec<Channel>> {
        db.list_channels()
    }

    pub fn delete(&self, db: &Db, id: i64) -> Result<()> {
        db.delete_channel(id)?;
        self.fire_changed();
        Ok(())
    }

    pub fn update(
        &self,
        db: &Db,
        id: i64,
        name: Option<&str>,
        credentials_json: Option<&str>,
        now: &str,
    ) -> Result<()> {
        db.update_channel(id, name, credentials_json, now)?;
        self.fire_changed();
        Ok(())
    }
}

impl Default for ChannelManager {
    fn default() -> Self {
        Self::new()
    }
}
