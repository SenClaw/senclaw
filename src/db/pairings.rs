//! Pairing requests — the gate an unknown chat passes before it gets a binding.
//!
//! Rows here are *requests*, not grants. A message from a chat with no binding
//! creates one and the bot answers with its `code`; only a human approving that
//! code turns it into a binding. Nothing in this file grants access on its own.

use anyhow::Result;
use rusqlite::{params, OptionalExtension};

use crate::types::ChannelPairing;

use super::rows::row_to_pairing;

/// How long a code stays usable. Matches the hour a person needs to walk from
/// "the bot answered me" to the Settings page, and bounds how long a stale
/// request sits there looking approvable.
pub const PAIRING_TTL_SECS: i64 = 3600;

/// Code alphabet: no `0`/`O`, no `1`/`I`/`L`. The code is read off a phone and
/// typed into a browser, so the pairs that get misread are simply absent.
const CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
const CODE_LEN: usize = 8;

/// One random code. Collisions are caught by the UNIQUE index on `code`, not by
/// trusting this function — see [`Db::create_pairing`].
fn generate_code() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..CODE_LEN)
        .map(|_| CODE_ALPHABET[rng.gen_range(0..CODE_ALPHABET.len())] as char)
        .collect()
}

/// Normalize whatever the user typed/sent into a comparable code: strip spaces
/// and dashes, upper-case. A person forwarding a code from Telegram brings
/// along formatting the sender never intended.
pub fn normalize_code(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

fn now_utc() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

/// Is this RFC3339 stamp in the past? An unparseable stamp counts as expired —
/// a row we cannot read the deadline of must not stay approvable forever.
pub fn is_expired(expires_at: &str) -> bool {
    match chrono::DateTime::parse_from_rfc3339(expires_at) {
        Ok(t) => t.with_timezone(&chrono::Utc) <= now_utc(),
        Err(_) => true,
    }
}

impl super::Db {
    // ============================================================
    // Channel pairings
    // ============================================================

    /// The live pending request for this chat, if one exists and has not
    /// expired. Used to answer a repeat message with the *same* code instead of
    /// minting a new one on every "hello".
    pub fn find_live_pairing(&self, channel_id: i64, chat_jid: &str) -> Result<Option<ChannelPairing>> {
        let found: Option<ChannelPairing> = self.with_conn(|c| {
            c.query_row(
                "SELECT * FROM channel_pairings
                 WHERE channel_id=?1 AND chat_jid=?2 AND status='pending'
                 ORDER BY id DESC LIMIT 1",
                params![channel_id, chat_jid],
                |r| Ok(row_to_pairing(r)),
            )
            .optional()?
            .transpose()
        })?;
        Ok(found.filter(|p| !is_expired(&p.expires_at)))
    }

    /// Mint a pending request. Retries on the (vanishingly rare) code
    /// collision the UNIQUE index rejects, rather than handing back a code that
    /// already belongs to somebody else's chat.
    #[allow(clippy::too_many_arguments)]
    pub fn create_pairing(
        &self,
        channel_id: i64,
        chat_jid: &str,
        chat_type: &str,
        sender_jid: &str,
        sender_name: &str,
        chat_title: Option<&str>,
        bot_token: Option<&str>,
    ) -> Result<ChannelPairing> {
        let now = now_utc();
        let created_at = now.to_rfc3339();
        let expires_at = (now + chrono::Duration::seconds(PAIRING_TTL_SECS)).to_rfc3339();

        for _ in 0..8 {
            let code = generate_code();
            let inserted = self.with_conn(|c| {
                let r = c.execute(
                    "INSERT INTO channel_pairings
                       (channel_id,chat_jid,chat_type,sender_jid,sender_name,chat_title,
                        code,status,bot_token,created_at,expires_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,'pending',?8,?9,?10)",
                    params![
                        channel_id, chat_jid, chat_type, sender_jid, sender_name, chat_title,
                        code, bot_token, created_at, expires_at
                    ],
                );
                match r {
                    Ok(_) => Ok(Some(c.last_insert_rowid())),
                    // UNIQUE(code) — try another one.
                    Err(rusqlite::Error::SqliteFailure(e, _))
                        if e.code == rusqlite::ErrorCode::ConstraintViolation =>
                    {
                        Ok(None)
                    }
                    Err(e) => Err(e.into()),
                }
            })?;
            if let Some(id) = inserted {
                return Ok(ChannelPairing {
                    id,
                    channel_id,
                    chat_jid: chat_jid.to_string(),
                    chat_type: chat_type.to_string(),
                    sender_jid: sender_jid.to_string(),
                    sender_name: sender_name.to_string(),
                    chat_title: chat_title.map(str::to_string),
                    code,
                    status: "pending".to_string(),
                    bot_token: bot_token.map(str::to_string),
                    created_at,
                    expires_at,
                    resolved_at: None,
                });
            }
        }
        anyhow::bail!("could not generate a unique pairing code")
    }

    pub fn get_pairing(&self, id: i64) -> Result<Option<ChannelPairing>> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT * FROM channel_pairings WHERE id=?1",
                params![id],
                |r| Ok(row_to_pairing(r)),
            )
            .optional()?
            .transpose()
        })
    }

    /// Look a request up by the code a person typed. Pending only — an
    /// already-resolved code must not be replayable.
    pub fn find_pending_pairing_by_code(&self, code: &str) -> Result<Option<ChannelPairing>> {
        let code = normalize_code(code);
        if code.is_empty() {
            return Ok(None);
        }
        self.with_conn(|c| {
            c.query_row(
                "SELECT * FROM channel_pairings WHERE code=?1 AND status='pending'",
                params![code],
                |r| Ok(row_to_pairing(r)),
            )
            .optional()?
            .transpose()
        })
    }

    /// Every request, newest first. `pending_only` hides the audit trail of
    /// what was already approved or turned away.
    pub fn list_pairings(&self, pending_only: bool) -> Result<Vec<ChannelPairing>> {
        self.with_conn(|c| {
            let sql = if pending_only {
                "SELECT * FROM channel_pairings WHERE status='pending' ORDER BY id DESC"
            } else {
                "SELECT * FROM channel_pairings ORDER BY id DESC"
            };
            let mut stmt = c.prepare(sql)?;
            let rows: Vec<_> = stmt
                .query_map([], |r| Ok(row_to_pairing(r)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().collect::<Result<Vec<_>>>()
        })
    }

    /// Move a request out of `pending`. Narrowed to `status='pending'` and
    /// returning the row count so a caller can tell "approved it" from
    /// "somebody else already did" — an `Ok` on a no-op update would let two
    /// approvers both believe they were the one who let this chat in.
    pub fn resolve_pairing(&self, id: i64, status: &str) -> Result<usize> {
        let now = now_utc().to_rfc3339();
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE channel_pairings SET status=?1, resolved_at=?2
                 WHERE id=?3 AND status='pending'",
                params![status, now, id],
            )?;
            Ok(n)
        })
    }

    /// Retire everything still pending for a chat — called after one of its
    /// requests is approved, so an older code cannot be approved a second time
    /// into a chat that is already bound.
    pub fn expire_other_pairings(&self, channel_id: i64, chat_jid: &str, keep_id: i64) -> Result<usize> {
        let now = now_utc().to_rfc3339();
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE channel_pairings SET status='superseded', resolved_at=?1
                 WHERE channel_id=?2 AND chat_jid=?3 AND id<>?4 AND status='pending'",
                params![now, channel_id, chat_jid, keep_id],
            )?;
            Ok(n)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_shape_and_alphabet() {
        for _ in 0..200 {
            let c = generate_code();
            assert_eq!(c.len(), CODE_LEN);
            assert!(c.bytes().all(|b| CODE_ALPHABET.contains(&b)), "{c}");
            // The look-alike characters must never appear.
            assert!(!c.contains(['0', 'O', '1', 'I', 'L']), "{c}");
        }
    }

    #[test]
    fn normalize_strips_formatting_a_human_adds() {
        assert_eq!(normalize_code(" ab3d-4f5g "), "AB3D4F5G");
        assert_eq!(normalize_code("AB3D4F5G"), "AB3D4F5G");
        assert_eq!(normalize_code("  --  "), "");
    }

    #[test]
    fn unreadable_deadline_counts_as_expired() {
        assert!(is_expired("not a date"));
        assert!(is_expired("2000-01-01T00:00:00Z"));
        assert!(!is_expired("2999-01-01T00:00:00Z"));
    }
}
