//! Turning a pattern's files into the two strings an LLM call needs.
//!
//! ```text
//! system.md ─┬─ {{var}} substitution ─┬─ + strategy prompt ─ + language rule ─▶ system
//! user.md ───┘                        └─────────────────────────────────────▶ user
//! input ────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! Two conventions come straight from Fabric and are load-bearing for its
//! library to work unmodified:
//!
//! - **`{{input}}` is where the text goes when the pattern says so.** Most
//!   patterns end with a bare `# INPUT:` header and expect the input as the
//!   *user message*; a minority interpolate `{{input}}` mid-prompt. Doing both
//!   from the same call is what makes 250 patterns work without per-pattern
//!   handling.
//! - **An unknown `{{placeholder}}` is left verbatim**, never blanked — the
//!   same rule [`crate::scaffold`] follows. Blanking silently deletes an
//!   instruction; leaving it makes the omission visible in the output.

use std::collections::BTreeMap;

use serde::Serialize;

use super::strategy::Strategy;

/// What to render.
#[derive(Debug, Clone, Default)]
pub struct RenderRequest<'a> {
    /// Body of `system.md`.
    pub system: &'a str,
    /// Body of `user.md`, when the pattern ships one.
    pub user_template: Option<&'a str>,
    /// The text being transformed.
    pub input: &'a str,
    /// `-v` style variables. A leading `#` on the key is accepted and stripped,
    /// because that is how Fabric's CLI spells them (`-v=#role:expert`).
    pub variables: BTreeMap<String, String>,
    pub strategy: Option<&'a Strategy>,
    /// Reply language. See [`language_rule`].
    pub language: Option<&'a str>,
}

/// The finished prompt pair.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderedPattern {
    pub system: String,
    pub user: String,
    /// Placeholders left in the text because nothing supplied them. Surfaced
    /// rather than swallowed so the caller can tell the user which `-v` they
    /// forgot.
    pub unresolved: Vec<String>,
}

/// Instruction appended after everything else so it outranks the pattern's own
/// `# OUTPUT INSTRUCTIONS`.
///
/// Fabric's library is written by and for English speakers and most patterns
/// pin the output language implicitly. Without this a Vietnamese user feeding
/// in Vietnamese text gets an English summary back. It is appended at render
/// time rather than patched into the file because the file is a git checkout
/// the next sync would revert.
fn language_rule(language: &str) -> String {
    let l = language.trim();
    if l.eq_ignore_ascii_case("auto") || l.eq_ignore_ascii_case("input") {
        "# LANGUAGE\n\nWrite the entire response in the same language as the INPUT text. \
         Keep the section headers exactly as specified above, in English."
            .to_string()
    } else {
        format!(
            "# LANGUAGE\n\nWrite the entire response in {l}. \
             Keep the section headers exactly as specified above, in English."
        )
    }
}

/// Normalise a variable key: `#role` and `role` are the same variable.
fn norm_key(k: &str) -> String {
    k.trim().trim_start_matches('#').trim().to_string()
}

/// Replace every `{{key}}` the map knows, collecting the ones it does not.
///
/// Hand-rolled rather than regex-driven so that an unbalanced `{{` in a
/// pattern body (Fabric has a few, inside code samples) scans past instead of
/// eating the rest of the file.
fn substitute(text: &str, vars: &BTreeMap<String, String>, unresolved: &mut Vec<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < text.len() {
        if bytes[i] == b'{' && i + 1 < text.len() && bytes[i + 1] == b'{' {
            if let Some(rel) = text[i + 2..].find("}}") {
                let raw = &text[i + 2..i + 2 + rel];
                // A newline inside the braces means this was never a
                // placeholder — it is prose that happens to contain `{{`.
                if !raw.contains('\n') && raw.len() <= 64 {
                    let key = norm_key(raw);
                    match vars.get(&key) {
                        Some(v) => out.push_str(v),
                        None => {
                            if !key.is_empty() && !unresolved.contains(&key) {
                                unresolved.push(key);
                            }
                            out.push_str(&text[i..i + 2 + rel + 2]);
                        }
                    }
                    i += 2 + rel + 2;
                    continue;
                }
            }
        }
        // Step by whole characters: byte-stepping would split a multi-byte
        // Vietnamese character and panic on the slice.
        let ch_len = text[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&text[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// Build the system + user pair for one pattern run.
pub fn render_pattern(req: &RenderRequest<'_>) -> RenderedPattern {
    let mut vars: BTreeMap<String, String> = req
        .variables
        .iter()
        .map(|(k, v)| (norm_key(k), v.clone()))
        .collect();
    // `input` is reserved: a pattern that interpolates it wins over a caller
    // that also passed `-v input:…`, because the input is what was actually
    // being transformed.
    vars.insert("input".to_string(), req.input.to_string());

    let mut unresolved = Vec::new();
    let system_had_input = req.system.contains("{{input}}");
    let mut system = substitute(req.system, &vars, &mut unresolved);

    let user_had_input = req
        .user_template
        .map(|t| t.contains("{{input}}"))
        .unwrap_or(false);
    let mut user = match req.user_template {
        Some(t) => substitute(t, &vars, &mut unresolved),
        None => String::new(),
    };

    // Only append the raw input when the pattern did not already place it.
    // Appending unconditionally would send the whole document twice — which
    // for a long transcript is a doubled bill and a truncated context.
    if !system_had_input && !user_had_input {
        if user.trim().is_empty() {
            user = req.input.to_string();
        } else {
            user.push_str("\n\n");
            user.push_str(req.input);
        }
    }

    if let Some(s) = req.strategy {
        if !s.prompt.trim().is_empty() {
            system.push_str("\n\n");
            system.push_str(s.prompt.trim());
        }
    }

    if let Some(lang) = req.language.filter(|l| !l.trim().is_empty()) {
        system.push_str("\n\n");
        system.push_str(&language_rule(lang));
    }

    // `input` is always defined, so it is never a real miss.
    unresolved.retain(|k| k != "input");
    RenderedPattern {
        system,
        user,
        unresolved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req<'a>(system: &'a str, input: &'a str) -> RenderRequest<'a> {
        RenderRequest {
            system,
            input,
            ..Default::default()
        }
    }

    #[test]
    fn input_becomes_the_user_message_when_the_pattern_has_no_placeholder() {
        let r = render_pattern(&req("# IDENTITY\nSummarise.\n\n# INPUT:", "hello world"));
        assert_eq!(r.user, "hello world");
        assert!(r.system.ends_with("# INPUT:"));
    }

    #[test]
    fn input_is_interpolated_when_the_pattern_asks_for_it_and_not_sent_twice() {
        let r = render_pattern(&req("Rewrite this: {{input}}", "hello"));
        assert_eq!(r.system, "Rewrite this: hello");
        assert!(r.user.is_empty(), "input must not also be appended");
    }

    #[test]
    fn unknown_placeholders_survive_verbatim_and_are_reported() {
        let r = render_pattern(&req("You are a {{role}} writing {{points}} points.", "x"));
        assert!(r.system.contains("{{role}}"));
        assert!(r.system.contains("{{points}}"));
        let mut missing = r.unresolved.clone();
        missing.sort();
        assert_eq!(missing, vec!["points".to_string(), "role".to_string()]);
    }

    #[test]
    fn variables_accept_the_fabric_hash_spelling() {
        let mut r = req("You are a {{role}}.", "x");
        r.variables.insert("#role".into(), "surgeon".into());
        let out = render_pattern(&r);
        assert_eq!(out.system, "You are a surgeon.");
        assert!(out.unresolved.is_empty());
    }

    #[test]
    fn strategy_and_language_are_appended_after_the_pattern() {
        let mut r = req("# OUTPUT INSTRUCTIONS\n- Be terse.", "x");
        let s = Strategy {
            description: "CoT".into(),
            prompt: "Think step by step.".into(),
            name: "cot".into(),
        };
        r.strategy = Some(&s);
        r.language = Some("Vietnamese");
        let out = render_pattern(&r);
        let think = out.system.find("Think step by step.").unwrap();
        let lang = out.system.find("# LANGUAGE").unwrap();
        assert!(think < lang, "language rule must come last so it wins");
        assert!(out.system.contains("Vietnamese"));
    }

    #[test]
    fn auto_language_follows_the_input() {
        let mut r = req("body", "xin chào");
        r.language = Some("auto");
        assert!(render_pattern(&r)
            .system
            .contains("same language as the INPUT"));
    }

    #[test]
    fn multibyte_body_does_not_panic_and_survives_intact() {
        let body = "Tóm tắt nội dung sau đây thật ngắn gọn. {{role}} — xong.";
        let r = render_pattern(&req(body, "nội dung"));
        assert!(r.system.contains("Tóm tắt nội dung"));
        assert!(r.system.contains("{{role}}"));
    }

    #[test]
    fn unbalanced_braces_scan_past_instead_of_eating_the_file() {
        let r = render_pattern(&req("code: {{ not closed\n\n# OUTPUT", "x"));
        assert!(r.system.contains("# OUTPUT"));
    }

    #[test]
    fn user_template_receives_the_input_when_it_has_no_placeholder() {
        let mut r = req("sys", "the text");
        r.user_template = Some("Context: none.");
        let out = render_pattern(&r);
        assert_eq!(out.user, "Context: none.\n\nthe text");
    }
}
