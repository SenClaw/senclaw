//! Apple Notes, through `osascript` against the local Notes.app.
//!
//! **macOS only, and it says so.** The previous stub promised to reach iCloud
//! Notes over IMAP; that route stopped working when iCloud Notes moved off
//! IMAP, so there is no cross-platform path to promise. On another OS this
//! reports that plainly instead of returning a success shape with no notes in
//! it, which is indistinguishable from an empty account.
//!
//! Reading goes through JavaScript for Automation rather than AppleScript
//! string building: the note bodies are arbitrary user text, and JSON from
//! `JSON.stringify` survives quotes and newlines that a delimiter-joined
//! AppleScript string does not.

use anyhow::{Context, Result};

use crate::db::Db;

use super::store::upsert_note;
use super::SyncReport;

pub const SOURCE: &str = "apple-notes";
/// Notes.app is scripted synchronously; a large library still has to finish
/// inside something a user will wait for.
const TIMEOUT_SECS: u64 = 120;

/// Whether this machine can reach Notes.app at all.
pub fn supported() -> bool {
    cfg!(target_os = "macos")
}

/// The script that reads the library. Returns a JSON array of
/// `{id, name, body, modified}`.
///
/// `limit` caps the read: the first run of a decade-old library would
/// otherwise hold the daemon inside one `osascript` call.
fn read_script(limit: usize) -> String {
    format!(
        r#"
        const app = Application('Notes');
        app.includeStandardAdditions = true;
        const out = [];
        const notes = app.notes();
        const limit = Math.min(notes.length, {limit});
        for (let i = 0; i < limit; i++) {{
          const n = notes[i];
          try {{
            out.push({{
              id: n.id(),
              name: n.name(),
              body: n.plaintext(),
              modified: n.modificationDate().getTime(),
            }});
          }} catch (e) {{
            // A note the script cannot read (locked, or mid-sync) is skipped
            // rather than failing the whole run.
          }}
        }}
        JSON.stringify(out);
        "#
    )
}

#[derive(Debug, serde::Deserialize)]
struct AppleNote {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    body: String,
}

/// Pull notes into `space_notes`.
pub async fn sync(db: &Db, limit: usize) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    if !supported() {
        report.note_error(
            "Apple Notes can only be read on macOS, through the local Notes.app. \
             iCloud Notes is not reachable over IMAP, so there is no cross-platform route.",
        );
        return Ok(report);
    }

    let script = read_script(limit.clamp(1, 5_000));
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(TIMEOUT_SECS),
        tokio::process::Command::new("osascript")
            .arg("-l")
            .arg("JavaScript")
            .arg("-e")
            .arg(&script)
            .output(),
    )
    .await
    .context("Notes.app did not answer in time")?
    .context("run osascript")?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        // The first scripted access raises a TCC prompt; until it is granted
        // every call fails the same way, and saying so is more useful than
        // the raw error.
        if err.contains("Not authorized") || err.contains("-1743") {
            report.note_error(
                "macOS denied access to Notes.app — grant it under \
                 System Settings → Privacy & Security → Automation",
            );
            report.needs_reauth = true;
            return Ok(report);
        }
        anyhow::bail!("osascript failed: {}", err.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let notes: Vec<AppleNote> =
        serde_json::from_str(stdout.trim()).context("parse Notes.app output")?;

    for n in notes {
        let title = if n.name.trim().is_empty() {
            first_line(&n.body)
        } else {
            n.name.clone()
        };
        upsert_note(db, SOURCE, &n.id, &title, &n.body, &mut report);
    }
    Ok(report)
}

/// A title for a note that has none: its first non-empty line.
fn first_line(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(80).collect())
        .unwrap_or_else(|| "(untitled)".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_is_bounded_and_returns_json() {
        let s = read_script(50);
        assert!(s.contains("Math.min(notes.length, 50)"));
        assert!(s.contains("JSON.stringify(out)"));
    }

    #[test]
    fn a_note_without_a_name_borrows_its_first_line() {
        assert_eq!(first_line("\n\n  Shopping list\nmilk"), "Shopping list");
        assert_eq!(first_line("   "), "(untitled)");
        assert_eq!(first_line(&"x".repeat(200)).len(), 80);
    }

    #[test]
    fn output_parses_into_notes() {
        let notes: Vec<AppleNote> = serde_json::from_str(
            r#"[{"id":"x-coredata://1","name":"Ideas","body":"line \"quoted\"\nand more"}]"#,
        )
        .unwrap();
        assert_eq!(notes[0].id, "x-coredata://1");
        assert!(notes[0].body.contains('"'), "quotes survive the JSON round trip");
    }

    #[test]
    fn support_matches_the_platform_that_has_notes_app() {
        // An empty success is indistinguishable from an empty account, which
        // is what the old stub returned everywhere. `sync` checks this first
        // and reports the reason instead.
        assert_eq!(supported(), cfg!(target_os = "macos"));
    }
}
