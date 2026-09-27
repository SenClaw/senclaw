//! The variable set a template is rendered against.
//!
//! Everything a template can interpolate lives in one flat `name -> value` map,
//! built here. Two rules keep it predictable:
//!
//! - **Every case a template could want is derived once**, from the single name
//!   the user typed. A template never has to spell `{{app_id}}` and hope the
//!   user typed kebab-case — `senclaw create app "My Todo List"` yields
//!   `my-todo-list`, `my_todo_list`, `MyTodoList` and `myTodoList` together.
//! - **Unknown names are not invented.** `--var k=v` and the template's own
//!   `variables` block are merged in on top, so a template can ask for anything,
//!   but the renderer refuses to guess.

use std::collections::BTreeMap;

/// The rendering context: a flat map because templates address it by name and
/// nothing here is nested. `BTreeMap` so `--dry-run` output and error messages
/// list variables in a stable order.
pub type Vars = BTreeMap<String, String>;

/// Fold Vietnamese diacritics to ASCII, preserving case.
///
/// `"Ứng dụng"` has to become `ung-dung`, not `ng-d-ng`: stripping non-ASCII
/// character by character keeps the undecorated letters and drops the rest,
/// which produces an identifier that looks like a typo of the name the user
/// typed. Uses the same table as [`crate::security::replication::fold`] so the
/// whole codebase folds one way.
pub fn fold_ascii(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii() {
                return c;
            }
            let lower: String = c.to_lowercase().collect();
            let folded = crate::security::replication::fold(&lower)
                .chars()
                .next()
                .unwrap_or(c);
            if c.is_uppercase() {
                folded.to_ascii_uppercase()
            } else {
                folded
            }
        })
        .collect()
}

/// Split an arbitrary human name into lowercase words.
///
/// Handles the three shapes people actually type — `"My Todo List"`,
/// `"my-todo-list"`, `"MyTodoList"` — plus digits, folds Vietnamese diacritics,
/// and drops anything still not ASCII. A name with nothing ASCII left (`"日本語"`)
/// yields no words, and the caller turns that into a "pass --id" error rather
/// than writing an app called `-`.
pub fn words(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev_lower_or_digit = false;

    for ch in fold_ascii(raw).chars() {
        if ch.is_ascii_alphanumeric() {
            // camelCase / PascalCase boundary: a capital right after a
            // lowercase letter or digit starts a new word.
            if ch.is_ascii_uppercase() && prev_lower_or_digit && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            cur.push(ch.to_ascii_lowercase());
            prev_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        } else {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower_or_digit = false;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub fn kebab(raw: &str) -> String {
    words(raw).join("-")
}

pub fn snake(raw: &str) -> String {
    words(raw).join("_")
}

pub fn screaming(raw: &str) -> String {
    words(raw).join("_").to_ascii_uppercase()
}

pub fn pascal(raw: &str) -> String {
    words(raw)
        .iter()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

pub fn camel(raw: &str) -> String {
    let p = pascal(raw);
    let mut c = p.chars();
    match c.next() {
        Some(f) => f.to_ascii_lowercase().to_string() + c.as_str(),
        None => String::new(),
    }
}

/// The human-facing name — the manifest's `name`, the page heading, the entry
/// in the Space Apps list.
///
/// A name the user already wrote as a name is kept **exactly**, diacritics and
/// all: `"Báo cáo tuần"` must not become `"Bao Cao Tuan"` in the UI just
/// because the *id* has to fold to ASCII. Only a slug — no spaces, no capitals,
/// nothing to preserve — is expanded into Title Case.
pub fn title(raw: &str) -> String {
    let trimmed = raw.trim();
    let already_a_name = trimmed
        .chars()
        .any(|c| c.is_whitespace() || c.is_uppercase() || !c.is_ascii());
    if already_a_name {
        return trimmed.to_string();
    }
    words(trimmed)
        .iter()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A Space App id, a skill name and a persona filename share one shape, and it
/// is the shape the daemon's own loaders assume: lowercase, digits, hyphens,
/// starting with a letter. Enforced here so the failure is a sentence at create
/// time rather than an app that installs and never appears.
pub fn is_valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    id.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !id.ends_with('-')
        && !id.contains("--")
}

/// The variables every template gets, whatever its kind.
///
/// `id` is passed separately from `raw_name` because the CLI lets `--id`
/// override the slug derived from the name.
pub fn base_vars(raw_name: &str, id: &str) -> Vars {
    let mut v = Vars::new();
    v.insert("name".into(), raw_name.to_string());
    v.insert("id".into(), id.to_string());
    v.insert("kebab_name".into(), kebab(id));
    v.insert("snake_name".into(), snake(id));
    v.insert("pascal_name".into(), pascal(id));
    v.insert("camel_name".into(), camel(id));
    v.insert("screaming_name".into(), screaming(id));
    v.insert("title_name".into(), title(raw_name));
    v.insert("year".into(), current_year());
    v.insert(
        "senclaw_version".into(),
        env!("CARGO_PKG_VERSION").to_string(),
    );
    v.insert("api_version".into(), crate::apps::API_VERSION.to_string());
    v
}

/// The year, for a licence header. Uses `chrono` rather than a build-time
/// constant so a binary built in December still writes the right year in
/// January.
fn current_year() -> String {
    use chrono::Datelike;
    chrono::Local::now().year().to_string()
}

/// Parse a `--var key=value` pair. The value may contain `=`; the key may not.
pub fn parse_var(arg: &str) -> anyhow::Result<(String, String)> {
    match arg.split_once('=') {
        Some((k, val)) if !k.trim().is_empty() => Ok((k.trim().to_string(), val.to_string())),
        _ => anyhow::bail!("`--var` cần dạng key=value, nhận được: {arg:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_every_shape_people_type() {
        assert_eq!(words("My Todo List"), ["my", "todo", "list"]);
        assert_eq!(words("my-todo-list"), ["my", "todo", "list"]);
        assert_eq!(words("MyTodoList"), ["my", "todo", "list"]);
        assert_eq!(words("myTodoList"), ["my", "todo", "list"]);
        assert_eq!(words("my_todo_list"), ["my", "todo", "list"]);
        assert_eq!(words("todo2do"), ["todo2do"]);
        assert_eq!(words("Todo2Do"), ["todo2", "do"]);
    }

    #[test]
    fn folds_vietnamese_rather_than_mangling_it() {
        // Character-by-character stripping would give "ng-d-ng" — an identifier
        // that looks like a typo of what the user typed.
        assert_eq!(kebab("Ứng dụng"), "ung-dung");
        assert_eq!(kebab("Quản lý Kho"), "quan-ly-kho");
        assert_eq!(kebab("Đơn hàng"), "don-hang");
    }

    #[test]
    fn the_display_name_keeps_its_diacritics() {
        // Only the id folds. The manifest `name`, the page heading and the UI
        // list all show what the user actually typed.
        assert_eq!(title("Báo cáo tuần"), "Báo cáo tuần");
        assert_eq!(title("Quản lý Kho"), "Quản lý Kho");
        assert_eq!(title("My Todo List"), "My Todo List");
        // A bare slug has nothing to preserve, so it is expanded.
        assert_eq!(title("my-todo-list"), "My Todo List");
        assert_eq!(title("todo"), "Todo");
    }

    #[test]
    fn a_name_with_no_foldable_ascii_yields_nothing() {
        // The caller turns an empty slug into a "pass --id" error rather than
        // writing an app called "-".
        assert_eq!(kebab("日本語"), "");
        assert!(!is_valid_id(&kebab("日本語")));
    }

    #[test]
    fn derives_all_cases() {
        assert_eq!(kebab("My Todo List"), "my-todo-list");
        assert_eq!(snake("My Todo List"), "my_todo_list");
        assert_eq!(pascal("My Todo List"), "MyTodoList");
        assert_eq!(camel("My Todo List"), "myTodoList");
        assert_eq!(screaming("My Todo List"), "MY_TODO_LIST");
        assert_eq!(title("my-todo-list"), "My Todo List");
    }

    #[test]
    fn id_shape_matches_what_the_daemon_loads() {
        assert!(is_valid_id("todo"));
        assert!(is_valid_id("my-todo-2"));
        assert!(!is_valid_id("My-Todo"), "uppercase");
        assert!(!is_valid_id("2todo"), "leading digit");
        assert!(!is_valid_id("todo-"), "trailing hyphen");
        assert!(!is_valid_id("to--do"), "double hyphen");
        assert!(!is_valid_id("to_do"), "underscore");
        assert!(!is_valid_id(""), "empty");
    }

    #[test]
    fn var_pairs_keep_equals_in_the_value() {
        assert_eq!(
            parse_var("desc=a=b").unwrap(),
            ("desc".to_string(), "a=b".to_string())
        );
        assert!(parse_var("nope").is_err());
        assert!(parse_var("=v").is_err());
    }
}
