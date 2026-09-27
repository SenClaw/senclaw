//! Google Calendar, through the v3 REST API.
//!
//! Incremental by design: the first run asks for a window and keeps the
//! `nextSyncToken` the API returns; every run after that sends the token and
//! receives only what changed, including deletions. Re-listing the whole
//! calendar each time is what makes a sync expensive enough that people turn
//! the interval down until it stops being a sync.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::db::Db;

use super::ical::VEvent;
use super::store::{load_cursor, save_cursor, upsert_event};
use super::SyncReport;

pub const SOURCE: &str = "google";
const API_BASE: &str = "https://www.googleapis.com/calendar/v3";
/// Never walk more than this many pages in one run. A calendar that keeps
/// producing pages (a sync loop, a server bug) must not pin the daemon.
const MAX_PAGES: usize = 20;

#[derive(Debug, Deserialize)]
struct EventsResponse {
    #[serde(default)]
    items: Vec<GoogleEvent>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
    #[serde(rename = "nextSyncToken")]
    next_sync_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GoogleEvent {
    id: Option<String>,
    status: Option<String>,
    summary: Option<String>,
    description: Option<String>,
    location: Option<String>,
    start: Option<GoogleDate>,
    end: Option<GoogleDate>,
    #[serde(default)]
    recurrence: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct GoogleDate {
    /// RFC 3339, for a timed event.
    #[serde(rename = "dateTime")]
    date_time: Option<String>,
    /// `YYYY-MM-DD`, for an all-day event.
    date: Option<String>,
}

impl GoogleDate {
    /// Milliseconds since the epoch, and whether this was an all-day date.
    fn to_ms(&self) -> Option<(i64, bool)> {
        if let Some(dt) = self.date_time.as_deref() {
            let parsed = chrono::DateTime::parse_from_rfc3339(dt).ok()?;
            return Some((parsed.timestamp_millis(), false));
        }
        let d = self.date.as_deref()?;
        let date = chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()?;
        let dt = date.and_hms_opt(0, 0, 0)?;
        Some((
            chrono::TimeZone::from_utc_datetime(&chrono::Utc, &dt).timestamp_millis(),
            true,
        ))
    }
}

/// Map one API event onto the shape the store writes.
///
/// `status: "cancelled"` arrives with almost every other field missing —
/// that is how an incremental response reports a deletion — so it must be
/// recognised before anything requires a start time.
fn to_vevent(g: GoogleEvent) -> Option<VEvent> {
    let uid = g.id?;
    let cancelled = g
        .status
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("cancelled"));
    if cancelled {
        return Some(VEvent {
            uid,
            summary: g.summary.unwrap_or_default(),
            description: None,
            location: None,
            start_ms: 0,
            end_ms: 0,
            all_day: false,
            rrule: None,
            cancelled: true,
        });
    }
    let (start_ms, all_day) = g.start.as_ref()?.to_ms()?;
    let end_ms = g
        .end
        .as_ref()
        .and_then(|e| e.to_ms())
        .map(|(ms, _)| ms)
        .unwrap_or(if all_day {
            start_ms + 86_400_000
        } else {
            start_ms
        });
    Some(VEvent {
        uid,
        summary: g.summary.unwrap_or_else(|| "(no title)".to_string()),
        description: g.description,
        location: g.location,
        start_ms,
        end_ms,
        all_day,
        // Keep the RRULE line as sent; occurrences are the server's to expand.
        rrule: g
            .recurrence
            .into_iter()
            .find(|r| r.starts_with("RRULE:")),
        cancelled: false,
    })
}

/// Pull events into `space_events`.
///
/// `days` only matters on the first run — afterwards the stored sync token
/// decides the window, and mixing the two is an API error, not a narrower
/// query.
pub async fn sync(db: &Db, access_token: &str, days: u32, api_base: &str) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("http client")?;

    let stored_token = load_cursor(db, SOURCE);
    let mut page_token: Option<String> = None;
    let mut next_sync_token: Option<String> = None;

    for page in 0..MAX_PAGES {
        let mut req = http
            .get(format!("{api_base}/calendars/primary/events"))
            .bearer_auth(access_token)
            .query(&[("maxResults", "250"), ("singleEvents", "false")]);

        match (&stored_token, &page_token) {
            (_, Some(pt)) => req = req.query(&[("pageToken", pt.as_str())]),
            (Some(tok), None) => req = req.query(&[("syncToken", tok.as_str())]),
            (None, None) => {
                let from = chrono::Utc::now() - chrono::Duration::days(days.min(3650) as i64);
                let to = chrono::Utc::now() + chrono::Duration::days(days.min(3650) as i64);
                req = req.query(&[
                    ("timeMin", from.to_rfc3339().as_str()),
                    ("timeMax", to.to_rfc3339().as_str()),
                ]);
            }
        }

        let resp = req.send().await.context("calendar events request")?;
        let status = resp.status();

        // 401 means the credential is dead; retrying burns quota and never
        // recovers, so the caller is told to get a new one and stop.
        if status == reqwest::StatusCode::UNAUTHORIZED {
            report.needs_reauth = true;
            report.note_error("access token rejected (401) — re-authorise the Google account");
            return Ok(report);
        }
        // 410 is Google saying the stored sync token is too old. The fix is a
        // full re-list, not an error: drop the cursor and let the next run
        // start over.
        if status == reqwest::StatusCode::GONE {
            let _ = save_cursor(db, SOURCE, "");
            report.note_error("sync token expired (410) — the next run will re-list in full");
            return Ok(report);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("calendar events HTTP {status}: {body}");
        }

        let parsed: EventsResponse = resp.json().await.context("parse events response")?;
        for item in parsed.items {
            match to_vevent(item) {
                Some(e) => upsert_event(db, SOURCE, &e, &mut report),
                None => report.note_error("an event had no usable id or start time"),
            }
        }

        if let Some(tok) = parsed.next_sync_token {
            next_sync_token = Some(tok);
        }
        match parsed.next_page_token {
            Some(pt) => page_token = Some(pt),
            None => break,
        }
        if page + 1 == MAX_PAGES {
            report.note_error(format!("stopped after {MAX_PAGES} pages"));
        }
    }

    // Only persist the cursor once the whole run succeeded: storing it
    // mid-failure would skip past events that were never written.
    if let Some(tok) = &next_sync_token {
        if let Err(e) = save_cursor(db, SOURCE, tok) {
            report.note_error(format!("store sync token: {e}"));
        }
        report.sync_token = Some(tok.clone());
    }
    Ok(report)
}

/// The default API root; a test points this elsewhere.
pub fn default_api_base() -> &'static str {
    API_BASE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn google(json: &str) -> Option<VEvent> {
        to_vevent(serde_json::from_str(json).unwrap())
    }

    #[test]
    fn maps_a_timed_event() {
        let e = google(
            r#"{"id":"abc","summary":"Standup","location":"Room 2",
                "start":{"dateTime":"2026-09-12T09:00:00Z"},
                "end":{"dateTime":"2026-09-12T09:30:00Z"}}"#,
        )
        .unwrap();
        assert_eq!(e.uid, "abc");
        assert_eq!(e.summary, "Standup");
        assert_eq!(e.end_ms - e.start_ms, 30 * 60 * 1000);
        assert!(!e.all_day);
    }

    #[test]
    fn an_all_day_event_uses_the_date_field() {
        let e = google(r#"{"id":"d","summary":"Holiday","start":{"date":"2026-01-01"}}"#).unwrap();
        assert!(e.all_day);
        assert_eq!(e.end_ms - e.start_ms, 86_400_000);
    }

    #[test]
    fn a_deletion_arrives_as_a_bare_cancelled_row() {
        // This is the shape an incremental response uses for a deleted event:
        // an id, a status, and nothing else. Requiring a start time first
        // would drop it, and the event would never disappear locally.
        let e = google(r#"{"id":"gone","status":"cancelled"}"#).unwrap();
        assert!(e.cancelled);
        assert_eq!(e.uid, "gone");
    }

    #[test]
    fn recurrence_is_passed_through_not_expanded() {
        let e = google(
            r#"{"id":"r","start":{"dateTime":"2026-01-01T10:00:00Z"},
                "recurrence":["EXDATE;VALUE=DATE:20260108","RRULE:FREQ=WEEKLY"]}"#,
        )
        .unwrap();
        assert_eq!(e.rrule.as_deref(), Some("RRULE:FREQ=WEEKLY"));
    }

    #[test]
    fn an_event_without_an_id_is_unusable() {
        assert!(google(r#"{"summary":"no id","start":{"date":"2026-01-01"}}"#).is_none());
    }

    #[test]
    fn a_live_event_without_a_start_is_unusable() {
        assert!(google(r#"{"id":"x","summary":"no start"}"#).is_none());
    }

    #[test]
    fn a_missing_end_falls_back_to_the_start() {
        let e = google(r#"{"id":"p","start":{"dateTime":"2026-01-01T10:00:00Z"}}"#).unwrap();
        assert_eq!(e.start_ms, e.end_ms);
    }
}
