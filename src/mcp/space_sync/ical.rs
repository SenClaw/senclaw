//! Just enough iCalendar to read a VEVENT.
//!
//! CalDAV answers with whole `VCALENDAR` documents, and the events inside
//! them are what a calendar view needs. This reads the handful of properties
//! that map onto `space_events` and keeps the rest verbatim.
//!
//! Deliberately not a general iCalendar library: the parts that matter here
//! are line unfolding (RFC 5545 §3.1, which a naive line split silently
//! corrupts), the three shapes a date-time comes in, and the escaping rules
//! for text values. Recurrence is **kept as its original `RRULE` string** and
//! never expanded — expanding it would invent occurrences the server did not
//! send, and the value round-trips unchanged if it is ever written back.

use chrono::{NaiveDate, NaiveDateTime, TimeZone, Utc};

/// One event, in the terms `space_events` stores.
#[derive(Debug, Clone, PartialEq)]
pub struct VEvent {
    /// The server's own id for the event; the key an update is matched on.
    pub uid: String,
    pub summary: String,
    pub description: Option<String>,
    pub location: Option<String>,
    /// Unix milliseconds, UTC.
    pub start_ms: i64,
    pub end_ms: i64,
    pub all_day: bool,
    /// The `RRULE` line verbatim, or `None`.
    pub rrule: Option<String>,
    /// `CANCELLED` events are deletions in disguise.
    pub cancelled: bool,
}

/// Undo RFC 5545 line folding: a CRLF followed by a space or tab continues
/// the previous line. Splitting on newlines without this cuts long summaries
/// and URLs in half, which looks like corrupt data from the server.
pub fn unfold(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in raw.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(last) = out.last_mut() {
                last.push_str(&line[1..]);
                continue;
            }
        }
        out.push(line.to_string());
    }
    out
}

/// Split `DTSTART;TZID=Europe/Paris:20260101T090000` into
/// `("DTSTART", ["TZID=Europe/Paris"], "20260101T090000")`.
fn split_line(line: &str) -> Option<(String, Vec<String>, String)> {
    let colon = line.find(':')?;
    let (head, value) = line.split_at(colon);
    let value = &value[1..];
    let mut parts = head.split(';');
    let name = parts.next()?.to_ascii_uppercase();
    let params: Vec<String> = parts.map(str::to_string).collect();
    Some((name, params, value.to_string()))
}

/// Undo the text escaping of RFC 5545 §3.3.11.
fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') | Some('N') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some(',') => out.push(','),
            Some(';') => out.push(';'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Parse the three date-time shapes into UTC milliseconds, plus whether the
/// value was a date (an all-day event).
///
/// A floating local time (no `Z`, no `TZID`) is read as UTC. That is the
/// documented compromise: without the originating time zone there is no
/// correct answer, and a silently shifted event is worse than one that is
/// consistently placed.
pub fn parse_datetime(value: &str, params: &[String]) -> Option<(i64, bool)> {
    let is_date = params.iter().any(|p| p.eq_ignore_ascii_case("VALUE=DATE"))
        || (value.len() == 8 && !value.contains('T'));
    if is_date {
        let d = NaiveDate::parse_from_str(value, "%Y%m%d").ok()?;
        let dt = d.and_hms_opt(0, 0, 0)?;
        return Some((Utc.from_utc_datetime(&dt).timestamp_millis(), true));
    }
    let trimmed = value.trim_end_matches('Z');
    let dt = NaiveDateTime::parse_from_str(trimmed, "%Y%m%dT%H%M%S").ok()?;
    Some((Utc.from_utc_datetime(&dt).timestamp_millis(), false))
}

/// Read every `VEVENT` in a `VCALENDAR` document.
///
/// Anything that is not a VEVENT — VTODO, VTIMEZONE, VALARM — is skipped
/// rather than misread; an alarm's own `TRIGGER` must not become the event's
/// start, which is exactly what a flat property scan would do.
pub fn parse_events(raw: &str) -> Vec<VEvent> {
    let mut events = Vec::new();
    let mut current: Option<PartialEvent> = None;
    let mut depth_other = 0usize;

    for line in unfold(raw) {
        let upper = line.trim().to_ascii_uppercase();
        if upper == "BEGIN:VEVENT" {
            current = Some(PartialEvent::default());
            continue;
        }
        if upper == "END:VEVENT" {
            if let Some(p) = current.take() {
                if let Some(e) = p.finish() {
                    events.push(e);
                }
            }
            continue;
        }
        // A nested component (VALARM inside VEVENT, VTIMEZONE beside it)
        // carries its own DTSTART; ignore everything until it closes.
        if upper.starts_with("BEGIN:") {
            if current.is_some() {
                depth_other += 1;
            }
            continue;
        }
        if upper.starts_with("END:") {
            depth_other = depth_other.saturating_sub(1);
            continue;
        }
        if depth_other > 0 {
            continue;
        }
        let Some(p) = current.as_mut() else { continue };
        let Some((name, params, value)) = split_line(&line) else {
            continue;
        };
        match name.as_str() {
            "UID" => p.uid = Some(value),
            "SUMMARY" => p.summary = Some(unescape(&value)),
            "DESCRIPTION" => p.description = Some(unescape(&value)),
            "LOCATION" => p.location = Some(unescape(&value)),
            "DTSTART" => p.start = parse_datetime(&value, &params),
            "DTEND" => p.end = parse_datetime(&value, &params),
            "DURATION" => p.duration_secs = parse_duration(&value),
            "RRULE" => p.rrule = Some(format!("RRULE:{value}")),
            "STATUS" => p.cancelled = value.eq_ignore_ascii_case("CANCELLED"),
            _ => {}
        }
    }
    events
}

/// RFC 5545 durations, the subset an event uses: `PT1H30M`, `P1D`, `PT45M`.
fn parse_duration(v: &str) -> Option<i64> {
    let v = v.trim().trim_start_matches('+');
    let negative = v.starts_with('-');
    let v = v.trim_start_matches('-');
    let rest = v.strip_prefix('P')?;
    let (date_part, time_part) = match rest.split_once('T') {
        Some((d, t)) => (d, t),
        None => (rest, ""),
    };
    let mut total = 0i64;
    let mut num = String::new();
    for c in date_part.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            let n: i64 = num.parse().ok()?;
            num.clear();
            total += match c {
                'W' => n * 7 * 86_400,
                'D' => n * 86_400,
                _ => return None,
            };
        }
    }
    for c in time_part.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else {
            let n: i64 = num.parse().ok()?;
            num.clear();
            total += match c {
                'H' => n * 3_600,
                'M' => n * 60,
                'S' => n,
                _ => return None,
            };
        }
    }
    Some(if negative { -total } else { total })
}

#[derive(Default)]
struct PartialEvent {
    uid: Option<String>,
    summary: Option<String>,
    description: Option<String>,
    location: Option<String>,
    start: Option<(i64, bool)>,
    end: Option<(i64, bool)>,
    duration_secs: Option<i64>,
    rrule: Option<String>,
    cancelled: bool,
}

impl PartialEvent {
    fn finish(self) -> Option<VEvent> {
        // No uid means nothing can be matched on a later sync, and no start
        // means nothing can be placed; both are unusable rather than partial.
        let uid = self.uid?;
        let (start_ms, all_day) = self.start?;
        let end_ms = match (self.end, self.duration_secs) {
            (Some((e, _)), _) => e,
            (None, Some(secs)) => start_ms + secs * 1000,
            // RFC 5545: a date-only event with no end lasts one day; a
            // date-time one is instantaneous.
            (None, None) if all_day => start_ms + 86_400_000,
            (None, None) => start_ms,
        };
        Some(VEvent {
            uid,
            summary: self.summary.unwrap_or_else(|| "(no title)".to_string()),
            description: self.description,
            location: self.location,
            start_ms,
            end_ms,
            all_day,
            rrule: self.rrule,
            cancelled: self.cancelled,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfolds_continuation_lines() {
        let raw = "SUMMARY:A very long\r\n  title that was folded\r\nUID:1";
        let lines = unfold(raw);
        assert_eq!(lines[0], "SUMMARY:A very long title that was folded");
        assert_eq!(lines[1], "UID:1");
    }

    #[test]
    fn parses_a_timed_event() {
        let raw = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:abc\r\n\
                   SUMMARY:Standup\r\nDTSTART:20260912T090000Z\r\nDTEND:20260912T093000Z\r\n\
                   LOCATION:Room 2\r\nEND:VEVENT\r\nEND:VCALENDAR";
        let events = parse_events(raw);
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.uid, "abc");
        assert_eq!(e.summary, "Standup");
        assert_eq!(e.location.as_deref(), Some("Room 2"));
        assert_eq!(e.end_ms - e.start_ms, 30 * 60 * 1000);
        assert!(!e.all_day);
    }

    #[test]
    fn an_all_day_event_lasts_a_day() {
        let raw = "BEGIN:VEVENT\r\nUID:d\r\nSUMMARY:Holiday\r\n\
                   DTSTART;VALUE=DATE:20260101\r\nEND:VEVENT";
        let e = &parse_events(raw)[0];
        assert!(e.all_day);
        assert_eq!(e.end_ms - e.start_ms, 86_400_000);
    }

    #[test]
    fn duration_stands_in_for_a_missing_end() {
        let raw = "BEGIN:VEVENT\r\nUID:x\r\nDTSTART:20260101T100000Z\r\n\
                   DURATION:PT1H30M\r\nEND:VEVENT";
        let e = &parse_events(raw)[0];
        assert_eq!(e.end_ms - e.start_ms, 90 * 60 * 1000);
    }

    #[test]
    fn an_alarms_own_trigger_is_not_the_events_start() {
        // A flat property scan reads the VALARM's fields as the event's.
        let raw = "BEGIN:VEVENT\r\nUID:a\r\nSUMMARY:Real\r\nDTSTART:20260101T100000Z\r\n\
                   BEGIN:VALARM\r\nTRIGGER:-PT15M\r\nSUMMARY:Reminder\r\n\
                   DTSTART:20250101T000000Z\r\nEND:VALARM\r\nEND:VEVENT";
        let e = &parse_events(raw)[0];
        assert_eq!(e.summary, "Real");
        let (expected, _) = parse_datetime("20260101T100000Z", &[]).unwrap();
        assert_eq!(e.start_ms, expected);
    }

    #[test]
    fn recurrence_is_kept_verbatim_never_expanded() {
        let raw = "BEGIN:VEVENT\r\nUID:r\r\nDTSTART:20260101T100000Z\r\n\
                   RRULE:FREQ=WEEKLY;BYDAY=MO,WE;COUNT=10\r\nEND:VEVENT";
        let events = parse_events(raw);
        assert_eq!(events.len(), 1, "one stored event, not ten occurrences");
        assert_eq!(
            events[0].rrule.as_deref(),
            Some("RRULE:FREQ=WEEKLY;BYDAY=MO,WE;COUNT=10")
        );
    }

    #[test]
    fn text_escapes_are_undone() {
        let raw = "BEGIN:VEVENT\r\nUID:e\r\nDTSTART:20260101T100000Z\r\n\
                   DESCRIPTION:line one\\nline two\\, with a comma\r\nEND:VEVENT";
        let e = &parse_events(raw)[0];
        assert_eq!(
            e.description.as_deref(),
            Some("line one\nline two, with a comma")
        );
    }

    #[test]
    fn an_event_without_uid_or_start_is_dropped() {
        assert!(parse_events("BEGIN:VEVENT\r\nSUMMARY:no uid\r\nEND:VEVENT").is_empty());
        assert!(parse_events("BEGIN:VEVENT\r\nUID:u\r\nEND:VEVENT").is_empty());
    }

    #[test]
    fn a_cancelled_event_is_marked() {
        let raw = "BEGIN:VEVENT\r\nUID:c\r\nDTSTART:20260101T100000Z\r\n\
                   STATUS:CANCELLED\r\nEND:VEVENT";
        assert!(parse_events(raw)[0].cancelled);
    }
}
