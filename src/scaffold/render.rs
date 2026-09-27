//! The template engine: `{{variable}}` substitution in file contents and in
//! path segments.
//!
//! Deliberately not a general-purpose engine — no loops, no conditionals, no
//! filters. A scaffold template is a working app with a few names swapped; the
//! moment it needs branching it should be two templates. That keeps the
//! contract small enough that a template author can hold all of it:
//!
//! - `{{name}}` or `{{ name }}` — substituted when `name` is a known variable.
//! - `{{{{` — a literal `{{`, for a template that ships handlebars/Vue/Go
//!   template syntax of its own.
//! - Anything else in braces is **left exactly as written**, and reported.
//!   This is the important one: a Vue template's `{{ count }}` and a Go
//!   template's `{{.Name}}` must survive scaffolding untouched, so an unknown
//!   placeholder cannot be a hard error. It is surfaced as a warning instead,
//!   which is what catches the real case — a typo'd `{{app_i}}`.
//!
//! Files that are not valid UTF-8 are copied byte-for-byte, so an icon or a
//! font in a template is not corrupted by a pass that has nothing to do.

use std::collections::BTreeSet;

use super::vars::Vars;

/// What a render produced: the text, plus every `{{placeholder}}` that matched
/// the variable syntax but had no variable behind it.
#[derive(Debug, Default)]
pub struct Rendered {
    pub text: String,
    pub unknown: BTreeSet<String>,
}

/// How a substituted **value** must be escaped for the file it lands in.
///
/// Only the value is escaped, never the template's own text — a template author
/// who writes a quote in their JSON meant it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escape {
    /// Source code, markdown, shell: substituted verbatim.
    None,
    /// JSON. Values come from `--desc`, `--icon` and `--var`, which are
    /// arbitrary user text: `--desc 'Quản lý "công việc"'` would otherwise
    /// produce a manifest that does not parse, and a value crafted as
    /// `x", "id": "evil` would produce one that parses into a *different app*
    /// — serde takes the last duplicate key, and the injected one comes after.
    Json,
    /// Markdown carrying a YAML frontmatter block — a skill's `SKILL.md`, a
    /// persona's `.md`.
    ///
    /// Same attack, different parser. A description containing a newline can
    /// add `name: evil-persona` (the registry's map keeps the *last* duplicate,
    /// so the persona registers under a name its file never had) or a bare
    /// `---`, which ends the frontmatter early and turns the rest into the
    /// system prompt. Even without malice, an ordinary `--desc "Trợ lý: quản lý
    /// kho"` breaks the block on the colon.
    ///
    /// Escaping is YAML double-quoted rules, which are JSON's — so a template
    /// that writes `description: "{{description}}"` is safe for any input.
    ///
    /// Applies to the **frontmatter block only**, not the markdown after it —
    /// see [`render_file`]. A README has no frontmatter at all, and escaping its
    /// prose would print `has \"quotes\"` at the top of every generated project.
    Yaml,
    /// An HTML page. A description lands in the app's own UI, and `<script>` in
    /// it is a script tag on a page the daemon proxies. The author supplies the
    /// string, so this is not remote XSS — but there is no reason to write a
    /// live tag into a file that only meant to show a sentence.
    Html,
}

impl Escape {
    /// Pick from the destination path.
    pub fn for_path(rel: &str) -> Escape {
        let lower = rel.to_ascii_lowercase();
        let base = lower.rsplit('/').next().unwrap_or(&lower);
        // Config files that are JSON without saying so in an extension. None of
        // the bundled templates ship one, but a Node template is one `.babelrc`
        // away from writing unparseable JSON that nothing validates.
        const JSON_BASENAMES: &[&str] = &[
            ".babelrc",
            ".eslintrc",
            ".prettierrc",
            ".swcrc",
            "jsconfig",
            "tsconfig",
        ];
        if lower.ends_with(".json") || JSON_BASENAMES.contains(&base) {
            Escape::Json
        } else if lower.ends_with(".md") || lower.ends_with(".markdown") {
            Escape::Yaml
        } else if lower.ends_with(".html") || lower.ends_with(".htm") {
            Escape::Html
        } else {
            Escape::None
        }
    }

    fn apply(self, value: &str, out: &mut String) {
        match self {
            Escape::None => out.push_str(value),
            // YAML's double-quoted scalar uses JSON's escapes, so one routine
            // serves both.
            Escape::Json | Escape::Yaml => {
                for c in value.chars() {
                    match c {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        '\r' => out.push_str("\\r"),
                        '\t' => out.push_str("\\t"),
                        c if (c as u32) < 0x20 => {
                            out.push_str(&format!("\\u{:04x}", c as u32))
                        }
                        c => out.push(c),
                    }
                }
            }
            Escape::Html => {
                for c in value.chars() {
                    match c {
                        '&' => out.push_str("&amp;"),
                        '<' => out.push_str("&lt;"),
                        '>' => out.push_str("&gt;"),
                        '"' => out.push_str("&quot;"),
                        c => out.push(c),
                    }
                }
            }
        }
    }
}

/// Render one file, choosing the escaping from its path — and, for markdown,
/// from its structure.
///
/// A `.md` file is two things at once: a YAML block that a parser reads, and
/// prose that a person reads. They need opposite treatment, and picking one
/// escaping for the whole file gets one of them wrong — either a description
/// with a colon breaks a skill's frontmatter, or every generated README opens
/// with `has \"quotes\"`. So the frontmatter is rendered as YAML and everything
/// after it as plain text.
pub fn render_file(rel: &str, text: &str, vars: &Vars) -> Rendered {
    let escape = Escape::for_path(rel);
    if escape != Escape::Yaml {
        return render_escaped(text, vars, escape);
    }
    match split_frontmatter(text) {
        Some((front, body)) => {
            let f = render_escaped(front, vars, Escape::Yaml);
            let b = render_escaped(body, vars, Escape::None);
            let mut unknown = f.unknown;
            unknown.extend(b.unknown);
            let mut text = f.text;
            text.push_str(&b.text);
            Rendered { text, unknown }
        }
        // No frontmatter — a README. All prose.
        None => render_escaped(text, vars, Escape::None),
    }
}

/// Split a markdown file into its leading `--- … ---` block (inclusive) and the
/// rest. `None` when there is no frontmatter.
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    // +3 for the leading `---` we stripped, +4 for `\n---`.
    let close = 3 + end + 4;
    Some(text.split_at(close))
}

/// True when `s` could be one of our variable names: lowercase ASCII, digits
/// and underscores, starting with a letter.
///
/// The narrow shape is what makes coexistence with other `{{…}}` syntaxes work.
/// `{{.Name}}`, `{{ item.title }}` and `{{#each}}` all fail this test and are
/// passed through without even being reported.
fn is_var_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Substitute `{{var}}` in `input`, with values inserted verbatim.
pub fn render(input: &str, vars: &Vars) -> Rendered {
    render_escaped(input, vars, Escape::None)
}

/// Substitute `{{var}}` in `input`, escaping each value for `escape`.
pub fn render_escaped(input: &str, vars: &Vars, escape: Escape) -> Rendered {
    let mut out = String::with_capacity(input.len());
    let mut unknown = BTreeSet::new();
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'{' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            // `{{{{` is the escape for a literal `{{`.
            if i + 3 < bytes.len() && bytes[i + 2] == b'{' && bytes[i + 3] == b'{' {
                out.push_str("{{");
                i += 4;
                continue;
            }
            if let Some(close) = input[i + 2..].find("}}") {
                let inner = &input[i + 2..i + 2 + close];
                let key = inner.trim();
                if is_var_name(key) {
                    match vars.get(key) {
                        Some(val) => escape.apply(val, &mut out),
                        None => {
                            unknown.insert(key.to_string());
                            out.push_str(&input[i..i + 2 + close + 2]);
                        }
                    }
                    i += 2 + close + 2;
                    continue;
                }
                // Not our syntax (`{{.Name}}`, `{{#if}}`, `{{ a.b }}`): copy the
                // opening braces and carry on scanning from just after them, so
                // a nested `{{var}}` inside is still reachable.
                out.push_str("{{");
                i += 2;
                continue;
            }
            // Unterminated `{{` — nothing to do but copy it.
            out.push_str("{{");
            i += 2;
            continue;
        }

        // Copy one whole UTF-8 character.
        let ch_len = utf8_len(bytes[i]);
        let end = (i + ch_len).min(bytes.len());
        out.push_str(&input[i..end]);
        i = end;
    }

    Rendered { text: out, unknown }
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// Render a relative path one segment at a time, so `{{id}}/src/{{id}}.rs`
/// works and a variable containing `/` cannot escape the target directory.
///
/// Returns `None` when a segment renders to empty (a template guarding an
/// optional file behind `{{maybe}}/x` would otherwise write into the parent).
pub fn render_path(rel: &str, vars: &Vars) -> Option<(String, BTreeSet<String>)> {
    let mut parts: Vec<String> = Vec::new();
    let mut unknown = BTreeSet::new();

    for seg in rel.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        // A rendered `..` is refused for the same reason a rendered `/` is:
        // path traversal out of the destination.
        if seg == ".." {
            return None;
        }
        let r = render(seg, vars);
        unknown.extend(r.unknown);
        let cleaned = r.text.trim().to_string();
        if cleaned.is_empty() || cleaned == ".." || cleaned.contains('/') || cleaned.contains('\\') {
            return None;
        }
        parts.push(cleaned);
    }

    if parts.is_empty() {
        return None;
    }
    Some((parts.join("/"), unknown))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars {
        let mut v = Vars::new();
        v.insert("id".into(), "todo".into());
        v.insert("port".into(), "4800".into());
        v
    }

    #[test]
    fn substitutes_with_and_without_spaces() {
        let r = render("id={{id}} port={{ port }}", &vars());
        assert_eq!(r.text, "id=todo port=4800");
        assert!(r.unknown.is_empty());
    }

    #[test]
    fn unknown_variable_survives_verbatim_and_is_reported() {
        let r = render("{{app_i}}", &vars());
        assert_eq!(r.text, "{{app_i}}", "must not silently blank the text");
        assert_eq!(r.unknown.iter().next().unwrap(), "app_i");
    }

    #[test]
    fn foreign_template_syntax_is_untouched_and_unreported() {
        // A Go template, a Vue expression, a handlebars block: all pass through
        // and none of them count as a typo.
        for src in ["{{.Name}}", "{{ item.title }}", "{{#each xs}}", "{{ Foo }}"] {
            let r = render(src, &vars());
            assert_eq!(r.text, src, "{src}");
            assert!(r.unknown.is_empty(), "{src}");
        }
    }

    #[test]
    fn nested_variable_inside_foreign_syntax_still_renders() {
        let r = render("{{ x.y {{id}} }}", &vars());
        assert_eq!(r.text, "{{ x.y todo }}");
    }

    #[test]
    fn double_braces_escape_to_a_literal() {
        let r = render("{{{{ raw }}", &vars());
        assert_eq!(r.text, "{{ raw }}");
    }

    #[test]
    fn non_ascii_content_is_preserved() {
        let r = render("Ứng dụng {{id}} — đã tạo ✅", &vars());
        assert_eq!(r.text, "Ứng dụng todo — đã tạo ✅");
    }

    #[test]
    fn unterminated_brace_is_copied() {
        let r = render("a {{ b", &vars());
        assert_eq!(r.text, "a {{ b");
    }

    #[test]
    fn json_values_are_escaped_but_the_templates_own_text_is_not() {
        let mut v = vars();
        v.insert("desc".into(), r#"Quản lý "công việc""#.into());
        let r = render_escaped(r#"{"a": "{{desc}}", "b": "x"}"#, &v, Escape::Json);
        let parsed: serde_json::Value = serde_json::from_str(&r.text).expect("phải là JSON hợp lệ");
        assert_eq!(parsed["a"], r#"Quản lý "công việc""#);
        assert_eq!(parsed["b"], "x");
    }

    /// serde takes the *last* duplicate key, so a value that closes its string
    /// and opens a new pair would rename the app being created.
    #[test]
    fn a_value_cannot_inject_a_second_json_key() {
        let mut v = vars();
        v.insert("desc".into(), r#"x", "id": "evil"#.into());
        let r = render_escaped(r#"{"id": "todo", "d": "{{desc}}"}"#, &v, Escape::Json);
        let parsed: serde_json::Value = serde_json::from_str(&r.text).unwrap();
        assert_eq!(parsed["id"], "todo", "id không được bị ghi đè");
        assert_eq!(parsed["d"], r#"x", "id": "evil"#);
    }

    /// A markdown file is a YAML block a parser reads plus prose a person
    /// reads. One escaping for the whole file gets one of them wrong.
    #[test]
    fn markdown_escapes_the_frontmatter_and_leaves_the_prose_alone() {
        let mut v = vars();
        v.insert("description".into(), r#"Proxy: có "nháy" kép"#.into());

        let skill = "---\nname: \"{{id}}\"\ndescription: \"{{description}}\"\n---\n\n# {{id}}\n\n{{description}}\n";
        let r = render_file("SKILL.md", skill, &v);

        let front = r.text.split("\n---").next().unwrap();
        assert!(
            front.contains(r#"description: "Proxy: có \"nháy\" kép""#),
            "frontmatter phải escape: {front}"
        );
        assert!(
            r.text.ends_with("Proxy: có \"nháy\" kép\n"),
            "phần body phải nguyên văn: {}",
            r.text
        );
        // And it must actually parse.
        let fm = crate::skills::metadata::extract_frontmatter(&r.text).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(fm).unwrap();
        assert_eq!(
            parsed.get("description").and_then(|d| d.as_str()),
            Some(r#"Proxy: có "nháy" kép"#)
        );
    }

    /// A README has no frontmatter, so escaping it would open every generated
    /// project with `has \"quotes\"`.
    #[test]
    fn a_markdown_file_without_frontmatter_is_pure_prose() {
        let mut v = vars();
        v.insert("description".into(), r#"Proxy: có "nháy" kép"#.into());
        let r = render_file("README.md", "# {{id}}\n\n{{description}}\n", &v);
        assert_eq!(r.text, "# todo\n\nProxy: có \"nháy\" kép\n");
    }

    #[test]
    fn html_values_cannot_carry_a_live_tag_into_the_page() {
        let mut v = vars();
        v.insert("description".into(), "x<script>alert(1)</script> & <b>".into());
        let r = render_file("web/index.html", "<p>{{description}}</p>", &v);
        assert_eq!(
            r.text,
            "<p>x&lt;script&gt;alert(1)&lt;/script&gt; &amp; &lt;b&gt;</p>"
        );
    }

    #[test]
    fn escaping_follows_the_destination_extension() {
        assert_eq!(Escape::for_path("senclaw-manifest.json"), Escape::Json);
        assert_eq!(Escape::for_path("web/config.JSON"), Escape::Json);
        assert_eq!(Escape::for_path("web/index.html"), Escape::Html);
        assert_eq!(Escape::for_path("src/main.rs"), Escape::None);
        // Source code must not be escaped: a description in a Rust string
        // literal is the template author's problem, not ours to mangle.
        let mut v = vars();
        v.insert("desc".into(), "a\\b".into());
        assert_eq!(render_escaped("{{desc}}", &v, Escape::None).text, "a\\b");
        assert_eq!(render_escaped("{{desc}}", &v, Escape::Json).text, "a\\\\b");
    }

    #[test]
    fn path_segments_render_independently() {
        let (p, _) = render_path("{{id}}/src/{{id}}.rs", &vars()).unwrap();
        assert_eq!(p, "todo/src/todo.rs");
    }

    #[test]
    fn a_variable_cannot_smuggle_a_path_separator() {
        let mut v = vars();
        v.insert("id".into(), "../../etc".into());
        assert!(
            render_path("{{id}}/x", &v).is_none(),
            "a value containing / must not create parent directories"
        );
    }

    #[test]
    fn literal_dotdot_in_a_template_path_is_refused() {
        assert!(render_path("../escape", &vars()).is_none());
    }

    #[test]
    fn empty_segment_drops_the_file() {
        let mut v = vars();
        v.insert("maybe".into(), "".into());
        assert!(render_path("{{maybe}}/x", &v).is_none());
    }
}
