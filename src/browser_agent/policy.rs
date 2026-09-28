//! Code-only policy for browser actions: risk tiers, privacy, and which
//! browser / decision backend a task may use. No model can lower a tier.

use serde::Serialize;
use serde_json::Value;

use super::budget::fold;
use super::encoder::Profile;
use super::settings::{BrowserSettings, DecisionBackend, Driver};
use crate::decision::types::Backend;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Scroll, wait, open a filter or link, type into a search box.
    Auto,
    /// Ordinary form input and choices: done, and shown in the step list.
    Logged,
    /// Purchase, payment, send, post, delete, confirm…: waits for the person.
    Approve,
    /// Credentials, one-time codes, card data: the person does it (handover).
    Human,
}

/// Words that make a click consequential, English and Vietnamese (diacritics
/// folded). Matched on word boundaries so "Sendai" is not "send".
const RISKY: &[&str] = &[
    "buy", "purchase", "pay", "payment", "checkout", "check out", "place order", "order now", "book now",
    "reserve", "confirm", "submit order", "send", "post", "publish", "tweet", "reply", "delete", "remove",
    "cancel subscription", "unsubscribe", "transfer", "donate", "subscribe", "sign up", "register", "withdraw",
    "mua", "mua ngay", "thanh toan", "dat hang", "dat ngay", "dat ve", "dat phong", "xac nhan", "gui", "dang bai",
    "dang tin", "xoa", "huy", "chuyen khoan", "chuyen tien", "dang ky", "nap tien", "rut tien", "ung ho",
];

const SEARCH_WORDS: &[&str] = &["search", "query", "find", "tim", "tim kiem", "tra cuu"];

fn has_word(haystack: &str, word: &str) -> bool {
    let hay = fold(haystack);
    let mut start = 0;
    while let Some(pos) = hay[start..].find(word) {
        let i = start + pos;
        let before = hay[..i].chars().next_back();
        let after = hay[i + word.len()..].chars().next();
        let boundary = |c: Option<char>| c.map(|c| !c.is_alphanumeric()).unwrap_or(true);
        if boundary(before) && boundary(after) {
            return true;
        }
        start = i + word.len();
    }
    false
}

fn any_word(text: &str, words: &'static [&'static str]) -> Option<&'static str> {
    words.iter().copied().find(|w| has_word(text, w))
}

/// The tier of executing `operation` on `action`, with the reason shown to the person.
pub fn risk_tier(operation: &str, action: &Value, dialog_type: Option<&str>) -> (Tier, String) {
    let label = action.get("label").and_then(Value::as_str).unwrap_or_default();
    let role = action.get("role").and_then(Value::as_str).unwrap_or_default();
    match operation {
        "TYPE_TEXT" => {
            if action.get("sensitive").and_then(Value::as_bool) == Some(true) {
                return (Tier::Human, format!("\"{label}\" looks like a credential or one-time code"));
            }
            if matches!(role, "searchbox" | "combobox") || any_word(label, SEARCH_WORDS).is_some() {
                (Tier::Auto, "search input".into())
            } else {
                (Tier::Logged, "form input".into())
            }
        }
        "CLICK" => match any_word(label, RISKY) {
            Some(word) => (Tier::Approve, format!("\"{label}\" may be irreversible ({word})")),
            None => (Tier::Auto, String::new()),
        },
        "SELECT" => (Tier::Logged, "dropdown choice".into()),
        "KEY_ENTER" => {
            if any_word(label, SEARCH_WORDS).is_some() {
                (Tier::Auto, "submit a search".into())
            } else {
                (Tier::Logged, "submit a field".into())
            }
        }
        "DIALOG_ACCEPT" => match dialog_type {
            Some("alert") | None => (Tier::Auto, String::new()),
            Some(kind) => (Tier::Approve, format!("accept a {kind} dialog")),
        },
        _ => (Tier::Auto, String::new()),
    }
}

/// Mask personal data before page text leaves the machine (hosted backend):
/// e-mail addresses, phone numbers, and any run of 9+ digits (cards, IDs,
/// account numbers). Prices, dates and short counts survive.
pub fn redact_pii(text: &str) -> String {
    use std::sync::LazyLock;
    static EMAIL: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}").expect("email regex"));
    static DIGITS: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\+?\d(?:[ .-]?\d){8,}").expect("digits regex"));
    let text = EMAIL.replace_all(text, "[email]");
    DIGITS
        .replace_all(&text, |caps: &regex::Captures| {
            let m = &caps[0];
            let digits: String = m.chars().filter(|c| c.is_ascii_digit()).collect();
            let phone = m.starts_with("+84") || (digits.starts_with('0') && (10..=11).contains(&digits.len()));
            if phone { "[phone]" } else { "[number]" }.to_string()
        })
        .into_owned()
}

pub fn host_of(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    rest.split(['/', '?', '#']).next().unwrap_or_default().split('@').last().unwrap_or_default().split(':').next().unwrap_or_default().to_ascii_lowercase()
}

fn matches_domain(host: &str, domain: &str) -> bool {
    let d = domain.trim().trim_start_matches("*.").to_ascii_lowercase();
    !d.is_empty() && (host == d || host.ends_with(&format!(".{d}")))
}

/// Mail, banking, payment, government, identity and local-network pages.
pub fn is_sensitive_host(host: &str, extra: &[String]) -> bool {
    const DOMAINS: &[&str] = &[
        "mail.google.com", "accounts.google.com", "outlook.live.com", "outlook.office.com", "mail.yahoo.com",
        "paypal.com", "momo.vn", "zalopay.vn", "vnpay.vn", "vietcombank.com.vn", "techcombank.com.vn",
        "bidv.com.vn", "vietinbank.vn", "agribank.com.vn", "mbbank.com.vn", "tpb.vn", "acb.com.vn",
        "vpbank.com.vn", "sacombank.com.vn", "dichvucong.gov.vn",
    ];
    if host == "localhost" || host.ends_with(".local") || host.ends_with(".internal") || host.starts_with("127.") || host == "[::1]" {
        return true;
    }
    if host.starts_with("10.") || host.starts_with("192.168.") || (host.starts_with("172.") && host.split('.').nth(1).and_then(|o| o.parse::<u8>().ok()).map(|o| (16..=31).contains(&o)).unwrap_or(false)) {
        return true;
    }
    if host.ends_with(".gov") || host.ends_with(".gov.vn") || host.contains("bank") {
        return true;
    }
    DOMAINS.iter().any(|d| matches_domain(host, d)) || extra.iter().any(|d| matches_domain(host, d))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverError {
    ExtensionNotConnected,
}

/// Which browser a task runs in (design §9.1).
pub fn select_driver(
    requested: Option<Driver>,
    url: Option<&str>,
    settings: &BrowserSettings,
    extension_connected: bool,
) -> Result<Driver, DriverError> {
    let chosen = requested.or_else(|| {
        let host = url.map(host_of)?;
        settings.domain_drivers.iter().find(|(d, _)| matches_domain(&host, d)).map(|(_, driver)| *driver)
    });
    match chosen.unwrap_or(settings.default_driver) {
        // A person's sign-ins do not exist in the managed profile: never
        // silently switch browsers.
        Driver::Extension if !extension_connected => Err(DriverError::ExtensionNotConnected),
        driver => Ok(driver),
    }
}

/// How one step's decision is made.
#[derive(Debug, Clone, PartialEq)]
pub enum DecisionRoute {
    Model { profile: Profile, backend: Backend, model: Option<String>, redact: bool },
    LlmOnly,
}

pub fn select_backend(settings: &BrowserSettings, url: &str, driver: Driver) -> DecisionRoute {
    let host = host_of(url);
    let local = DecisionRoute::Model {
        profile: Profile::LayaV3,
        backend: Backend::Local,
        model: Some(settings.local_model.clone()),
        redact: false,
    };
    let hosted = DecisionRoute::Model {
        profile: Profile::JevFull,
        backend: Backend::Online,
        model: settings.hosted_model.clone(),
        redact: true,
    };
    let sensitive = driver == Driver::Extension || is_sensitive_host(&host, &settings.sensitive_domains);
    match settings.decision_backend {
        DecisionBackend::LlmOnly => DecisionRoute::LlmOnly,
        DecisionBackend::Local => local,
        DecisionBackend::Hosted if sensitive => local,
        DecisionBackend::Hosted => hosted,
        DecisionBackend::Auto if !sensitive && settings.hosted_domains.iter().any(|d| matches_domain(&host, d)) => hosted,
        DecisionBackend::Auto => local,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn click(label: &str) -> Value {
        json!({"kind": "click", "role": "button", "label": label})
    }

    #[test]
    fn risk_tiers() {
        assert_eq!(risk_tier("CLICK", &click("Place order"), None).0, Tier::Approve);
        assert_eq!(risk_tier("CLICK", &click("Thanh toán"), None).0, Tier::Approve);
        assert_eq!(risk_tier("CLICK", &click("Xóa bài viết"), None).0, Tier::Approve);
        assert_eq!(risk_tier("CLICK", &click("Đặt vé ngay"), None).0, Tier::Approve);
        assert_eq!(risk_tier("CLICK", &click("Send"), None).0, Tier::Approve);
        assert_eq!(risk_tier("CLICK", &click("Sendai hotels"), None).0, Tier::Auto, "word boundary");
        assert_eq!(risk_tier("CLICK", &click("Search"), None).0, Tier::Auto);
        assert_eq!(risk_tier("CLICK", &click("Free cancellation"), None).0, Tier::Auto);
        let otp = json!({"kind": "fill", "role": "textbox", "label": "One-time code", "sensitive": true});
        assert_eq!(risk_tier("TYPE_TEXT", &otp, None).0, Tier::Human);
        let dest = json!({"kind": "fill", "role": "textbox", "label": "Destination"});
        assert_eq!(risk_tier("TYPE_TEXT", &dest, None).0, Tier::Logged);
        let search = json!({"kind": "fill", "role": "searchbox", "label": "Tìm kiếm"});
        assert_eq!(risk_tier("TYPE_TEXT", &search, None).0, Tier::Auto);
        assert_eq!(risk_tier("DIALOG_ACCEPT", &json!({}), Some("confirm")).0, Tier::Approve);
        assert_eq!(risk_tier("DIALOG_ACCEPT", &json!({}), Some("alert")).0, Tier::Auto);
        assert_eq!(risk_tier("SCROLL_DOWN", &json!({}), None).0, Tier::Auto);
    }

    #[test]
    fn pii_is_redacted_for_hosted() {
        let text = "Liên hệ an.nguyen@example.com hoặc 0912 345 678 / +84912345678. Thẻ 4111 1111 1111 1111, CCCD 001203004567.";
        let masked = redact_pii(text);
        assert!(!masked.contains("an.nguyen@example.com") && masked.contains("[email]"), "{masked}");
        assert!(!masked.contains("0912 345 678") && masked.contains("[phone]"), "{masked}");
        assert!(!masked.contains("4111 1111 1111 1111"), "{masked}");
        assert!(!masked.contains("001203004567"), "{masked}");
        let ordinary = "Giá 216 USD, ngày 20/09/2026, còn 12 chỗ.";
        assert_eq!(redact_pii(ordinary), ordinary);
    }

    #[test]
    fn driver_selection() {
        let mut s = BrowserSettings::default();
        assert_eq!(select_driver(None, Some("https://news.ycombinator.com/"), &s, false), Ok(Driver::Managed));
        assert_eq!(select_driver(Some(Driver::Extension), None, &s, false), Err(DriverError::ExtensionNotConnected));
        assert_eq!(select_driver(Some(Driver::Extension), None, &s, true), Ok(Driver::Extension));
        assert_eq!(select_driver(Some(Driver::Managed), None, &s, true), Ok(Driver::Managed));
        s.domain_drivers.insert("mail.google.com".into(), Driver::Extension);
        assert_eq!(select_driver(None, Some("https://mail.google.com/mail/u/0/"), &s, true), Ok(Driver::Extension));
        assert_eq!(select_driver(None, Some("https://mail.google.com/"), &s, false), Err(DriverError::ExtensionNotConnected));
        s.default_driver = Driver::Extension;
        assert_eq!(select_driver(None, None, &s, true), Ok(Driver::Extension));

        // Backend: local unless the domain is allow-listed for hosted and not sensitive.
        let mut s = BrowserSettings::default();
        assert!(matches!(select_backend(&s, "https://example.com/", Driver::Managed), DecisionRoute::Model { backend: Backend::Local, .. }));
        s.hosted_domains = vec!["example.com".into(), "vietcombank.com.vn".into()];
        assert!(matches!(select_backend(&s, "https://www.example.com/a", Driver::Managed), DecisionRoute::Model { backend: Backend::Online, redact: true, .. }));
        assert!(matches!(select_backend(&s, "https://www.example.com/a", Driver::Extension), DecisionRoute::Model { backend: Backend::Local, .. }), "the person's Chrome stays local");
        assert!(matches!(select_backend(&s, "https://vietcombank.com.vn/", Driver::Managed), DecisionRoute::Model { backend: Backend::Local, .. }), "sensitive stays local");
        s.decision_backend = DecisionBackend::LlmOnly;
        assert_eq!(select_backend(&s, "https://example.com/", Driver::Managed), DecisionRoute::LlmOnly);
        assert_eq!(host_of("https://user@Sub.Example.com:8443/path?q=1"), "sub.example.com");
    }
}
