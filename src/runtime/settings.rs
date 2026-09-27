//! `~/.senclaw/runtimes/settings.json` — slot selections, update channel and
//! idle timeouts. Read at boot and after every change; written atomically
//! (`*.tmp` + rename) so a crash mid-save never corrupts it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sen_runtime_sdk::manifest::Slot;
use serde::{Deserialize, Serialize};

/// One package identity — what a slot is selected to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub id: String,
    /// `None` = newest installed version of `id` (re-resolved on every use, so
    /// an install of a newer version moves the slot without a settings write).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// `stable` tracks the index's `channels.stable` version; `beta` tracks
/// `channels.beta` (`"latest"` for upstream llama.cpp).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    #[default]
    Stable,
    Beta,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Beta => "beta",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdleTimeouts {
    pub service: u64,
    pub model: u64,
}

/// Defaults from `docs/runtime-protocol.md` §3.2 step 6.
impl Default for IdleTimeouts {
    fn default() -> Self {
        IdleTimeouts { service: 300, model: 900 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSettings {
    #[serde(default)]
    pub selections: BTreeMap<Slot, Selection>,
    #[serde(default = "yes")]
    pub auto_update: bool,
    #[serde(default)]
    pub channel: Channel,
    #[serde(default)]
    pub idle_timeout_secs: IdleTimeouts,
}

fn yes() -> bool {
    true
}

impl Default for RuntimeSettings {
    fn default() -> Self {
        RuntimeSettings {
            selections: BTreeMap::new(),
            auto_update: true,
            channel: Channel::default(),
            idle_timeout_secs: IdleTimeouts::default(),
        }
    }
}

fn settings_path(runtimes_dir: &Path) -> PathBuf {
    runtimes_dir.join("settings.json")
}

/// Load settings, defaulting on a missing or unreadable file — a hand-edited
/// mistake costs only itself, never a daemon that refuses to start.
pub fn load(runtimes_dir: &Path) -> RuntimeSettings {
    let path = settings_path(runtimes_dir);
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
            tracing::warn!("[runtime] ignoring unreadable {}: {e}", path.display());
            RuntimeSettings::default()
        }),
        Err(_) => RuntimeSettings::default(),
    }
}

pub fn save(runtimes_dir: &Path, settings: &RuntimeSettings) -> Result<()> {
    std::fs::create_dir_all(runtimes_dir)?;
    let path = settings_path(runtimes_dir);
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(settings)?;
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

impl RuntimeSettings {
    pub fn selected(&self, slot: Slot) -> Option<&Selection> {
        self.selections.get(&slot)
    }

    /// `id = None` clears the slot (`PUT /api/runtimes/selections`).
    pub fn select(&mut self, slot: Slot, id: Option<String>, version: Option<String>) {
        match id {
            Some(id) => {
                self.selections.insert(slot, Selection { id, version });
            }
            None => {
                self.selections.remove(&slot);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_stable_channel_auto_update_on() {
        let s = RuntimeSettings::default();
        assert_eq!(s.channel, Channel::Stable);
        assert!(s.auto_update);
        assert_eq!(s.idle_timeout_secs, IdleTimeouts { service: 300, model: 900 });
        assert!(s.selections.is_empty());
    }

    #[test]
    fn save_then_load_round_trips_selections() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = RuntimeSettings::default();
        s.select(Slot::Gguf, Some("llama.cpp-metal".into()), Some("b11201".into()));
        s.select(Slot::Ocr, Some("sen-ocr".into()), None);
        save(tmp.path(), &s).unwrap();

        let loaded = load(tmp.path());
        assert_eq!(loaded.selected(Slot::Gguf).unwrap().id, "llama.cpp-metal");
        assert_eq!(loaded.selected(Slot::Gguf).unwrap().version.as_deref(), Some("b11201"));
        assert_eq!(loaded.selected(Slot::Ocr).unwrap().version, None);
        assert!(loaded.selected(Slot::Tts).is_none());
    }

    #[test]
    fn clearing_a_selection_removes_it() {
        let mut s = RuntimeSettings::default();
        s.select(Slot::Decision, Some("sen-sysone".into()), None);
        assert!(s.selected(Slot::Decision).is_some());
        s.select(Slot::Decision, None, None);
        assert!(s.selected(Slot::Decision).is_none());
    }

    #[test]
    fn a_hand_broken_file_falls_back_to_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(settings_path(tmp.path()), "{not json").unwrap();
        assert_eq!(load(tmp.path()), RuntimeSettings::default());
    }
}
