//! Pulling a space's calendar and notes in from outside.
//!
//! Three tools used to take a credential, say "Token received and stored",
//! and do nothing — which reads to a user as a sync that worked. Each one now
//! either performs the sync or says plainly that it cannot on this machine.
//!
//! * [`google_calendar`] — Calendar API v3, incremental via `syncToken`.
//! * [`caldav`] — iCloud and any other CalDAV server, over plain HTTP verbs.
//! * [`apple_notes`] — Notes.app through `osascript`; macOS only, and it says
//!   so everywhere else rather than pretending.
//!
//! All three land in the same two tables the space's own UI reads
//! (`space_events`, `space_notes`) and report the same shape, so a caller can
//! tell what happened without knowing which backend ran:
//!
//! ```json
//! { "synced": 12, "created": 3, "updated": 9, "errors": [], "needsReauth": false }
//! ```

pub mod apple_notes;
pub mod caldav;
pub mod google_calendar;
pub mod ical;
pub mod store;

use serde::Serialize;

/// What a sync did. Every backend answers in this shape.
#[derive(Debug, Default, Clone, Serialize)]
pub struct SyncReport {
    /// Items the remote returned and we understood.
    pub synced: usize,
    pub created: usize,
    pub updated: usize,
    /// Problems that did not stop the sync — one bad event should not lose
    /// the other forty.
    pub errors: Vec<String>,
    /// The credential was rejected. The caller must stop retrying and ask a
    /// person for a new one; a loop here burns a rate limit for nothing.
    #[serde(rename = "needsReauth")]
    pub needs_reauth: bool,
    /// Opaque cursor to hand back next time, when the backend has one.
    #[serde(rename = "syncToken", skip_serializing_if = "Option::is_none")]
    pub sync_token: Option<String>,
}

impl SyncReport {
    pub fn note_error(&mut self, e: impl std::fmt::Display) {
        // A hundred identical parse failures are one fact, not a hundred.
        let msg = e.to_string();
        if self.errors.len() < 20 && !self.errors.contains(&msg) {
            self.errors.push(msg);
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}))
    }
}
