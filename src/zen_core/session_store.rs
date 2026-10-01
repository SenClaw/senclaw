//! Persist / hydrate the LLM trajectory (`Vec<Message>`) per chat JID.
//!
//! The UI transcript (`group_messages`) and the engine history are separate.
//! Without this store, `create_session` / daemon restart wipe the in-memory
//! trajectory while the UI still shows the chat — the model then "forgets"
//! the session. Layout: `~/.senclaw/llm-sessions/<safe_jid>.json`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::Message;

fn root() -> PathBuf {
    std::env::var("SENCLAW_LLM_SESSIONS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".senclaw")
                .join("llm-sessions")
        })
}

fn safe_jid(jid: &str) -> String {
    jid.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn path_for(jid: &str) -> PathBuf {
    root().join(format!("{}.json", safe_jid(jid)))
}

#[cfg(unix)]
fn restrict_dir(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
}
#[cfg(not(unix))]
fn restrict_dir(_p: &Path) {}

#[cfg(unix)]
fn restrict_file(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn restrict_file(_p: &Path) {}

/// Save the full LLM message list for this chat. Empty list removes the file.
pub fn save(jid: &str, messages: &[Message]) -> Result<()> {
    if jid.is_empty() {
        return Ok(());
    }
    let path = path_for(jid);
    if messages.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    let dir = root();
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    restrict_dir(&dir);
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(messages).context("serialize llm session")?;
    std::fs::write(&tmp, &bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("rename {}", path.display()))?;
    restrict_file(&path);
    Ok(())
}

/// Load a previously saved trajectory. `None` when missing or unreadable.
pub fn load(jid: &str) -> Option<Vec<Message>> {
    if jid.is_empty() {
        return None;
    }
    let path = path_for(jid);
    let raw = std::fs::read(&path).ok()?;
    match serde_json::from_slice::<Vec<Message>>(&raw) {
        Ok(msgs) if !msgs.is_empty() => Some(msgs),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(
                jid,
                path = %path.display(),
                error = %e,
                "llm session store: corrupt file — ignoring"
            );
            None
        }
    }
}

/// Delete persisted trajectory (stop_and_clear / /reset).
pub fn clear(jid: &str) -> Result<()> {
    if jid.is_empty() {
        return Ok(());
    }
    let path = path_for(jid);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zen_core::{create_user_message, ContentBlock};

    fn with_temp_root(test: impl FnOnce()) {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("SENCLAW_LLM_SESSIONS_DIR", dir.path());
        test();
        std::env::remove_var("SENCLAW_LLM_SESSIONS_DIR");
    }

    #[test]
    fn round_trip_save_load_clear() {
        with_temp_root(|| {
            let jid = "web:test-session";
            let msgs = vec![create_user_message(vec![ContentBlock::Text {
                text: "hello".into(),
            }])];
            save(jid, &msgs).unwrap();
            let loaded = load(jid).expect("loaded");
            assert_eq!(loaded.len(), 1);
            clear(jid).unwrap();
            assert!(load(jid).is_none());
        });
    }

    #[test]
    fn empty_save_removes_file() {
        with_temp_root(|| {
            let jid = "web:empty";
            let msgs = vec![create_user_message(vec![ContentBlock::Text {
                text: "x".into(),
            }])];
            save(jid, &msgs).unwrap();
            save(jid, &[]).unwrap();
            assert!(load(jid).is_none());
        });
    }
}
