//! Rendering a loaded template into a project on disk.
//!
//! Three phases, in this order, and the order is the point:
//!
//! 1. **Render** every file into memory.
//! 2. **Validate** the result — the manifest parses, `runtime.mode` is spelled
//!    a way the daemon recognises, nothing binds `0.0.0.0`.
//! 3. **Write**, only then.
//!
//! A scaffolder that writes as it renders leaves a half-made project behind
//! when the tenth file fails, and the user has to know which files to delete
//! before retrying. Rendering into memory first costs nothing at these sizes
//! and makes the failure clean.
//!
//! Validation deserves a word. These are the checks whose absence produces a
//! project that *looks* fine and fails later, in a place far from the cause:
//! a misspelled `runtime.mode` silently falls back to `session`, so a
//! background poller quietly stops; a hardcoded `0.0.0.0` serves the app's
//! whole unauthenticated REST + MCP surface to the LAN. Both are cheap to catch
//! here and expensive to find in production.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::render::{render, render_file, render_path};
use super::source::LoadedTemplate;
use super::spec::{Kind, TemplateSpec};
use super::vars::Vars;

/// One rendered file, ready to write.
#[derive(Debug, Clone)]
pub struct OutFile {
    pub rel: String,
    pub bytes: Vec<u8>,
    pub executable: bool,
}

/// What a create produced, whether or not it was written.
#[derive(Debug)]
pub struct CreateReport {
    pub dest: PathBuf,
    pub kind: Kind,
    pub files: Vec<OutFile>,
    pub vars: Vars,
    pub origin: String,
    /// Non-fatal problems: unknown `{{placeholders}}`, a manifest field that
    /// disagrees with what the CLI chose.
    pub warnings: Vec<String>,
    pub post_create: Vec<String>,
    /// False for `--dry-run`.
    pub written: bool,
}

/// Fill in the variables a template declares, on top of the ones already set.
///
/// Defaults may reference earlier variables (`"default": "{{id}}-mcp"`), which
/// is why this resolves in declaration order rather than all at once.
pub fn apply_spec_vars(spec: &TemplateSpec, vars: &mut Vars) -> Result<()> {
    for v in &spec.variables {
        if vars.contains_key(&v.name) {
            continue; // --var, or a built-in, already answered it
        }
        match &v.default {
            Some(d) => {
                let r = render(d, vars);
                vars.insert(v.name.clone(), r.text);
            }
            None if v.required => bail!(
                "template cần biến {:?}{}. Truyền bằng: --var {}=…",
                v.name,
                v.description
                    .as_deref()
                    .map(|d| format!(" ({d})"))
                    .unwrap_or_default(),
                v.name
            ),
            None => {
                vars.insert(v.name.clone(), String::new());
            }
        }
    }
    Ok(())
}

/// Render a template's payload. No I/O on the destination.
pub fn render_template(tpl: &LoadedTemplate, vars: &Vars) -> Result<(Vec<OutFile>, Vec<String>)> {
    let mut files = Vec::with_capacity(tpl.files.len());
    let mut unknown: BTreeSet<String> = BTreeSet::new();
    let mut warnings = Vec::new();
    let mut seen: BTreeMap<String, String> = BTreeMap::new();

    for f in &tpl.files {
        let Some((rel, path_unknown)) = render_path(&f.rel, vars) else {
            warnings.push(format!("bỏ qua {} (đường dẫn render ra rỗng)", f.rel));
            continue;
        };
        unknown.extend(path_unknown);

        // Two source files rendering to one destination means the template has
        // a bug that would otherwise show up as a mysteriously missing file.
        if let Some(prev) = seen.insert(rel.clone(), f.rel.clone()) {
            bail!("template hỏng: {} và {} cùng render thành {rel}", prev, f.rel);
        }

        // Only text is substituted. Anything that is not UTF-8 is an image, a
        // font or a lockfile-shaped binary and is copied byte for byte.
        let bytes = match std::str::from_utf8(&f.bytes) {
            Ok(text) => {
                // Escaping is chosen by the *destination*: a description with a
                // quote in it must not be able to break — or rewrite — the
                // manifest it lands in.
                let r = render_file(&rel, text, vars);
                unknown.extend(r.unknown);
                r.text.into_bytes()
            }
            Err(_) => f.bytes.clone(),
        };

        files.push(OutFile {
            rel,
            bytes,
            executable: f.executable,
        });
    }

    if !unknown.is_empty() {
        // Not fatal: `{{ count }}` in a Vue template is legitimate. But a typo'd
        // `{{app_i}}` looks exactly the same, so it has to be said out loud.
        warnings.push(format!(
            "còn placeholder chưa thay: {} — kiểm tra template nếu đó là lỗi chính tả",
            unknown.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }

    Ok((files, warnings))
}

/// Post-render checks on the produced files. Errors stop the write; warnings
/// are printed.
///
/// The wildcard-bind rule is **not** here — it belongs on the template's own
/// source ([`check_bind_host`]), because after substitution a user's
/// description is indistinguishable from the template's code.
pub fn validate(kind: Kind, files: &[OutFile], vars: &Vars) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    let find = |rel: &str| files.iter().find(|f| f.rel == rel);
    let text_of = |f: &OutFile| String::from_utf8_lossy(&f.bytes).to_string();

    match kind {
        Kind::App => {
            let manifest_file = find("senclaw-manifest.json").context(
                "template kiểu app nhưng kết quả không có senclaw-manifest.json — \
                 daemon sẽ không bao giờ nạp thư mục này",
            )?;
            let manifest: serde_json::Value = serde_json::from_str(&text_of(manifest_file))
                .context("senclaw-manifest.json sau khi render không phải JSON hợp lệ")?;

            let id = match manifest.get("id") {
                Some(v) => v
                    .as_str()
                    .context("senclaw-manifest.json: `id` phải là chuỗi")?,
                None => bail!("senclaw-manifest.json thiếu `id`"),
            };
            if id.is_empty() {
                bail!("senclaw-manifest.json có `id` rỗng");
            }
            if let Some(want) = vars.get("id") {
                if id != want {
                    warnings.push(format!(
                        "manifest.id = {id:?} nhưng app id đang tạo là {want:?}"
                    ));
                }
            }

            if let Some(rt) = manifest.get("runtime") {
                // Type first. Every check below reads a field with `as_str` /
                // `as_u64`, which returns None for the *wrong type* just as it
                // does for absent — so a `"port": "4800"` (one stray quote
                // pair, still valid JSON) would skip the port check entirely
                // and reach the daemon, which parses it as 0.
                let rt = rt.as_object().context(
                    "senclaw-manifest.json: `runtime` phải là một object — \
                     daemon bỏ qua mọi thứ trong đó nếu không",
                )?;

                // `kind` and `start` are what decide whether the daemon ever
                // launches this at all: it computes
                // `is_server = kind == "server" && start.is_some()` with a
                // case-sensitive compare and no error path. Get either wrong
                // and the app installs, shows up in the list, and silently does
                // nothing — worse than the misspelled `mode` below, because
                // there is no log line to find.
                match rt.get("kind") {
                    Some(k) => {
                        let k = k
                            .as_str()
                            .context("senclaw-manifest.json: `runtime.kind` phải là chuỗi")?;
                        if k != "server" {
                            bail!(
                                "runtime.kind = {k:?} — daemon so sánh phân biệt hoa thường với \
                                 \"server\", nên app sẽ được cài mà không bao giờ chạy. \
                                 Dùng đúng \"server\"."
                            );
                        }
                        let start = rt
                            .get("start")
                            .context(
                                "senclaw-manifest.json: `runtime.kind` = \"server\" nhưng thiếu \
                                 `runtime.start` — daemon sẽ không khởi chạy gì cả",
                            )?
                            .as_str()
                            .context(
                                "senclaw-manifest.json: `runtime.start` phải là một chuỗi lệnh, \
                                 không phải mảng",
                            )?;
                        if start.trim().is_empty() {
                            bail!("senclaw-manifest.json: `runtime.start` rỗng");
                        }
                    }
                    None => warnings.push(
                        "manifest không có `runtime.kind` — daemon sẽ không coi đây là app có \
                         server và không khởi chạy gì"
                            .into(),
                    ),
                }

                // The silent-failure field: an unrecognised mode falls back to
                // `session`, so an app that must poll a channel just stops.
                if let Some(mode) = rt.get("mode") {
                    let mode = mode.as_str().context(
                        "senclaw-manifest.json: `runtime.mode` phải là chuỗi \
                         `background` hoặc `session`",
                    )?;
                    if crate::apps::manifest::RunMode::parse(mode).is_none() {
                        bail!(
                            "runtime.mode = {mode:?} không hợp lệ — daemon sẽ âm thầm coi là \
                             `session`. Dùng `background` hoặc `session`."
                        );
                    }
                }
                if let Some(runner) = rt.get("runner") {
                    let runner = runner
                        .as_str()
                        .context("senclaw-manifest.json: `runtime.runner` phải là chuỗi")?;
                    if crate::apps::manifest::Runner::parse(runner).is_none() {
                        bail!(
                            "runtime.runner = {runner:?} không hợp lệ — dùng \
                             binary | node | python | shell"
                        );
                    }
                }
                if let Some(port) = rt.get("port") {
                    let port = port.as_u64().context(
                        "senclaw-manifest.json: `runtime.port` phải là số không có \
                         dấu nháy — daemon đọc một chuỗi thành cổng 0",
                    )?;
                    // The daemon casts this `as u16`, which wraps silently:
                    // 70000 becomes 4464 — a different port, and one inside the
                    // band the bundled apps use. 65536 becomes 0.
                    if port > u16::MAX as u64 {
                        bail!(
                            "senclaw-manifest.json: `runtime.port` = {port} vượt 65535 — \
                             daemon cắt xuống u16 nên app sẽ nghe ở một cổng khác hẳn"
                        );
                    }
                    if let Some(want) = vars.get("port").and_then(|p| p.parse::<u64>().ok()) {
                        if port != want {
                            warnings.push(format!(
                                "manifest runtime.port = {port} nhưng cổng đã chọn là {want}"
                            ));
                        }
                    }
                }
            }

        }
        Kind::Skill => {
            let skill = find("SKILL.md")
                .context("template kiểu skill nhưng kết quả không có SKILL.md")?;
            let content = text_of(skill);
            let meta = crate::skills::metadata::parse_skill_metadata(&content, "", "");
            if meta.name.trim().is_empty() {
                bail!("SKILL.md thiếu `name:` trong frontmatter — skill sẽ không được nạp");
            }
            if meta.description.trim().is_empty() {
                warnings.push(
                    "SKILL.md chưa có `description:` — đó là dòng agent dùng để quyết định \
                     có gọi skill hay không"
                        .into(),
                );
            }
        }
        Kind::SubAgent => {
            let md = files
                .iter()
                .find(|f| f.rel.ends_with(".md"))
                .context("template kiểu sub-agent nhưng kết quả không có file .md nào")?;
            let content = text_of(md);
            let fm = crate::skills::metadata::extract_frontmatter(&content).context(
                "file persona thiếu frontmatter YAML (`---` … `---`) — \
                 PersonaRegistry sẽ bỏ qua nó",
            )?;
            if !fm.lines().any(|l| l.trim_start().starts_with("name:")) {
                bail!("frontmatter của persona thiếu `name:`");
            }
            if !fm.lines().any(|l| l.trim_start().starts_with("description:")) {
                warnings.push("frontmatter của persona chưa có `description:`".into());
            }
        }
    }

    Ok(warnings)
}

/// A Space App authenticates nothing of its own, so a wildcard bind hands its
/// entire REST + MCP surface to the LAN. Templates are copied between apps more
/// than anything else in this codebase — one bad copy re-exposes the fleet — so
/// this is an error, not a warning.
///
/// It runs on the template's **own source**, before substitution, not on the
/// rendered output. A `--desc "reverse proxy that never binds 0.0.0.0"` is user
/// data landing in a manifest string; it cannot bind anything, and failing the
/// create over it points the user at a rule they did not break.
///
/// Two shapes are caught. The literal (`"0.0.0.0"`, `[::]`) is the obvious one.
/// The one that actually ships is the *idiomatic* wildcard — a listen call with
/// no host at all — which contains no literal to grep for, so those forms are
/// matched by name.
pub fn check_bind_host(files: &[super::source::TemplateFile]) -> Result<Vec<String>> {
    let warnings = Vec::new();
    for f in files {
        // Prose talks *about* the rule — every template's README warns against
        // it — and prose cannot bind a socket.
        if is_prose(&f.rel) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&f.bytes) else {
            continue;
        };
        for (i, raw) in text.lines().enumerate() {
            let line = strip_comment(raw);
            let hit = if line.contains("0.0.0.0") || line.contains("[::]") {
                Some("bind 0.0.0.0")
            } else if is_hostless_listen(&line) {
                Some("listen không truyền host — mặc định là mọi interface")
            } else {
                None
            };
            let Some(what) = hit else { continue };
            // No exemption for lines mentioning SENCLAW_BIND_HOST. That looks
            // like it whitelists the documented opt-in comment, but it also
            // whitelists the one mistake this check exists for:
            // `env("SENCLAW_BIND_HOST") || "0.0.0.0"` — the env var read with a
            // wildcard *default*, which binds every interface out of the box.
            // Comments are stripped above, so a comment explaining the rule is
            // already safe.
            bail!(
                "{}:{} {what} — Space App không có xác thực riêng, làm vậy là mở \
                 toàn bộ REST + MCP ra LAN. Đọc host từ SENCLAW_BIND_HOST, mặc \
                 định 127.0.0.1, và truyền nó vào lời gọi listen.",
                f.rel,
                i + 1
            );
        }
    }
    Ok(warnings)
}

/// Drop the commented tail of a line.
///
/// Deliberately crude — it does not know about strings, so `"http://a#b"` loses
/// its fragment. That only ever makes the check *quieter*, and a comment is the
/// normal place to explain the rule, so over-refusing a documented line is the
/// worse failure.
fn strip_comment(line: &str) -> String {
    let mut out = line.to_string();
    for marker in ["//", "#", "/*", "*/", "<!--", "-->", "\"\"\""] {
        if let Some(pos) = out.find(marker) {
            out.truncate(pos);
        }
    }
    let trimmed = out.trim_start();
    // A block-comment continuation line (` * ...`) is all comment.
    if trimmed.starts_with('*') {
        return String::new();
    }
    out
}

/// The wildcard binds that carry no `0.0.0.0` to grep for — the forms CLAUDE.md
/// names as the real exposure, and the ones a template author writes by
/// reflex.
fn is_hostless_listen(line: &str) -> bool {
    let l = line.replace(' ', "");
    // Node: `server.listen(PORT, cb)` / `app.listen(PORT)` — the host argument
    // is what the rule is about, and omitting it binds every interface.
    if let Some(rest) = l.split(".listen(").nth(1) {
        let args = rest.split(')').next().unwrap_or("");
        let host_arg = args.split(',').nth(1).unwrap_or("");
        let looks_like_host = host_arg.contains("HOST")
            || host_arg.contains("host")
            || host_arg.contains("127.0.0.1")
            || host_arg.contains("localhost");
        if !looks_like_host {
            return true;
        }
    }
    // Go: `":" + port` / `":%d"` as the whole address.
    if l.contains("ListenAndServe") && (l.contains("\":\"+") || l.contains("\":%d\"")) {
        return true;
    }
    // Python: `(("", PORT))` — the empty host is INADDR_ANY.
    if l.contains("((\"\",") || l.contains("(('',") {
        return true;
    }
    // Next.js binds 0.0.0.0 unless -H is passed.
    if l.contains("nextstart") && !l.contains("-H") {
        return true;
    }
    false
}

/// Documentation, not something that runs.
fn is_prose(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    [".md", ".mdx", ".txt", ".rst", ".adoc"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

/// How much of the destination this create owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occupancy {
    /// The whole directory is the new thing (an app project). A non-empty
    /// destination means the user is about to lose track of what came from
    /// where, so it is refused.
    OwnsDirectory,
    /// The new thing is a file or two inside a directory that is *supposed* to
    /// hold many — the personas directory holds every persona on the machine.
    /// Refusing "directory is non-empty" there would mean the command works
    /// exactly once per machine, so only a collision on the specific files
    /// being written counts.
    SharesDirectory,
}

/// Write a rendered project to `dest`.
///
/// `force` overwrites whatever the relevant guard would have refused.
pub fn write_out(
    dest: &Path,
    files: &[OutFile],
    occupancy: Occupancy,
    force: bool,
) -> Result<()> {
    if !force {
        match occupancy {
            Occupancy::OwnsDirectory => {
                let non_empty = std::fs::read_dir(dest)
                    .map(|mut d| d.next().is_some())
                    .unwrap_or(false);
                if non_empty {
                    bail!(
                        "{} đã tồn tại và không rỗng. Dùng --force để ghi đè.",
                        dest.display()
                    );
                }
            }
            Occupancy::SharesDirectory => {
                if let Some(clash) = files.iter().find(|f| dest.join(&f.rel).exists()) {
                    bail!(
                        "{} đã tồn tại. Đổi tên, hoặc dùng --force để ghi đè.",
                        dest.join(&clash.rel).display()
                    );
                }
            }
        }
    }
    std::fs::create_dir_all(dest)
        .with_context(|| format!("không tạo được {}", dest.display()))?;

    for f in files {
        let path = dest.join(&f.rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("không tạo được {}", parent.display()))?;
        }
        std::fs::write(&path, &f.bytes)
            .with_context(|| format!("không ghi được {}", path.display()))?;
        if f.executable {
            set_executable(&path)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o755);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scaffold::source::TemplateFile;
    use crate::scaffold::spec::VarSpec;

    fn vars() -> Vars {
        let mut v = Vars::new();
        v.insert("id".into(), "todo".into());
        v.insert("port".into(), "4800".into());
        v
    }

    fn tpl(files: Vec<(&str, &str)>) -> LoadedTemplate {
        LoadedTemplate {
            spec: TemplateSpec::parse(r#"{"name":"t","kind":"app"}"#, "t").unwrap(),
            files: files
                .into_iter()
                .map(|(rel, body)| TemplateFile {
                    rel: rel.to_string(),
                    bytes: body.as_bytes().to_vec(),
                    executable: false,
                })
                .collect(),
            origin: "test".into(),
        }
    }

    fn out(rel: &str, body: &str) -> OutFile {
        OutFile {
            rel: rel.into(),
            bytes: body.as_bytes().to_vec(),
            executable: false,
        }
    }

    #[test]
    fn spec_defaults_can_reference_earlier_variables() {
        let spec = TemplateSpec::parse(
            r#"{"name":"t","variables":[{"name":"mcp_name","default":"{{id}}-mcp"}]}"#,
            "t",
        )
        .unwrap();
        let mut v = vars();
        apply_spec_vars(&spec, &mut v).unwrap();
        assert_eq!(v.get("mcp_name").unwrap(), "todo-mcp");
    }

    #[test]
    fn a_required_variable_with_no_answer_is_an_error() {
        let spec = TemplateSpec {
            variables: vec![VarSpec {
                name: "api_key".into(),
                description: Some("khoá API".into()),
                default: None,
                required: true,
            }],
            ..TemplateSpec::parse(r#"{"name":"t"}"#, "t").unwrap()
        };
        let err = apply_spec_vars(&spec, &mut vars()).unwrap_err().to_string();
        assert!(err.contains("--var api_key="), "{err}");
    }

    #[test]
    fn an_explicit_var_wins_over_the_template_default() {
        let spec = TemplateSpec::parse(
            r#"{"name":"t","variables":[{"name":"mcp_name","default":"{{id}}-mcp"}]}"#,
            "t",
        )
        .unwrap();
        let mut v = vars();
        v.insert("mcp_name".into(), "custom-mcp".into());
        apply_spec_vars(&spec, &mut v).unwrap();
        assert_eq!(v.get("mcp_name").unwrap(), "custom-mcp");
    }

    #[test]
    fn renders_paths_and_contents() {
        let t = tpl(vec![("src/{{id}}.rs", "// {{id}} on {{port}}")]);
        let (files, warnings) = render_template(&t, &vars()).unwrap();
        assert_eq!(files[0].rel, "src/todo.rs");
        assert_eq!(String::from_utf8_lossy(&files[0].bytes), "// todo on 4800");
        assert!(warnings.is_empty());
    }

    #[test]
    fn binary_files_are_copied_untouched() {
        let png = vec![0x89, b'P', b'N', b'G', 0xFF, 0xFE];
        let t = LoadedTemplate {
            spec: TemplateSpec::parse(r#"{"name":"t"}"#, "t").unwrap(),
            files: vec![TemplateFile {
                rel: "web/icon.png".into(),
                bytes: png.clone(),
                executable: false,
            }],
            origin: "test".into(),
        };
        let (files, _) = render_template(&t, &vars()).unwrap();
        assert_eq!(files[0].bytes, png);
    }

    #[test]
    fn colliding_destinations_are_a_template_bug_not_a_lost_file() {
        let t = tpl(vec![("a/{{id}}.rs", "1"), ("b/../a/{{id}}.rs", "2")]);
        // `..` is refused outright, so this collides only if traversal were
        // allowed — assert the traversal path is the one that is rejected.
        let (files, warnings) = render_template(&t, &vars()).unwrap();
        assert_eq!(files.len(), 1);
        assert!(warnings.iter().any(|w| w.contains("bỏ qua")));
    }

    #[test]
    fn unknown_placeholder_warns_but_still_renders() {
        let t = tpl(vec![("x.txt", "{{id}} {{app_i}}")]);
        let (files, warnings) = render_template(&t, &vars()).unwrap();
        assert_eq!(String::from_utf8_lossy(&files[0].bytes), "todo {{app_i}}");
        assert!(warnings[0].contains("app_i"), "{warnings:?}");
    }

    #[test]
    fn misspelled_run_mode_is_refused() {
        let files = vec![out(
            "senclaw-manifest.json",
            r#"{"id":"todo","runtime":{"mode":"backgroud"}}"#,
        )];
        let err = validate(Kind::App, &files, &vars()).unwrap_err().to_string();
        assert!(err.contains("runtime.mode"), "{err}");
        assert!(err.contains("session"), "{err}");
    }

    #[test]
    fn valid_modes_pass() {
        for mode in ["background", "session"] {
            let files = vec![out(
                "senclaw-manifest.json",
                &format!(r#"{{"id":"todo","runtime":{{"mode":"{mode}","port":4800}}}}"#),
            )];
            assert!(validate(Kind::App, &files, &vars()).is_ok(), "{mode}");
        }
    }

    fn src(rel: &str, body: &str) -> crate::scaffold::source::TemplateFile {
        crate::scaffold::source::TemplateFile {
            rel: rel.into(),
            bytes: body.as_bytes().to_vec(),
            executable: false,
        }
    }

    /// The mistake that matters is not `bind("0.0.0.0")` — nobody writes that
    /// by accident. It is reading the env var and *defaulting* to the wildcard,
    /// which looks like the documented pattern and binds every interface out of
    /// the box.
    #[test]
    fn the_env_var_with_a_wildcard_default_is_refused() {
        for line in [
            "const HOST = process.env.SENCLAW_BIND_HOST || '0.0.0.0';",
            "let host = env::var(\"SENCLAW_BIND_HOST\").unwrap_or_else(|_| \"0.0.0.0\".into());",
            "HOST = os.environ.get(\"SENCLAW_BIND_HOST\", \"0.0.0.0\")",
            "let l = bind(\"0.0.0.0:4800\");",
            "TcpListener::bind(\"[::]:4800\")",
        ] {
            let err = check_bind_host(&[src("src/main.rs", line)])
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("SENCLAW_BIND_HOST"),
                "phải chặn: {line} → {err}"
            );
        }
    }

    /// The wildcard binds that contain no `0.0.0.0` to grep for. These are the
    /// forms a template author writes by reflex, and the ones CLAUDE.md names.
    #[test]
    fn a_listen_with_no_host_argument_is_refused() {
        for line in [
            "server.listen(PORT, () => console.log('up'));",
            "app.listen(PORT);",
            "  \"start\": \"next start -p 4800\"",
            "server = ThreadingHTTPServer((\"\", PORT), Handler)",
            "log.Fatal(http.ListenAndServe(\":\"+port, mux))",
        ] {
            assert!(
                check_bind_host(&[src("server.mjs", line)]).is_err(),
                "phải chặn: {line}"
            );
        }
    }

    #[test]
    fn a_listen_that_passes_the_host_is_allowed() {
        for line in [
            "server.listen(PORT, HOST, () => {});",
            "server.listen(PORT, '127.0.0.1');",
            "app.listen(PORT, host);",
        ] {
            assert!(
                check_bind_host(&[src("server.mjs", line)]).is_ok(),
                "phải cho qua: {line}"
            );
        }
    }

    /// Every template documents this rule, in prose and in comments. Refusing
    /// the documentation of a rule is worse than not checking it at all.
    #[test]
    fn comments_and_prose_explaining_the_rule_are_allowed() {
        for (rel, body) in [
            ("src/main.rs", "// Set SENCLAW_BIND_HOST=0.0.0.0 to opt in."),
            (
                "src/main.rs",
                "let host = env(\"X\", \"127.0.0.1\"); // never 0.0.0.0",
            ),
            ("src/main.rs", "/* do not bind 0.0.0.0 here */"),
            ("server.mjs", " * 0.0.0.0 hands the surface to the LAN."),
            ("main.py", "# HOST defaults to 127.0.0.1, never 0.0.0.0"),
            ("web/index.html", "<!-- the app never binds 0.0.0.0 -->"),
            (
                "README.md",
                "**Đừng bind `0.0.0.0`.** Space App không có xác thực.",
            ),
        ] {
            assert!(
                check_bind_host(&[src(rel, body)]).is_ok(),
                "phải cho qua {rel}: {body}"
            );
        }
    }

    /// The check runs on the template's source, not the rendered output — so a
    /// description that merely mentions the address cannot fail the create.
    #[test]
    fn a_description_mentioning_the_address_is_not_a_bind() {
        let mut v = vars();
        v.insert(
            "description".into(),
            "Reverse proxy: never binds 0.0.0.0".into(),
        );
        let files = vec![out(
            "senclaw-manifest.json",
            r#"{"id":"todo","runtime":{"kind":"server","start":"./x","port":4800}}"#,
        )];
        assert!(
            validate(Kind::App, &files, &v).is_ok(),
            "mô tả của người dùng không phải mã nguồn"
        );
    }

    /// Every check reads its field with `as_str`/`as_u64`, which returns None
    /// for the wrong *type* exactly as it does for absent — so without an
    /// explicit type check a quoted port skips the port check and reaches the
    /// daemon, which parses it as 0.
    #[test]
    fn a_wrong_typed_runtime_block_is_refused_not_skipped() {
        for manifest in [
            r#"{"id":"todo","runtime":{"port":"4800"}}"#,
            r#"{"id":"todo","runtime":"background"}"#,
            r#"{"id":"todo","runtime":{"mode":["background"]}}"#,
            r#"{"id":"todo","runtime":{"runner":1}}"#,
        ] {
            let files = vec![out("senclaw-manifest.json", manifest)];
            assert!(
                validate(Kind::App, &files, &vars()).is_err(),
                "phải chặn: {manifest}"
            );
        }
    }

    /// `kind` and `start` decide whether the daemon launches the app at all,
    /// with a case-sensitive compare and no error path — so getting either
    /// wrong installs an app that appears in the list and does nothing.
    #[test]
    fn a_runtime_that_would_never_launch_is_refused() {
        for manifest in [
            r#"{"id":"todo","runtime":{"kind":"Server","start":"./x"}}"#,
            r#"{"id":"todo","runtime":{"kind":5,"start":"./x"}}"#,
            r#"{"id":"todo","runtime":{"kind":"server"}}"#,
            r#"{"id":"todo","runtime":{"kind":"server","start":["node","server.mjs"]}}"#,
            r#"{"id":"todo","runtime":{"kind":"server","start":"  "}}"#,
        ] {
            let files = vec![out("senclaw-manifest.json", manifest)];
            assert!(
                validate(Kind::App, &files, &vars()).is_err(),
                "phải chặn: {manifest}"
            );
        }
    }

    /// The daemon casts the port `as u16`, which wraps: 70000 becomes 4464 — a
    /// live port inside the band the bundled apps use.
    #[test]
    fn a_port_above_u16_is_refused_rather_than_silently_wrapped() {
        let mut v = vars();
        v.insert("port".into(), "70000".into());
        let files = vec![out(
            "senclaw-manifest.json",
            r#"{"id":"todo","runtime":{"kind":"server","start":"./x","port":70000}}"#,
        )];
        let err = validate(Kind::App, &files, &v).unwrap_err().to_string();
        assert!(err.contains("65535"), "{err}");
    }

    #[test]
    fn port_mismatch_warns_rather_than_blocking() {
        let files = vec![out(
            "senclaw-manifest.json",
            r#"{"id":"todo","runtime":{"port":9999}}"#,
        )];
        let w = validate(Kind::App, &files, &vars()).unwrap();
        assert!(w.iter().any(|x| x.contains("9999")), "{w:?}");
    }

    #[test]
    fn an_app_without_a_manifest_is_refused() {
        let files = vec![out("src/main.rs", "fn main() {}")];
        assert!(validate(Kind::App, &files, &vars()).is_err());
    }

    #[test]
    fn skill_without_a_name_is_refused() {
        let files = vec![out("SKILL.md", "no frontmatter here")];
        assert!(validate(Kind::Skill, &files, &vars()).is_err());
        let ok = vec![out("SKILL.md", "---\nname: x\ndescription: d\n---\nbody")];
        assert!(validate(Kind::Skill, &ok, &vars()).is_ok());
    }

    #[test]
    fn persona_without_frontmatter_is_refused() {
        let files = vec![out("todo.md", "# just a heading")];
        assert!(validate(Kind::SubAgent, &files, &vars()).is_err());
        let ok = vec![out("todo.md", "---\nname: todo\ndescription: d\n---\nbody")];
        assert!(validate(Kind::SubAgent, &ok, &vars()).is_ok());
    }

    #[test]
    fn refuses_a_non_empty_destination_without_force() {
        let td = tempfile::TempDir::new().unwrap();
        let dest = td.path().join("app");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("existing.txt"), "mine").unwrap();

        let files = vec![out("a.txt", "new")];
        assert!(write_out(&dest, &files, Occupancy::OwnsDirectory, false).is_err());
        write_out(&dest, &files, Occupancy::OwnsDirectory, true).unwrap();
        assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "new");
        assert!(
            dest.join("existing.txt").exists(),
            "--force ghi đè file trùng tên, không xoá cả thư mục"
        );
    }

    /// The personas directory holds every persona on the machine, so a
    /// "directory is non-empty" guard would make `create sub-agent` work
    /// exactly once and then fail forever.
    #[test]
    fn a_shared_directory_only_refuses_on_the_actual_file() {
        let td = tempfile::TempDir::new().unwrap();
        let dest = td.path().join("virtual-agents");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("someone-else.md"), "existing persona").unwrap();

        let files = vec![out("mine.md", "new persona")];
        write_out(&dest, &files, Occupancy::SharesDirectory, false)
            .expect("hàng xóm trong cùng thư mục không được chặn");
        assert!(dest.join("someone-else.md").exists());

        // Writing the same persona again *is* a collision.
        let err = write_out(&dest, &files, Occupancy::SharesDirectory, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("mine.md"), "{err}");
        write_out(&dest, &files, Occupancy::SharesDirectory, true).unwrap();
    }

    #[test]
    fn writes_nested_paths_and_keeps_the_exec_bit() {
        let td = tempfile::TempDir::new().unwrap();
        let dest = td.path().join("app");
        let files = vec![OutFile {
            rel: "scripts/pack.sh".into(),
            bytes: b"#!/bin/sh\n".to_vec(),
            executable: true,
        }];
        write_out(&dest, &files, Occupancy::OwnsDirectory, false).unwrap();
        let p = dest.join("scripts/pack.sh");
        assert!(p.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode();
            assert!(mode & 0o111 != 0, "pack.sh phải giữ quyền thực thi");
        }
    }
}
