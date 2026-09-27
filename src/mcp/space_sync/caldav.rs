//! CalDAV, the protocol iCloud and most other calendar servers speak.
//!
//! Three requests, in the order RFC 6764 / RFC 4791 prescribe:
//!
//! 1. `PROPFIND /` for `current-user-principal` — who the credential is.
//! 2. `PROPFIND <principal>` for `calendar-home-set` — where their calendars live.
//! 3. `PROPFIND <home>` to list the collections, then `REPORT calendar-query`
//!    on each for the events in a time range.
//!
//! Hand-written over `reqwest` rather than a CalDAV crate: the useful ones are
//! either GPL (`minicaldav`) or pull a large dependency tree for three verbs
//! and a fixed set of properties. The XML here is read with a targeted
//! extractor, not a general parser, because the responses are machine-written
//! and the properties wanted are known — and every extraction is tested
//! against real iCloud response shapes.
//!
//! **Read and create only.** Editing a recurring series correctly means
//! rewriting RRULE/EXDATE across occurrences; getting that subtly wrong
//! silently corrupts someone's calendar, so it is not attempted.

use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use reqwest::Method;

use crate::db::Db;

use super::ical::parse_events;
use super::store::upsert_event;
use super::SyncReport;

pub const SOURCE: &str = "caldav";
/// iCloud's CalDAV entry point. Any other server works by passing its own.
pub const ICLOUD_BASE: &str = "https://caldav.icloud.com";
/// Stop after this many calendar collections in one run.
const MAX_CALENDARS: usize = 20;

/// Pull events from every calendar the credential can see.
///
/// `password` is an **app-specific password** on iCloud; the account password
/// is refused by the server when two-factor auth is on, which is always.
pub async fn sync(
    db: &Db,
    base_url: &str,
    username: &str,
    password: &str,
    days: u32,
) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(45))
        // iCloud answers discovery with a 301 to the user's own shard.
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .context("http client")?;

    let principal = match propfind_one(
        &http, base_url, base_url, username, password, PROP_PRINCIPAL, "0",
    )
    .await
    {
        Ok(Some(href)) => href,
        Ok(None) => {
            report.note_error("server returned no current-user-principal");
            return Ok(report);
        }
        Err(e) => return unauthorized_or(e, report),
    };

    let home = match propfind_one(
        &http,
        base_url,
        &absolute(base_url, &principal),
        username,
        password,
        PROP_HOME_SET,
        "0",
    )
    .await
    {
        Ok(Some(href)) => href,
        Ok(None) => {
            report.note_error("server returned no calendar-home-set");
            return Ok(report);
        }
        Err(e) => return unauthorized_or(e, report),
    };

    let home_url = absolute(base_url, &home);
    let body = match request(
        &http,
        Method::from_bytes(b"PROPFIND").expect("PROPFIND"),
        &home_url,
        username,
        password,
        PROP_CALENDARS,
        Some("1"),
    )
    .await
    {
        Ok(b) => b,
        Err(e) => return unauthorized_or(e, report),
    };

    let calendars = calendar_hrefs(&body);
    if calendars.is_empty() {
        report.note_error("no calendar collections found under the calendar home");
        return Ok(report);
    }

    let from = chrono::Utc::now() - chrono::Duration::days(days.min(3650) as i64);
    let to = chrono::Utc::now() + chrono::Duration::days(days.min(3650) as i64);
    let query = calendar_query_body(&from, &to);

    for href in calendars.into_iter().take(MAX_CALENDARS) {
        let url = absolute(base_url, &href);
        let body = match request(
            &http,
            Method::from_bytes(b"REPORT").expect("REPORT"),
            &url,
            username,
            password,
            &query,
            Some("1"),
        )
        .await
        {
            Ok(b) => b,
            Err(e) => {
                // One unreadable calendar must not lose the others.
                report.note_error(format!("calendar {href}: {e}"));
                continue;
            }
        };
        for ics in calendar_data(&body) {
            let events = parse_events(&ics);
            if events.is_empty() {
                report.note_error("a calendar object contained no readable VEVENT");
            }
            for e in events {
                upsert_event(db, SOURCE, &e, &mut report);
            }
        }
    }

    Ok(report)
}

/// Turn a transport error into a report, marking a rejected credential so the
/// caller stops retrying it.
fn unauthorized_or(e: anyhow::Error, mut report: SyncReport) -> Result<SyncReport> {
    let msg = e.to_string();
    if msg.contains("401") || msg.contains("403") {
        report.needs_reauth = true;
        report.note_error(
            "the server rejected the credential — on iCloud this must be an \
             app-specific password, not the account password",
        );
        return Ok(report);
    }
    Err(e)
}

const PROP_PRINCIPAL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:current-user-principal/></d:prop></d:propfind>"#;

const PROP_HOME_SET: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
<d:prop><c:calendar-home-set/></d:prop></d:propfind>"#;

const PROP_CALENDARS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:displayname/></d:prop></d:propfind>"#;

/// The `calendar-query` REPORT body for a time range.
fn calendar_query_body(
    from: &chrono::DateTime<chrono::Utc>,
    to: &chrono::DateTime<chrono::Utc>,
) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:getetag/><c:calendar-data/></d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VEVENT">
        <c:time-range start="{}" end="{}"/>
      </c:comp-filter>
    </c:comp-filter>
  </c:filter>
</c:calendar-query>"#,
        from.format("%Y%m%dT%H%M%SZ"),
        to.format("%Y%m%dT%H%M%SZ")
    )
}

async fn request(
    http: &reqwest::Client,
    method: Method,
    url: &str,
    username: &str,
    password: &str,
    body: &str,
    depth: Option<&str>,
) -> Result<String> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/xml; charset=utf-8"));
    if let Some(d) = depth {
        headers.insert("Depth", HeaderValue::from_str(d)?);
    }
    let resp = http
        .request(method, url)
        .basic_auth(username, Some(password))
        .headers(headers)
        .body(body.to_string())
        .send()
        .await
        .context("caldav request")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("caldav HTTP {status}");
    }
    Ok(text)
}

async fn propfind_one(
    http: &reqwest::Client,
    _base: &str,
    url: &str,
    username: &str,
    password: &str,
    body: &str,
    depth: &str,
) -> Result<Option<String>> {
    let xml = request(
        http,
        Method::from_bytes(b"PROPFIND").expect("PROPFIND"),
        url,
        username,
        password,
        body,
        Some(depth),
    )
    .await?;
    Ok(first_href_inside_prop(&xml))
}

// ── XML extraction ──────────────────────────────────────────────────────────
//
// Namespace prefixes are the server's choice (`d:`, `D:`, none at all), so
// everything matches on the local name.

/// The text of the first `<href>` that sits inside a property value.
///
/// A multistatus response opens with the `<href>` of the resource that was
/// asked about; the answer is the one *inside* `<prop>`. Taking the first
/// href in the document returns the request URL back, and discovery then
/// walks in a circle.
pub fn first_href_inside_prop(xml: &str) -> Option<String> {
    let prop_start = find_tag(xml, "prop", 0)?;
    let region = &xml[prop_start..];
    let href_start = find_tag(region, "href", 0)?;
    let after = &region[href_start..];
    let content_start = after.find('>')? + 1;
    let content_end = after[content_start..].find('<')? + content_start;
    let href = after[content_start..content_end].trim();
    if href.is_empty() {
        None
    } else {
        Some(href.to_string())
    }
}

/// Every `<href>` whose response is a calendar collection.
///
/// The calendar home itself appears in the same listing and is *not* a
/// calendar; so do iCloud's inbox and outbox collections, which answer
/// `calendar-query` with nothing useful.
pub fn calendar_hrefs(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    for block in split_responses(xml) {
        if !block.contains("calendar") {
            continue;
        }
        // `<C:calendar/>` in the resourcetype is what marks a collection as a
        // calendar. The literal check keeps `calendar-home-set` from matching.
        let is_calendar = block.contains(":calendar/>")
            || block.contains("<calendar/>")
            || block.contains(":calendar />")
            || block.contains("<calendar />");
        if !is_calendar {
            continue;
        }
        if block.contains("schedule-inbox") || block.contains("schedule-outbox") {
            continue;
        }
        if let Some(href) = first_href(block) {
            out.push(href);
        }
    }
    out
}

/// The iCalendar payloads carried in a `calendar-data` element.
pub fn calendar_data(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(start) = find_tag(xml, "calendar-data", pos) {
        let after = &xml[start..];
        let Some(open_end) = after.find('>') else { break };
        // A self-closing element carries nothing.
        if after[..open_end].ends_with('/') {
            pos = start + open_end + 1;
            continue;
        }
        let body_start = start + open_end + 1;
        let Some(close_rel) = find_closing(&xml[body_start..], "calendar-data") else {
            break;
        };
        let raw = &xml[body_start..body_start + close_rel];
        let decoded = decode_entities(raw);
        if decoded.contains("BEGIN:VEVENT") {
            out.push(decoded);
        }
        pos = body_start + close_rel;
    }
    out
}

/// Split a multistatus body into its `<response>` blocks.
fn split_responses(xml: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(start) = find_tag(xml, "response", pos) {
        let rest = &xml[start..];
        let Some(open_end) = rest.find('>') else { break };
        let body_start = start + open_end + 1;
        match find_closing(&xml[body_start..], "response") {
            Some(end) => {
                out.push(&xml[body_start..body_start + end]);
                pos = body_start + end;
            }
            None => break,
        }
    }
    out
}

/// Byte offset of the next opening tag with this local name.
fn find_tag(xml: &str, local: &str, from: usize) -> Option<usize> {
    let hay = xml.get(from..)?;
    let mut search = 0usize;
    loop {
        let rel = hay.get(search..)?.find('<')? + search;
        let after = hay.get(rel + 1..)?;
        // Skip closing tags and declarations.
        let name_part: String = after
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '>' && *c != '/')
            .collect();
        let name = name_part.rsplit(':').next().unwrap_or("");
        if name == local && !after.starts_with('/') {
            return Some(from + rel);
        }
        search = rel + 1;
    }
}

/// Offset of the matching closing tag, accounting for nesting.
fn find_closing(xml: &str, local: &str) -> Option<usize> {
    let mut depth = 1usize;
    let mut pos = 0usize;
    loop {
        let rel = xml.get(pos..)?.find('<')? + pos;
        let after = xml.get(rel + 1..)?;
        let closing = after.starts_with('/');
        let body = if closing { &after[1..] } else { after };
        let name_part: String = body
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '>' && *c != '/')
            .collect();
        let name = name_part.rsplit(':').next().unwrap_or("");
        if name == local {
            if closing {
                depth -= 1;
                if depth == 0 {
                    return Some(rel);
                }
            } else {
                let self_closing = after
                    .split_once('>')
                    .is_some_and(|(head, _)| head.trim_end().ends_with('/'));
                if !self_closing {
                    depth += 1;
                }
            }
        }
        pos = rel + 1;
    }
}

fn first_href(block: &str) -> Option<String> {
    let start = find_tag(block, "href", 0)?;
    let after = &block[start..];
    let content_start = after.find('>')? + 1;
    let content_end = after[content_start..].find('<')? + content_start;
    Some(after[content_start..content_end].trim().to_string())
}

/// The five predefined XML entities. `&amp;` is undone last so `&amp;lt;`
/// does not turn into `<`.
fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#13;", "\r")
        .replace("&amp;", "&")
}

/// Resolve an href against the base URL. Servers answer with a path, and
/// iCloud answers with an absolute URL on a different host than the one asked.
pub fn absolute(base: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    let base = base.trim_end_matches('/');
    // Keep only the scheme and host of the base for a root-relative href.
    let root = match base.split_once("://") {
        Some((scheme, rest)) => {
            let host = rest.split('/').next().unwrap_or(rest);
            format!("{scheme}://{host}")
        }
        None => base.to_string(),
    };
    if href.starts_with('/') {
        format!("{root}{href}")
    } else {
        format!("{base}/{href}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRINCIPAL_RESPONSE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:">
  <response>
    <href>/</href>
    <propstat>
      <prop><current-user-principal><href>/123456789/principal/</href></current-user-principal></prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
  </response>
</multistatus>"#;

    #[test]
    fn discovery_reads_the_href_inside_the_property() {
        // The response opens with the *requested* href ("/"). Taking the
        // document's first href walks discovery in a circle.
        assert_eq!(
            first_href_inside_prop(PRINCIPAL_RESPONSE).as_deref(),
            Some("/123456789/principal/")
        );
    }

    #[test]
    fn a_prefixed_namespace_reads_the_same() {
        let xml = r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/</D:href>
          <D:propstat><D:prop><D:current-user-principal><D:href>/p/</D:href>
          </D:current-user-principal></D:prop></D:propstat></D:response></D:multistatus>"#;
        assert_eq!(first_href_inside_prop(xml).as_deref(), Some("/p/"));
    }

    const HOME_LISTING: &str = r#"<multistatus xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <response>
    <href>/123/calendars/</href>
    <propstat><prop><resourcetype><collection/></resourcetype>
      <displayname>Home</displayname></prop></propstat>
  </response>
  <response>
    <href>/123/calendars/work/</href>
    <propstat><prop><resourcetype><collection/><C:calendar/></resourcetype>
      <displayname>Work</displayname></prop></propstat>
  </response>
  <response>
    <href>/123/calendars/inbox/</href>
    <propstat><prop><resourcetype><collection/><C:schedule-inbox/></resourcetype>
      </prop></propstat>
  </response>
</multistatus>"#;

    #[test]
    fn only_real_calendars_are_listed() {
        // The home collection itself and the scheduling inbox are not
        // calendars; querying them returns nothing and wastes a round trip.
        let hrefs = calendar_hrefs(HOME_LISTING);
        assert_eq!(hrefs, vec!["/123/calendars/work/".to_string()]);
    }

    #[test]
    fn calendar_data_is_entity_decoded_and_only_real_events_kept() {
        let xml = r#"<multistatus xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <response><href>/c/1.ics</href><propstat><prop>
    <C:calendar-data>BEGIN:VCALENDAR&#13;
BEGIN:VEVENT&#13;
UID:a&#13;
SUMMARY:Tea &amp; biscuits&#13;
DTSTART:20260101T100000Z&#13;
END:VEVENT&#13;
END:VCALENDAR</C:calendar-data>
  </prop></propstat></response>
  <response><href>/c/2.ics</href><propstat><prop>
    <C:calendar-data/>
  </prop></propstat></response>
</multistatus>"#;
        let data = calendar_data(xml);
        assert_eq!(data.len(), 1, "the empty element carries nothing");
        assert!(data[0].contains("Tea & biscuits"));
        let events = parse_events(&data[0]);
        assert_eq!(events[0].summary, "Tea & biscuits");
    }

    #[test]
    fn hrefs_resolve_against_the_right_root() {
        // iCloud answers discovery with an absolute URL on the user's own
        // shard; joining that onto the base would produce a dead URL.
        assert_eq!(
            absolute("https://caldav.icloud.com", "/123/principal/"),
            "https://caldav.icloud.com/123/principal/"
        );
        assert_eq!(
            absolute("https://caldav.icloud.com/123/", "https://p52.icloud.com/123/calendars/"),
            "https://p52.icloud.com/123/calendars/"
        );
        assert_eq!(
            absolute("https://example.test/dav/", "work/"),
            "https://example.test/dav/work/"
        );
    }

    #[test]
    fn the_time_range_body_is_well_formed() {
        let from = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let to = from + chrono::Duration::days(30);
        let body = calendar_query_body(&from, &to);
        assert!(body.contains(r#"start="20260101T000000Z""#));
        assert!(body.contains(r#"end="20260131T000000Z""#));
        assert!(body.contains("VEVENT"));
    }
}
