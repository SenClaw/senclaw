//! Reading a time out of a question: "hôm qua", "last week", "tháng 3",
//! "in 2025", "31/07" → the window `[start, end)` it names.
//!
//! Rule-based and bilingual (Vietnamese, English) on purpose. Deciding that a
//! query is about a time is a job for a few dozen patterns, not for a model:
//! this runs on every turn and a model call here would put an LLM on the
//! request path. Anything it does not recognise is simply "no time in this
//! question", and temporal recall stays silent.
//!
//! Windows are calendar days in the caller's timezone, expressed as UTC
//! midnights — the same convention [`super::extract::parse_when`] stores
//! facts in, so a "yesterday" window and a fact dated yesterday line up.

use std::sync::LazyLock;

use chrono::{Datelike, Duration, NaiveDate};
use regex::Regex;

use super::extract::midnight;
use crate::security::replication::fold;

macro_rules! re {
    ($name:ident, $pat:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).expect($pat));
    };
}

re!(ISO_DAY, r"\b(\d{4})-(\d{2})-(\d{2})\b");
re!(ISO_MONTH, r"\b(\d{4})-(\d{2})\b");
re!(DAY_MONTH, r"\b(\d{1,2})/(\d{1,2})(?:/(\d{4}))?\b");
re!(VI_MONTH, r"\bthang\s+(\d{1,2})(?:\s+(?:nam\s+)?(\d{4}))?\b");
re!(
    EN_MONTH,
    r"\b(?:in|during|of|since|from)\s+(january|february|march|april|may|june|july|august|september|october|november|december)(?:\s+(\d{4}))?\b"
);
re!(YEAR, r"\b(?:nam|in|during|of|since)\s+((?:19|20)\d{2})\b");
re!(DAYS_AGO, r"\b(\d{1,3})\s+(?:ngay\s+truoc|days?\s+ago)\b");
re!(
    PAST_DAYS,
    r"\b(?:(\d{1,3})\s+ngay\s+qua|(?:past|last)\s+(\d{1,3})\s+days)\b"
);

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

/// The window the question names, if it names one. `today` is the caller's
/// current date.
pub fn parse_window(query: &str, today: NaiveDate) -> Option<(i64, i64)> {
    let q = fold(query);
    let span = |start: NaiveDate, end: NaiveDate| Some((midnight(start), midnight(end)));
    let day = |d: NaiveDate| span(d, d.succ_opt()?);

    // Most specific first: an explicit date beats a relative phrase.
    if let Some(c) = ISO_DAY.captures(&q) {
        return day(ymd(num(&c, 1)? as i32, num(&c, 2)?, num(&c, 3)?)?);
    }
    if let Some(c) = ISO_MONTH.captures(&q) {
        return month(num(&c, 1)? as i32, num(&c, 2)?);
    }
    if let Some(c) = DAY_MONTH.captures(&q) {
        let (d, m) = (num(&c, 1)?, num(&c, 2)?);
        if (1..=12).contains(&m) && (1..=31).contains(&d) {
            let year = match num(&c, 3) {
                Some(y) => y as i32,
                None => latest_past_year(today, m, Some(d)),
            };
            return day(ymd(year, m, d)?);
        }
    }
    if let Some(c) = VI_MONTH.captures(&q) {
        let m = num(&c, 1)?;
        if (1..=12).contains(&m) {
            let year = num(&c, 2).map_or_else(|| latest_past_year(today, m, None), |y| y as i32);
            return month(year, m);
        }
    }
    if let Some(c) = EN_MONTH.captures(&q) {
        let m = MONTHS.iter().position(|n| *n == &c[1])? as u32 + 1;
        let year = num(&c, 2).map_or_else(|| latest_past_year(today, m, None), |y| y as i32);
        return month(year, m);
    }
    if let Some(c) = YEAR.captures(&q) {
        let y = num(&c, 1)? as i32;
        return span(ymd(y, 1, 1)?, ymd(y + 1, 1, 1)?);
    }
    if let Some(c) = DAYS_AGO.captures(&q) {
        return day(today - Duration::days(num(&c, 1)? as i64));
    }
    if let Some(c) = PAST_DAYS.captures(&q) {
        let n = num(&c, 1).or_else(|| num(&c, 2))? as i64;
        return span(today - Duration::days(n), today.succ_opt()?);
    }

    let has = |needles: &[&str]| needles.iter().any(|n| contains_phrase(&q, n));
    if has(&["hom qua", "yesterday"]) {
        return day(today.pred_opt()?);
    }
    if has(&["hom nay", "today"]) {
        return day(today);
    }
    if has(&["tuan truoc", "last week", "previous week"]) {
        let this_monday = today - Duration::days(today.weekday().num_days_from_monday() as i64);
        return span(this_monday - Duration::days(7), this_monday);
    }
    if has(&["tuan nay", "this week"]) {
        let this_monday = today - Duration::days(today.weekday().num_days_from_monday() as i64);
        return span(this_monday, this_monday + Duration::days(7));
    }
    if has(&["thang truoc", "last month", "previous month"]) {
        let first = ymd(today.year(), today.month(), 1)?;
        let prev = first.pred_opt()?;
        return span(ymd(prev.year(), prev.month(), 1)?, first);
    }
    if has(&["thang nay", "this month"]) {
        return month(today.year(), today.month());
    }
    if has(&["nam ngoai", "nam truoc", "last year", "previous year"]) {
        let y = today.year() - 1;
        return span(ymd(y, 1, 1)?, ymd(y + 1, 1, 1)?);
    }
    if has(&["nam nay", "this year"]) {
        let y = today.year();
        return span(ymd(y, 1, 1)?, ymd(y + 1, 1, 1)?);
    }
    None
}

/// `needle` as whole words in `haystack` — "today" must not fire inside
/// "todays", nor "hom nay" inside "nhom nay".
fn contains_phrase(haystack: &str, needle: &str) -> bool {
    haystack.match_indices(needle).any(|(at, _)| {
        let before = haystack[..at].chars().next_back();
        let after = haystack[at + needle.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

fn num(c: &regex::Captures<'_>, i: usize) -> Option<u32> {
    c.get(i)?.as_str().parse().ok()
}

fn ymd(y: i32, m: u32, d: u32) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(y, m, d)
}

fn month(y: i32, m: u32) -> Option<(i64, i64)> {
    let start = ymd(y, m, 1)?;
    let end = if m == 12 {
        ymd(y + 1, 1, 1)?
    } else {
        ymd(y, m + 1, 1)?
    };
    Some((midnight(start), midnight(end)))
}

/// A question about "March" with no year means the most recent March that has
/// begun: memory is about the past, so a month later in the year than today
/// is last year's.
fn latest_past_year(today: NaiveDate, month: u32, day: Option<u32>) -> i32 {
    let this_year = today.year();
    let candidate = ymd(this_year, month, day.unwrap_or(1));
    match candidate {
        Some(c) if c <= today => this_year,
        _ => this_year - 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A Friday.
    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 16).unwrap()
    }

    fn win(q: &str) -> Option<(NaiveDate, NaiveDate)> {
        parse_window(q, today()).map(|(s, e)| {
            let d = |t: i64| chrono::DateTime::from_timestamp(t, 0).unwrap().date_naive();
            (d(s), d(e))
        })
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn single_days_in_both_languages() {
        assert_eq!(
            win("Hôm qua tôi đã làm gì?"),
            Some((d(2026, 10, 15), d(2026, 10, 16)))
        );
        assert_eq!(
            win("what did I do yesterday"),
            Some((d(2026, 10, 15), d(2026, 10, 16)))
        );
        assert_eq!(
            win("hôm nay có gì"),
            Some((d(2026, 10, 16), d(2026, 10, 17)))
        );
        assert_eq!(
            win("anything today?"),
            Some((d(2026, 10, 16), d(2026, 10, 17)))
        );
        assert_eq!(
            win("3 ngày trước ai nhắn tin"),
            Some((d(2026, 10, 13), d(2026, 10, 14)))
        );
        assert_eq!(
            win("what happened 3 days ago"),
            Some((d(2026, 10, 13), d(2026, 10, 14)))
        );
    }

    #[test]
    fn weeks_run_monday_to_monday() {
        assert_eq!(
            win("tuần trước tôi bàn gì"),
            Some((d(2026, 10, 5), d(2026, 10, 12)))
        );
        assert_eq!(
            win("last week's decisions"),
            Some((d(2026, 10, 5), d(2026, 10, 12)))
        );
        assert_eq!(
            win("tuần này có việc gì"),
            Some((d(2026, 10, 12), d(2026, 10, 19)))
        );
    }

    #[test]
    fn months_and_years_relative_to_today() {
        assert_eq!(
            win("tháng trước chi tiêu bao nhiêu"),
            Some((d(2026, 9, 1), d(2026, 10, 1)))
        );
        assert_eq!(
            win("this month plans"),
            Some((d(2026, 10, 1), d(2026, 11, 1)))
        );
        assert_eq!(
            win("năm ngoái tôi ở đâu"),
            Some((d(2025, 1, 1), d(2026, 1, 1)))
        );
        assert_eq!(
            win("what did we ship this year"),
            Some((d(2026, 1, 1), d(2027, 1, 1)))
        );
        // January looks back across the year boundary.
        let jan = NaiveDate::from_ymd_opt(2026, 1, 10).unwrap();
        let (s, e) = parse_window("last month", jan).unwrap();
        assert_eq!((s, e), (midnight(d(2025, 12, 1)), midnight(d(2026, 1, 1))));
    }

    #[test]
    fn a_trailing_span_of_days() {
        assert_eq!(
            win("7 ngày qua có gì mới"),
            Some((d(2026, 10, 9), d(2026, 10, 17)))
        );
        assert_eq!(
            win("anything in the past 14 days"),
            Some((d(2026, 10, 2), d(2026, 10, 17)))
        );
        assert_eq!(win("last 3 days"), Some((d(2026, 10, 13), d(2026, 10, 17))));
    }

    #[test]
    fn explicit_dates_months_and_years() {
        assert_eq!(
            win("họp ngày 2026-07-31"),
            Some((d(2026, 7, 31), d(2026, 8, 1)))
        );
        assert_eq!(win("giá vàng 31/07"), Some((d(2026, 7, 31), d(2026, 8, 1))));
        assert_eq!(
            win("giá vàng 31/07/2025"),
            Some((d(2025, 7, 31), d(2025, 8, 1)))
        );
        assert_eq!(
            win("kế hoạch 2026-03"),
            Some((d(2026, 3, 1), d(2026, 4, 1)))
        );
        assert_eq!(
            win("Tháng 3 tôi làm gì"),
            Some((d(2026, 3, 1), d(2026, 4, 1)))
        );
        assert_eq!(
            win("tháng 3 năm 2024"),
            Some((d(2024, 3, 1), d(2024, 4, 1)))
        );
        assert_eq!(
            win("what happened in March"),
            Some((d(2026, 3, 1), d(2026, 4, 1)))
        );
        assert_eq!(
            win("during June 2025"),
            Some((d(2025, 6, 1), d(2025, 7, 1)))
        );
        assert_eq!(
            win("trong năm 2025 có gì"),
            Some((d(2025, 1, 1), d(2026, 1, 1)))
        );
        assert_eq!(
            win("what changed in 2024?"),
            Some((d(2024, 1, 1), d(2025, 1, 1)))
        );
    }

    #[test]
    fn a_month_that_has_not_come_yet_means_last_year() {
        // Today is October: "tháng 12" is last December, "tháng 10" is now.
        assert_eq!(
            win("tháng 12 tôi đi đâu"),
            Some((d(2025, 12, 1), d(2026, 1, 1)))
        );
        assert_eq!(
            win("tháng 10 có gì"),
            Some((d(2026, 10, 1), d(2026, 11, 1)))
        );
        assert_eq!(win("ngày 25/12"), Some((d(2025, 12, 25), d(2025, 12, 26))));
    }

    #[test]
    fn a_specific_date_beats_a_relative_phrase() {
        assert_eq!(
            win("hôm qua hay ngày 2026-07-31?"),
            Some((d(2026, 7, 31), d(2026, 8, 1)))
        );
    }

    #[test]
    fn questions_without_a_time_name_no_window() {
        for q in [
            "Which ports does the daemon use?",
            "Tôi thích cổng nào cho app thử nghiệm?",
            "the may flower",              // "may" without a preposition is a word
            "bao nhiêu nhóm này",          // "nhom nay" must not be "hom nay"
            "port 2049 is NFS",            // a bare number is not a year
            "version 31/45 of the report", // not a date
            "",
        ] {
            assert_eq!(win(q), None, "{q}");
        }
    }

    #[test]
    fn impossible_dates_name_no_window() {
        assert_eq!(win("ngày 31/02"), None);
        assert_eq!(win("tháng 13"), None);
        assert_eq!(win("2026-02-30"), None);
    }
}
