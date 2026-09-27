//! Local-time formatting helpers.
//!
//! Mirrors `src-old/util/localTime.ts`. `chrono::Local` is used because we
//! want host-local time for log filenames and display timestamps. DB storage
//! and time comparisons should still use UTC (`chrono::Utc`).

use chrono::{DateTime, Datelike, Local, Timelike};

/// Local date as `YYYY-MM-DD`.
pub fn local_date_string(d: DateTime<Local>) -> String {
    format!("{:04}-{:02}-{:02}", d.year(), d.month(), d.day())
}

/// Local time as `HH:MM`.
pub fn local_time_string(d: DateTime<Local>) -> String {
    format!("{:02}:{:02}", d.hour(), d.minute())
}

/// Local timestamp as `YYYY-MM-DD HH:MM:SS` (display only).
pub fn local_iso_string(d: DateTime<Local>) -> String {
    format!(
        "{} {:02}:{:02}:{:02}",
        local_date_string(d),
        d.hour(),
        d.minute(),
        d.second(),
    )
}

/// Convenience wrappers using `Local::now()` for the common no-arg call site.
pub fn local_date_string_now() -> String {
    local_date_string(Local::now())
}
pub fn local_time_string_now() -> String {
    local_time_string(Local::now())
}
pub fn local_iso_string_now() -> String {
    local_iso_string(Local::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fixed() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 4, 28, 9, 5, 7).unwrap()
    }

    #[test]
    fn formats_date() {
        assert_eq!(local_date_string(fixed()), "2026-04-28");
    }

    #[test]
    fn formats_time() {
        assert_eq!(local_time_string(fixed()), "09:05");
    }

    #[test]
    fn formats_iso() {
        assert_eq!(local_iso_string(fixed()), "2026-04-28 09:05:07");
    }
}
