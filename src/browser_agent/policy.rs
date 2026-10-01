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

/// Words that make an action consequential, English and Vietnamese
/// (diacritics folded). Matched as whole words, a phrase's words in order with
/// up to two words between them: "Place your order" is "place order", while
/// "Sendai" is not "send" and "Books" is not "book". A false alarm costs one
/// confirmation; a miss is a purchase nobody approved.
const RISKY: &[&str] = &[
    "buy", "purchase", "pay", "payment", "checkout", "check out", "place order", "complete order", "confirm order",
    "order now", "submit", "book", "booking", "reserve", "confirm", "send", "post", "publish", "tweet", "reply",
    "post comment", "add comment", "share", "delete", "remove", "trash", "discard", "cancel subscription", "unsubscribe", "transfer",
    "donate", "subscribe", "sign up", "register", "withdraw", "sell", "bid", "apply now",
    "mua", "thanh toan", "dat hang", "dat ngay", "dat ve", "dat phong", "dat cho", "hoan tat", "xac nhan", "gui",
    "dang bai", "dang tin", "binh luan", "chia se", "xoa", "huy", "chuyen khoan", "chuyen tien", "dang ky",
    "nap tien", "rut tien", "ung ho",
];

const SEARCH_WORDS: &[&str] = &["search", "query", "find", "tim", "tim kiem", "tra cuu"];

fn tokens(text: &str) -> Vec<String> {
    fold(text).split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).map(str::to_string).collect()
}

/// `phrase`'s words appear in `text`, in order, each within two words of the last.
fn has_phrase(text: &[String], phrase: &str) -> bool {
    let want: Vec<&str> = phrase.split(' ').collect();
    (0..text.len()).any(|start| {
        if text[start] != want[0] {
            return false;
        }
        let mut at = start;
        want[1..].iter().all(|w| match text[at + 1..text.len().min(at + 4)].iter().position(|t| t == w) {
            Some(offset) => {
                at += 1 + offset;
                true
            }
            None => false,
        })
    })
}

fn any_word(text: &str, words: &'static [&'static str]) -> Option<&'static str> {
    let text = tokens(text);
    words.iter().copied().find(|w| has_phrase(&text, w))
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
        // Enter submits: a search is harmless; a text area or a chat box may
        // send what was typed; otherwise it presses the form's own button.
        "KEY_ENTER" => {
            let flag = |k: &str| action.get(k).and_then(Value::as_bool) == Some(true);
            if flag("search") || any_word(label, SEARCH_WORDS).is_some() {
                return (Tier::Auto, "submit a search".into());
            }
            if flag("multiline") {
                return (Tier::Approve, format!("{label} may send what was typed"));
            }
            match action.get("submit").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
                Some(submit) => match any_word(submit, RISKY) {
                    Some(word) => (Tier::Approve, format!("Enter presses \"{submit}\" ({word})")),
                    None => (Tier::Logged, format!("Enter presses \"{submit}\"")),
                },
                // No form, or a submit button with no words (an icon): unknown.
                None => (Tier::Approve, format!("{label}: what Enter submits is unknown")),
            }
        }
        // Only an alert is harmless to accept; confirm, prompt and
        // beforeunload are decisions, and an unknown dialog is treated as one.
        "DIALOG_ACCEPT" => match dialog_type {
            Some("alert") => (Tier::Auto, String::new()),
            Some(kind) => (Tier::Approve, format!("accept a {kind} dialog")),
            None => (Tier::Approve, "accept a dialog".into()),
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

static OWN_PORTS: std::sync::OnceLock<Vec<u16>> = std::sync::OnceLock::new();

/// The daemon's own ports (UI and WebSocket gateway), set once at start.
pub fn set_own_ports(ports: &[u16]) {
    let _ = OWN_PORTS.set(ports.to_vec());
}

/// A page on this machine at one of SenClaw's own ports. Its API answers any
/// loopback caller in the default auth mode, so a tab there could read the
/// settings, provider keys included, for whoever is steering the agent.
pub fn is_own_api(url: &str) -> bool {
    OWN_PORTS.get().is_some_and(|ports| is_local_port(url, ports))
}

fn is_local_port(url: &str, ports: &[u16]) -> bool {
    let Ok(u) = reqwest::Url::parse(url.trim()) else { return false };
    // `localhost.` is `localhost`; `[::ffff:127.0.0.1]` is 127.0.0.1. (Names
    // that merely resolve here, like `lvh.me`, are refused by the daemon's own
    // Host check instead: they get no loopback trust.)
    let host = u.host_str().unwrap_or_default().trim_start_matches('[').trim_end_matches(']').trim_end_matches('.').to_ascii_lowercase();
    let local = host == "localhost"
        || host.ends_with(".localhost")
        || host.parse::<std::net::IpAddr>().is_ok_and(|ip| {
            let ip = match ip {
                std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map(std::net::IpAddr::V4).unwrap_or(ip),
                v4 => v4,
            };
            ip.is_loopback() || ip.is_unspecified()
        });
    local && u.port_or_known_default().is_some_and(|p| ports.contains(&p))
}

/// The host a browser would contact (WHATWG parsing: `https://evil.test\\@mail.google.com/` is evil.test).
pub fn host_of(url: &str) -> String {
    reqwest::Url::parse(url.trim())
        .ok()
        .and_then(|u| u.host_str().map(|h| h.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase()))
        .unwrap_or_default()
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

/// `local_profile` is the request format of the local checkpoint
/// (`Decider::local_profile`).
pub fn select_backend(settings: &BrowserSettings, url: &str, driver: Driver, local_profile: Profile) -> DecisionRoute {
    let host = host_of(url);
    let local = DecisionRoute::Model {
        profile: local_profile,
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
        assert_eq!(risk_tier("DIALOG_ACCEPT", &json!({}), None).0, Tier::Approve, "an unknown dialog is a decision");
        assert_eq!(risk_tier("SCROLL_DOWN", &json!({}), None).0, Tier::Auto);
    }

    #[test]
    fn final_purchase_labels_need_the_person() {
        for label in ["Place your order", "Complete booking", "Book", "Submit", "Complete order", "Move to trash", "Pay now", "Hoàn tất đặt chỗ"] {
            assert_eq!(risk_tier("CLICK", &click(label), None).0, Tier::Approve, "{label}");
        }
        for label in ["Books", "Order by price", "53 comments", "Facebook", "Cài đặt", "Đăng nhập", "Free cancellation"] {
            assert_eq!(risk_tier("CLICK", &click(label), None).0, Tier::Auto, "{label}");
        }
    }

    #[test]
    fn enter_is_judged_by_what_it_submits() {
        let enter = |extra: Value| {
            let mut a = json!({"id": "key_enter", "kind": "key", "label": "Press Enter in Message"});
            a.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            risk_tier("KEY_ENTER", &a, None).0
        };
        assert_eq!(enter(json!({"search": true})), Tier::Auto);
        assert_eq!(enter(json!({"multiline": true})), Tier::Approve, "a chat box sends on Enter");
        assert_eq!(enter(json!({"submit": "Place order"})), Tier::Approve);
        assert_eq!(enter(json!({"submit": "Sign in"})), Tier::Logged);
        assert_eq!(enter(json!({})), Tier::Approve, "outside a form Enter may do anything");
        assert_eq!(enter(json!({"submit": ""})), Tier::Approve, "an icon-only submit button says nothing");
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
        assert!(matches!(select_backend(&s, "https://example.com/", Driver::Managed, Profile::LayaV5), DecisionRoute::Model { backend: Backend::Local, profile: Profile::LayaV5, .. }), "the local checkpoint's own format");
        s.hosted_domains = vec!["example.com".into(), "vietcombank.com.vn".into()];
        assert!(matches!(select_backend(&s, "https://www.example.com/a", Driver::Managed, Profile::LayaV5), DecisionRoute::Model { backend: Backend::Online, profile: Profile::JevFull, redact: true, .. }));
        assert!(matches!(select_backend(&s, "https://www.example.com/a", Driver::Extension, Profile::LayaV3), DecisionRoute::Model { backend: Backend::Local, .. }), "the person's Chrome stays local");
        assert!(matches!(select_backend(&s, "https://vietcombank.com.vn/", Driver::Managed, Profile::LayaV3), DecisionRoute::Model { backend: Backend::Local, .. }), "sensitive stays local");
        s.decision_backend = DecisionBackend::LlmOnly;
        assert_eq!(select_backend(&s, "https://example.com/", Driver::Managed, Profile::LayaV3), DecisionRoute::LlmOnly);
        assert_eq!(host_of("https://user@Sub.Example.com:8443/path?q=1"), "sub.example.com");
        assert_eq!(host_of("https://evil.test\\@mail.google.com/"), "evil.test", "a backslash ends the host");
        assert_eq!(host_of("not a url"), "");
        let own = [18788, 18789];
        for url in [
            "http://127.0.0.1:18788/api/llm-config",
            "http://localhost:18789/",
            "http://[::1]:18788/x",
            "http://0.0.0.0:18788/",
            "http://localhost.:18788/",
            "http://[::ffff:127.0.0.1]:18788/",
            "http://2130706433:18788/",
            "http://0x7f.1:18788/",
        ] {
            assert!(is_local_port(url, &own), "{url}");
        }
        for url in ["http://127.0.0.1:28795/", "https://example.com:18788/", "http://127.0.0.1/"] {
            assert!(!is_local_port(url, &own), "{url}");
        }
    }
}
