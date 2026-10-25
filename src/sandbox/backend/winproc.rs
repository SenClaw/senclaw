//! The parts of the Windows `direct` backend that are plain string and path
//! logic — compiled on every OS so they are unit-tested on a developer's Mac
//! or Linux box, where `direct_windows.rs` cannot even be built.
//!
//! Everything here is a function of its arguments: the host environment is
//! passed in as a lookup, never read from `std::env`, so a test can describe a
//! machine without having one.

use std::collections::BTreeMap;

/// One argument, quoted so that `CommandLineToArgvW` (and the C runtime of every
/// ordinary program) reads back exactly this string.
///
/// The easy version — wrap in quotes, escape `"` — is wrong for a backslash
/// before a quote, and for a trailing backslash: `C:\dir\` becomes `"C:\dir\"`,
/// where the last `\"` is an escaped quote that swallows the rest of the line.
pub fn quote_arg(arg: &str) -> String {
    let needs_quotes = arg.is_empty() || arg.chars().any(|c| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '"'));
    if !needs_quotes {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                // Backslashes before a quote are doubled, then the quote itself
                // is escaped.
                out.extend(std::iter::repeat('\\').take(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat('\\').take(backslashes));
                backslashes = 0;
                out.push(c);
            }
        }
    }
    // Backslashes before the closing quote are doubled so they stay literal.
    out.extend(std::iter::repeat('\\').take(backslashes * 2));
    out.push('"');
    out
}

/// A whole command line from an argv. The program is quoted like any argument,
/// which is what `CreateProcessW` needs when its path contains a space.
pub fn command_line(argv: &[String]) -> String {
    argv.iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" ")
}

/// A UTF-16 environment block: `NAME=value\0…\0\0`.
///
/// `CreateProcessW` documents that the block must be sorted by name,
/// case-insensitively, and Windows names are case-insensitive — so `Path` and
/// `PATH` are one variable, and the later entry in `vars` wins.
pub fn env_block(vars: &[(String, String)]) -> Vec<u16> {
    let mut by_name: BTreeMap<String, (&str, &str)> = BTreeMap::new();
    for (k, v) in vars {
        // An empty name, or one with `=`, would corrupt the block (`=C:` style
        // names are the OS's own and are never ours to set).
        if k.is_empty() || k.contains('=') || k.contains('\0') {
            continue;
        }
        by_name.insert(k.to_uppercase(), (k.as_str(), v.as_str()));
    }
    // The map is keyed by the uppercased name only to collapse spellings; the
    // order the OS asks for is by UTF-16 code unit of that uppercased name.
    let mut ordered: Vec<(&str, &str)> = by_name.into_values().collect();
    ordered.sort_by(|a, b| a.0.to_uppercase().encode_utf16().cmp(b.0.to_uppercase().encode_utf16()));

    let mut out: Vec<u16> = Vec::new();
    for (k, v) in ordered {
        out.extend(k.encode_utf16());
        out.push(b'=' as u16);
        out.extend(v.chars().filter(|c| *c != '\0').collect::<String>().encode_utf16());
        out.push(0);
    }
    if out.is_empty() {
        // An empty block is still two terminators.
        out.push(0);
    }
    out.push(0);
    out
}

/// Variables copied from the host when the sandbox has not set them. All are
/// system facts rather than anything personal; none can hold a credential.
///
/// `SystemRoot` is the one that matters most: Python 3 refuses to start without
/// it ("failed to get random numbers to initialize Python"), because it cannot
/// find the DLL that seeds its hash randomisation.
const HOST_SYSTEM_VARS: &[&str] = &[
    "SystemRoot",
    "windir",
    "SystemDrive",
    "ComSpec",
    "PATHEXT",
    "OS",
    "PROCESSOR_ARCHITECTURE",
    "PROCESSOR_ARCHITEW6432",
    "NUMBER_OF_PROCESSORS",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "CommonProgramFiles",
    "CommonProgramFiles(x86)",
    "CommonProgramW6432",
    "ProgramData",
    "ALLUSERSPROFILE",
    "PUBLIC",
];

fn has(env: &[(String, String)], name: &str) -> bool {
    env.iter().any(|(k, _)| k.eq_ignore_ascii_case(name))
}

/// `home` plus a relative piece, with Windows separators.
fn under(home: &str, rel: &str) -> String {
    format!("{}\\{}", home.trim_end_matches(['\\', '/']), rel.replace('/', "\\"))
}

/// Turn the Unix-shaped environment `build_env` produces into one a Windows
/// program can start with.
///
/// * the system variables a process cannot start without (`SystemRoot`, …) come
///   from the host;
/// * `TEMP`, `TMP`, `USERPROFILE`, `APPDATA`, `LOCALAPPDATA` point inside the
///   sandbox, so the tools that write caches there land in the one place the
///   container may write — and never in the real profile;
/// * Python is told to speak UTF-8: through a pipe it would otherwise use the
///   ANSI code page and garble every non-ASCII character of the output.
///
/// Anything the sandbox or the caller already set is left alone.
pub fn windows_env(
    mut env: Vec<(String, String)>,
    home: &str,
    host: &dyn Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    // `build_env` joins with `/`; a Windows tool wants backslashes.
    for (k, v) in env.iter_mut() {
        if k.eq_ignore_ascii_case("HOME") || k.eq_ignore_ascii_case("TMPDIR") {
            *v = v.replace('/', "\\");
        }
    }
    for name in HOST_SYSTEM_VARS {
        if !has(&env, name) {
            if let Some(v) = host(name).filter(|v| !v.is_empty()) {
                env.push((name.to_string(), v));
            }
        }
    }
    if !has(&env, "SystemRoot") {
        env.push(("SystemRoot".into(), r"C:\Windows".into()));
    }
    let defaults = [
        ("TEMP", under(home, ".tmp")),
        ("TMP", under(home, ".tmp")),
        ("USERPROFILE", home.replace('/', "\\")),
        ("APPDATA", under(home, "AppData/Roaming")),
        ("LOCALAPPDATA", under(home, "AppData/Local")),
        ("PYTHONUTF8", "1".to_string()),
        ("PYTHONIOENCODING", "utf-8".to_string()),
    ];
    for (k, v) in defaults {
        if !has(&env, k) {
            env.push((k.to_string(), v));
        }
    }
    env
}

/// Longest `LOCALAPPDATA` handed to an AppContainer child.
///
/// Windows appends `\Packages\<moniker>\AC` (~47 characters) to it when the
/// container's first process starts. Measured on Windows 11: from 209 characters
/// on, a console program that `cmd.exe` then starts never gets past creation
/// (its thread stays suspended and `cmd` spins), so every shell command of a
/// sandbox ~195 characters deep timed out. The limit keeps a margin below that.
pub const MAX_LOCAL_APPDATA: usize = 180;

/// Point `LOCALAPPDATA` at `fallback` when it is longer than
/// [`MAX_LOCAL_APPDATA`]. Returns whether it changed.
pub fn shorten_local_appdata(env: &mut [(String, String)], fallback: &str) -> bool {
    match env.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case("LOCALAPPDATA")) {
        Some((_, v)) if v.len() > MAX_LOCAL_APPDATA => {
            *v = fallback.to_string();
            true
        }
        _ => false,
    }
}

/// Let Node.js run inside an AppContainer.
///
/// Node resolves every module through a JavaScript `realpath` that `lstat`s the
/// drive root, and an AppContainer may not read `C:\` — so no script ran, `npm`
/// included (`EPERM: operation not permitted, lstat 'C:\'`). Preserving symlinks
/// skips that walk. The cost: packages that are symlinks (`pnpm`'s layout,
/// `npm link`) resolve by their link path. A value the sandbox already set wins.
pub fn node_without_realpath(env: &mut Vec<(String, String)>) {
    if !has(env, "NODE_PRESERVE_SYMLINKS") {
        env.push(("NODE_PRESERVE_SYMLINKS".into(), "1".into()));
    }
    match env.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case("NODE_OPTIONS")) {
        Some((_, v)) if !v.contains("--preserve-symlinks-main") => {
            *v = format!("{} --preserve-symlinks-main", v.trim()).trim_start().to_string();
        }
        Some(_) => {}
        None => env.push(("NODE_OPTIONS".into(), "--preserve-symlinks-main".into())),
    }
}

/// How many runs of one sandbox can be found and stopped from another process.
pub const MAX_NAMED_RUNS: u32 = 32;

/// The name of a run's Job Object, so another process of this user — the MCP
/// server, when the run belongs to the daemon — can open it and stop it.
/// `slot` is the first one free when the run starts.
pub fn job_name(sandbox_id: &str, slot: u32) -> String {
    let id: String = sandbox_id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    format!(r"Local\SenClawSbxJob-{id}-{slot}")
}

/// A duration as `ps` writes `etime`: `MM:SS`, `HH:MM:SS`, `D-HH:MM:SS`.
pub fn ps_elapsed(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    if d > 0 {
        format!("{d}-{h:02}:{m:02}:{s:02}")
    } else if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// CPU percent over a sampling window, as `ps` reports it (one full core is 100).
pub fn cpu_percent(cpu_100ns: u64, window_100ns: u64) -> f64 {
    if window_100ns == 0 {
        return 0.0;
    }
    (cpu_100ns as f64 / window_100ns as f64 * 1000.0).round() / 10.0
}

/// `cmd.exe`'s own commands: always run before anything on PATH of that name.
const CMD_INTERNAL: &[&str] = &[
    "assoc", "break", "call", "cd", "chdir", "cls", "color", "copy", "date", "del", "dir", "echo",
    "endlocal", "erase", "exit", "for", "ftype", "goto", "if", "md", "mkdir", "mklink", "move", "path",
    "pause", "popd", "prompt", "pushd", "rd", "rem", "ren", "rename", "rmdir", "set", "setlocal",
    "shift", "start", "time", "title", "type", "ver", "verify", "vol",
];

/// Does `token`, the first word of a command, start another batch file the way
/// `cmd.exe` would look it up: its own commands first, then the current folder
/// and each PATH folder in turn, trying every `PATHEXT` extension in a folder
/// before moving on to the next?
pub fn runs_a_batch_file(
    token: &str,
    cwd: &str,
    path: &str,
    pathext: &str,
    exists: &dyn Fn(&str) -> bool,
) -> bool {
    let token = token.trim_matches('"');
    if token.is_empty() || token.contains(['*', '?']) {
        return false;
    }
    let lower = token.to_ascii_lowercase();
    if CMD_INTERNAL.contains(&lower.as_str()) {
        return false;
    }
    let is_batch = |name: &str| {
        let n = name.to_ascii_lowercase();
        n.ends_with(".cmd") || n.ends_with(".bat")
    };
    let file_name = lower.rsplit(['\\', '/']).next().unwrap_or(&lower);
    if file_name.contains('.') {
        // An extension of its own: decided by it, whether or not it exists yet.
        return is_batch(file_name);
    }
    let exts: Vec<&str> = pathext.split(';').map(str::trim).filter(|e| !e.is_empty()).collect();
    let has_dir = token.contains(['\\', '/', ':']);
    let dirs: Vec<String> = if has_dir {
        vec![String::new()]
    } else {
        std::iter::once(cwd.to_string())
            .chain(path.split(';').map(str::trim).filter(|d| !d.is_empty()).map(str::to_string))
            .collect()
    };
    for dir in dirs {
        let base = if dir.is_empty() {
            if is_absolute(token) { token.to_string() } else { under(cwd, token) }
        } else {
            under(&dir, token)
        };
        for ext in &exts {
            let candidate = format!("{base}{ext}");
            if exists(&candidate) {
                return is_batch(&candidate);
            }
        }
    }
    false
}

fn is_absolute(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with(r"\\") || (b.len() > 2 && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
}

/// Put `call` before every command of a script that starts another batch file.
///
/// A batch file that names another one without `call` hands control to it for
/// good: `npm.cmd` is a batch file, so in a script of several lines nothing
/// after the first `npm …` ran. A command counts where `cmd.exe` starts one: at
/// the start of a line, after `&&`, `||`, `&` and `|`, inside an `if (…)`
/// block, after a `for … do` and after `else`. Quoted text and `^`-escaped
/// characters are left alone. A command after an `if` condition without
/// parentheses is not recognised.
pub fn call_batch_commands(script: &str, is_batch: &dyn Fn(&str) -> bool) -> String {
    script
        .split('\n')
        .map(|line| call_in_line(line.strip_suffix('\r').unwrap_or(line), is_batch))
        .collect::<Vec<_>>()
        .join("\n")
}

fn call_in_line(line: &str, is_batch: &dyn Fn(&str) -> bool) -> String {
    let chars: Vec<char> = line.chars().collect();
    let len = chars.len();
    let mut out = String::with_capacity(line.len() + 8);
    let mut i = 0;
    let mut at_start = true;
    let mut first_word = String::new();
    let mut saw_if = false;
    // Inside a parenthesized block every line starts a command; a line that is
    // only `)` or `) else (` is handled by the same rules.
    while i < len {
        if at_start {
            while i < len && (chars[i].is_whitespace() || chars[i] == '@' || chars[i] == '(' || chars[i] == ')') {
                out.push(chars[i]);
                i += 1;
            }
            if i >= len {
                break;
            }
            let end = token_end(&chars, i);
            let token: String = chars[i..end].iter().collect();
            first_word = token.to_ascii_lowercase();
            saw_if |= first_word == "if";
            if first_word == "else" {
                out.push_str(&token);
                i = end;
                continue;
            }
            if first_word != "call" && is_batch(&token) {
                out.push_str("call ");
            }
            out.push_str(&token);
            i = end;
            at_start = false;
            continue;
        }
        let c = chars[i];
        match c {
            '^' => {
                out.push(c);
                if i + 1 < len {
                    out.push(chars[i + 1]);
                }
                i += 2;
            }
            '"' => {
                out.push(c);
                i += 1;
                while i < len {
                    out.push(chars[i]);
                    i += 1;
                    if chars[i - 1] == '"' {
                        break;
                    }
                }
            }
            '&' | '|' => {
                out.push(c);
                i += 1;
                if i < len && chars[i] == c {
                    out.push(c);
                    i += 1;
                }
                at_start = true;
            }
            '(' if first_word == "if" => {
                out.push(c);
                i += 1;
                at_start = true;
            }
            _ if (c == 'd' || c == 'D' || c == 'e' || c == 'E') && i > 0 && (chars[i - 1].is_whitespace() || chars[i - 1] == ')') => {
                let end = token_end(&chars, i);
                let word: String = chars[i..end].iter().collect::<String>().to_ascii_lowercase();
                let starts_command = (word == "do" && first_word == "for") || (word == "else" && saw_if);
                out.extend(&chars[i..end]);
                i = end;
                if starts_command {
                    at_start = true;
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Where the word starting at `i` ends: a quoted run, or up to whitespace or an
/// operator.
fn token_end(chars: &[char], mut i: usize) -> usize {
    if chars.get(i) == Some(&'"') {
        i += 1;
        while i < chars.len() && chars[i] != '"' {
            i += 1;
        }
        return (i + 1).min(chars.len());
    }
    while i < chars.len() && !chars[i].is_whitespace() && !matches!(chars[i], '&' | '|' | '<' | '>' | '(' | ')') {
        i += 1;
    }
    i
}

/// The text of the `.cmd` file a shell command is written to.
///
/// `chcp 65001` first: `cmd.exe` writes its own output in the console's OEM code
/// page, so without it a Vietnamese error message reaches the daemon as bytes
/// that are not UTF-8. Line endings are normalised to CRLF whatever came in —
/// converting each `\n` blindly turns an existing `\r\n` into `\r\r\n`.
pub fn cmd_script(script: &str) -> String {
    let body = script.replace("\r\n", "\n").replace('\n', "\r\n");
    format!("@echo off\r\nchcp 65001 >nul\r\n{body}\r\n")
}

/// A path relative to the sandbox directory, with the separators `cmd.exe`
/// expects: it reads a `/` as the start of a switch, so `.runs/x.cmd` is the
/// command `.runs` with an argument.
pub fn windows_rel(rel: &str) -> String {
    rel.replace('/', "\\")
}

/// `\\?\C:\dir` → `C:\dir`, `\\?\UNC\srv\share` → `\\srv\share`.
///
/// `Path::canonicalize` returns the verbatim form on Windows, and not every
/// security API accepts it. Anything else is returned unchanged.
pub fn strip_verbatim(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        let b = rest.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            return rest.to_string();
        }
    }
    path.to_string()
}

/// A path as a comparison key: verbatim prefix gone, `/` as `\`, no trailing
/// separator, lower case (NTFS is case-insensitive).
pub fn path_key(path: &str) -> String {
    let p = strip_verbatim(path).replace('/', "\\");
    let p = p.trim_end_matches('\\');
    // `C:` alone is a drive root; keep it distinguishable from `C:\dir`.
    p.to_lowercase()
}

/// `path` is `root` or something inside it.
pub fn is_within(path: &str, root: &str) -> bool {
    let (p, r) = (path_key(path), path_key(root));
    !r.is_empty() && (p == r || p.starts_with(&format!("{r}\\")))
}

/// `path` is inside a Windows system directory (`%SystemRoot%`, `%windir%`,
/// i.e. `C:\Windows` and everything under it).
///
/// Trying to modify the DACL of a system directory requires elevation; the
/// `ALL APPLICATION PACKAGES` SID already grants AppContainers read/execute
/// there, so the grant can be silently skipped.
pub fn is_windows_dir(path: &str, host: &dyn Fn(&str) -> Option<String>) -> bool {
    for var in ["SystemRoot", "windir"] {
        if let Some(v) = host(var).filter(|v| !v.is_empty()) {
            if is_within(path, &v) {
                return true;
            }
        }
    }
    // Fallback when neither env var is readable (tests, cross-compilation).
    is_within(path, r"C:\Windows")
}

/// A drive root (`C:\`) or a bare UNC share root.
fn is_drive_root(key: &str) -> bool {
    let b = key.as_bytes();
    (b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':') || (key.starts_with(r"\\") && key[2..].matches('\\').count() <= 1)
}

/// Folders inside a user profile that hold credentials or browser state.
const PROFILE_SECRET_TREES: &[&str] = &[
    ".ssh",
    ".aws",
    ".azure",
    ".gnupg",
    ".kube",
    ".docker",
    ".config/gcloud",
    ".senclaw",
    "AppData/Roaming/Microsoft",
    "AppData/Local/Microsoft",
    "AppData/Local/Google",
    "AppData/Local/BraveSoftware",
    "AppData/Local/Mozilla",
    "AppData/Roaming/Mozilla",
    "AppData/Roaming/gcloud",
];

/// Why a Windows folder may not be the source of a mount, or `None` when it may.
///
/// A mount makes the container's SID an owner-grade principal on the folder, and
/// the ACE is inherited by everything under it — on a system folder or a whole
/// profile that is both unsafe and a rewrite of thousands of ACLs. The Unix list
/// (`/etc`, `$HOME`, …) names nothing that exists on Windows, so it needs its
/// own.
pub fn forbidden_mount_reason(real: &str, host: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let key = path_key(real);
    if is_drive_root(&key) {
        return Some(format!("`{}` is a whole drive — pick a specific subfolder", strip_verbatim(real)));
    }

    // Folders refused with everything inside them.
    let mut trees: Vec<String> = Vec::new();
    for var in ["SystemRoot", "windir", "ProgramFiles", "ProgramFiles(x86)", "ProgramW6432", "ProgramData"] {
        if let Some(v) = host(var).filter(|v| !v.is_empty()) {
            trees.push(v);
        }
    }
    for tree in &trees {
        if is_within(real, tree) {
            return Some(format!("it sits inside `{tree}`, a system folder"));
        }
    }

    // Refused as such, but a subfolder is fine: the profile and the folders
    // that hold every profile.
    let mut exact: Vec<String> = Vec::new();
    if let Some(drive) = host("SystemDrive").filter(|v| !v.is_empty()) {
        exact.push(format!("{}\\Users", drive.trim_end_matches('\\')));
    }
    if let Some(profile) = host("USERPROFILE").filter(|v| !v.is_empty()) {
        for secret in PROFILE_SECRET_TREES {
            let t = under(&profile, secret);
            if is_within(real, &t) {
                return Some(format!("it sits inside `{t}`, which holds credentials or browser data"));
            }
        }
        exact.push(profile.clone());
        exact.push(under(&profile, "AppData"));
        exact.push(under(&profile, "AppData/Local"));
        exact.push(under(&profile, "AppData/Roaming"));
    }
    for e in &exact {
        if path_key(real) == path_key(e) {
            return Some(format!("mounting `{e}` itself is not allowed — pick a specific subfolder"));
        }
    }
    None
}

/// The machine's Microsoft Store "app execution alias": a zero-byte reparse point
/// in `…\Microsoft\WindowsApps` named `python.exe`. It is on PATH on a stock
/// Windows 10/11, it opens the Store instead of running Python, and it cannot be
/// launched from an AppContainer at all.
pub fn is_store_stub(path: &str, len: u64) -> bool {
    len == 0 && path_key(path).contains(r"\microsoft\windowsapps\")
}

/// The `home =` line of a virtualenv's `pyvenv.cfg`: the interpreter the venv's
/// `python.exe` forwards to. The container needs that folder too.
pub fn venv_home(cfg: &str) -> Option<String> {
    cfg.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim().eq_ignore_ascii_case("home")).then(|| v.trim().to_string()).filter(|v| !v.is_empty())
    })
}

/// Milliseconds for `WaitForSingleObject`: never `INFINITE` (`u32::MAX`), and
/// never a wrapped value — `timeout_ms as u32` turns 2^32 + 5 into 5.
pub fn wait_millis(timeout_ms: u64) -> u32 {
    timeout_ms.min(u32::MAX as u64 - 1) as u32
}

/// Job Object limits for a sandbox: (bytes per process, active processes).
pub fn job_limits(memory_mb: i64, pids: i64) -> (usize, u32) {
    let mb = memory_mb.max(64) as usize;
    (mb.saturating_mul(1024 * 1024), pids.clamp(16, 8192) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k: &str| pairs.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.to_string())
    }

    const HOST: &[(&str, &str)] = &[
        ("SystemRoot", r"C:\Windows"),
        ("windir", r"C:\Windows"),
        ("SystemDrive", "C:"),
        ("ComSpec", r"C:\Windows\System32\cmd.exe"),
        ("PATHEXT", ".COM;.EXE;.BAT;.CMD"),
        ("ProgramFiles", r"C:\Program Files"),
        ("ProgramData", r"C:\ProgramData"),
        ("USERPROFILE", r"C:\Users\jane"),
        ("GITHUB_TOKEN", "must-not-be-copied"),
    ];

    #[test]
    fn plain_arguments_are_left_alone() {
        assert_eq!(quote_arg("python"), "python");
        assert_eq!(quote_arg(r"C:\Python312\python.exe"), r"C:\Python312\python.exe");
        assert_eq!(quote_arg("--flag=1"), "--flag=1");
    }

    #[test]
    fn spaces_and_the_empty_string_are_quoted() {
        assert_eq!(quote_arg("a b"), r#""a b""#);
        assert_eq!(quote_arg(""), r#""""#);
        assert_eq!(quote_arg("a\tb"), "\"a\tb\"");
    }

    #[test]
    fn a_trailing_backslash_does_not_eat_the_closing_quote() {
        // The classic bug: `"C:\my dir\"` reads the last `\"` as a literal quote.
        assert_eq!(quote_arg(r"C:\my dir\"), r#""C:\my dir\\""#);
        assert_eq!(quote_arg(r"C:\my dir\\"), r#""C:\my dir\\\\""#);
    }

    #[test]
    fn quotes_inside_an_argument_are_escaped_with_their_backslashes_doubled() {
        assert_eq!(quote_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_arg(r#"a\"b"#), r#""a\\\"b""#);
    }

    /// The inverse of the quoting, written the way `CommandLineToArgvW` reads a
    /// line, so the round trip is checked against the rule rather than against
    /// the expected strings above.
    fn split(line: &str) -> Vec<String> {
        let chars: Vec<char> = line.chars().collect();
        let (mut i, mut args) = (0, Vec::new());
        while i < chars.len() {
            while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
                i += 1;
            }
            if i >= chars.len() {
                break;
            }
            let (mut cur, mut in_q) = (String::new(), false);
            while i < chars.len() && (in_q || (chars[i] != ' ' && chars[i] != '\t')) {
                let mut bs = 0;
                while i < chars.len() && chars[i] == '\\' {
                    bs += 1;
                    i += 1;
                }
                if i < chars.len() && chars[i] == '"' {
                    cur.extend(std::iter::repeat('\\').take(bs / 2));
                    if bs % 2 == 1 {
                        cur.push('"');
                    } else {
                        in_q = !in_q;
                    }
                    i += 1;
                } else {
                    cur.extend(std::iter::repeat('\\').take(bs));
                    // Backslashes not followed by a quote are literal; the
                    // character after them is read on the next turn, so it can
                    // still be the separator that ends this argument.
                    if bs == 0 && i < chars.len() {
                        cur.push(chars[i]);
                        i += 1;
                    }
                }
            }
            args.push(cur);
        }
        args
    }

    #[test]
    fn a_command_line_reads_back_as_the_same_argv() {
        let argv: Vec<String> = [
            r"C:\Program Files\Python 3\python.exe",
            r".runs\a b.py",
            r#"he said "no""#,
            r"C:\dir\",
            "",
            r"back\\slash\\",
            "plain",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(split(&command_line(&argv)), argv);
    }

    fn decode(block: &[u16]) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = Vec::new();
        for &u in block {
            if u == 0 {
                if cur.is_empty() {
                    break;
                }
                out.push(String::from_utf16(&cur).unwrap());
                cur.clear();
            } else {
                cur.push(u);
            }
        }
        out
    }

    #[test]
    fn the_environment_block_is_sorted_case_insensitively_and_double_terminated() {
        let block = env_block(&[
            ("zeta".into(), "1".into()),
            ("Alpha".into(), "2".into()),
            ("path".into(), "x".into()),
        ]);
        assert_eq!(decode(&block), vec!["Alpha=2", "path=x", "zeta=1"]);
        assert_eq!(&block[block.len() - 2..], &[0, 0]);
    }

    #[test]
    fn two_spellings_of_one_variable_collapse_to_the_later() {
        let block = env_block(&[("Path".into(), "old".into()), ("PATH".into(), "new".into())]);
        assert_eq!(decode(&block), vec!["PATH=new"]);
    }

    #[test]
    fn names_that_would_corrupt_the_block_are_dropped() {
        let block = env_block(&[
            ("".into(), "x".into()),
            ("A=B".into(), "x".into()),
            ("OK".into(), "va\0lue".into()),
        ]);
        assert_eq!(decode(&block), vec!["OK=value"]);
    }

    #[test]
    fn an_empty_environment_is_still_a_valid_block() {
        assert_eq!(env_block(&[]), vec![0, 0]);
    }

    fn get<'a>(env: &'a [(String, String)], k: &str) -> Option<&'a str> {
        env.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
    }

    #[test]
    fn python_can_start_because_systemroot_is_carried_over() {
        let env = windows_env(Vec::new(), r"C:\sb\one", &host(HOST));
        assert_eq!(get(&env, "SystemRoot"), Some(r"C:\Windows"));
        assert_eq!(get(&env, "ComSpec"), Some(r"C:\Windows\System32\cmd.exe"));
        assert_eq!(get(&env, "PATHEXT"), Some(".COM;.EXE;.BAT;.CMD"));
    }

    #[test]
    fn systemroot_has_a_fallback_when_the_host_reports_none() {
        let env = windows_env(Vec::new(), r"C:\sb", &|_: &str| None);
        assert_eq!(get(&env, "SystemRoot"), Some(r"C:\Windows"));
    }

    #[test]
    fn caches_and_temp_files_land_inside_the_sandbox() {
        let env = windows_env(Vec::new(), r"C:\sb\one", &host(HOST));
        assert_eq!(get(&env, "TEMP"), Some(r"C:\sb\one\.tmp"));
        assert_eq!(get(&env, "TMP"), Some(r"C:\sb\one\.tmp"));
        assert_eq!(get(&env, "USERPROFILE"), Some(r"C:\sb\one"));
        assert_eq!(get(&env, "APPDATA"), Some(r"C:\sb\one\AppData\Roaming"));
        assert_eq!(get(&env, "LOCALAPPDATA"), Some(r"C:\sb\one\AppData\Local"));
    }

    #[test]
    fn a_trailing_separator_on_home_does_not_double_up() {
        let env = windows_env(Vec::new(), r"C:\sb\one\", &host(HOST));
        assert_eq!(get(&env, "TEMP"), Some(r"C:\sb\one\.tmp"));
    }

    #[test]
    fn python_is_told_to_speak_utf8() {
        let env = windows_env(Vec::new(), r"C:\sb", &host(HOST));
        assert_eq!(get(&env, "PYTHONUTF8"), Some("1"));
        assert_eq!(get(&env, "PYTHONIOENCODING"), Some("utf-8"));
    }

    #[test]
    fn what_the_sandbox_already_set_wins() {
        let env = windows_env(
            vec![("TEMP".into(), r"D:\mine".into()), ("pythonutf8".into(), "0".into())],
            r"C:\sb",
            &host(HOST),
        );
        assert_eq!(get(&env, "TEMP"), Some(r"D:\mine"));
        assert_eq!(get(&env, "PYTHONUTF8"), Some("0"));
        assert_eq!(env.iter().filter(|(k, _)| k.eq_ignore_ascii_case("pythonutf8")).count(), 1);
    }

    #[test]
    fn unix_style_paths_from_build_env_get_backslashes() {
        let env = windows_env(
            vec![("HOME".into(), r"C:\sb\one".into()), ("TMPDIR".into(), r"C:\sb\one/.tmp".into())],
            r"C:\sb\one",
            &host(HOST),
        );
        assert_eq!(get(&env, "TMPDIR"), Some(r"C:\sb\one\.tmp"));
    }

    #[test]
    fn host_secrets_are_not_copied() {
        let env = windows_env(Vec::new(), r"C:\sb", &host(HOST));
        assert!(get(&env, "GITHUB_TOKEN").is_none(), "only the named system variables cross over");
    }

    #[test]
    fn a_cmd_script_switches_to_utf8_and_uses_crlf() {
        let s = cmd_script("echo a\necho b");
        assert_eq!(s, "@echo off\r\nchcp 65001 >nul\r\necho a\r\necho b\r\n");
    }

    #[test]
    fn an_existing_crlf_is_not_doubled() {
        let s = cmd_script("echo a\r\necho b\r\n");
        assert!(!s.contains("\r\r"), "{s:?}");
    }

    #[test]
    fn a_relative_path_for_cmd_has_no_forward_slash() {
        assert_eq!(windows_rel(".runs/abc.cmd"), r".runs\abc.cmd");
    }

    #[test]
    fn the_verbatim_prefix_is_removed_only_when_it_is_safe() {
        assert_eq!(strip_verbatim(r"\\?\C:\Users\jane"), r"C:\Users\jane");
        assert_eq!(strip_verbatim(r"\\?\UNC\srv\share\x"), r"\\srv\share\x");
        assert_eq!(strip_verbatim(r"C:\plain"), r"C:\plain");
        // A device path is not a drive path and stays as it is.
        assert_eq!(strip_verbatim(r"\\?\GLOBALROOT\Device\X"), r"\\?\GLOBALROOT\Device\X");
    }

    #[test]
    fn containment_ignores_case_separators_and_the_verbatim_prefix() {
        assert!(is_within(r"\\?\C:\Users\Jane\proj", r"c:/users/jane"));
        assert!(is_within(r"C:\Users\jane", r"C:\Users\jane\"));
        assert!(!is_within(r"C:\Users\janet", r"C:\Users\jane"), "a longer sibling name is not inside");
        assert!(!is_within(r"C:\Users", r"C:\Users\jane"));
    }

    #[test]
    fn a_whole_drive_cannot_be_mounted() {
        assert!(forbidden_mount_reason(r"C:\", &host(HOST)).is_some());
        assert!(forbidden_mount_reason(r"\\?\D:\", &host(HOST)).is_some());
    }

    #[test]
    fn system_folders_cannot_be_mounted_or_entered() {
        for p in [r"C:\Windows", r"C:\Windows\System32", r"C:\Program Files\Git", r"C:\ProgramData\Microsoft"] {
            assert!(forbidden_mount_reason(p, &host(HOST)).is_some(), "{p}");
        }
    }

    #[test]
    fn the_profile_itself_is_refused_but_a_project_inside_it_is_not() {
        assert!(forbidden_mount_reason(r"C:\Users\jane", &host(HOST)).is_some());
        assert!(forbidden_mount_reason(r"C:\Users", &host(HOST)).is_some());
        assert!(forbidden_mount_reason(r"C:\Users\jane\AppData\Local", &host(HOST)).is_some());
        assert!(forbidden_mount_reason(r"C:\Users\jane\projects\site", &host(HOST)).is_none());
        assert!(forbidden_mount_reason(r"D:\work\site", &host(HOST)).is_none());
    }

    #[test]
    fn credential_folders_are_refused_with_everything_inside() {
        for p in [
            r"C:\Users\jane\.ssh",
            r"C:\Users\jane\.ssh\keys",
            r"C:\Users\jane\.aws",
            r"C:\Users\jane\AppData\Local\Google\Chrome\User Data",
            r"C:\Users\jane\AppData\Roaming\Microsoft\Credentials",
            r"C:\Users\jane\.senclaw\data",
        ] {
            assert!(forbidden_mount_reason(p, &host(HOST)).is_some(), "{p}");
        }
    }

    #[test]
    fn the_store_stub_is_recognised_by_its_place_and_its_size() {
        let stub = r"C:\Users\jane\AppData\Local\Microsoft\WindowsApps\python.exe";
        assert!(is_store_stub(stub, 0));
        assert!(!is_store_stub(stub, 4096), "a real executable in that folder is not a stub");
        assert!(!is_store_stub(r"C:\Python312\python.exe", 0));
    }

    #[test]
    fn a_virtualenv_names_the_interpreter_it_forwards_to() {
        let cfg = "home = C:\\Python312\r\ninclude-system-site-packages = false\r\nversion = 3.12.4\r\n";
        assert_eq!(venv_home(cfg).as_deref(), Some(r"C:\Python312"));
        assert_eq!(venv_home("version = 3.12\n"), None);
        assert_eq!(venv_home("home =\n"), None);
    }

    #[test]
    fn the_wait_never_wraps_and_is_never_infinite() {
        assert_eq!(wait_millis(30_000), 30_000);
        assert_eq!(wait_millis(u64::MAX), u32::MAX - 1);
        assert_eq!(wait_millis(u32::MAX as u64 + 5), u32::MAX - 1);
    }

    #[test]
    fn job_limits_have_a_floor_and_a_ceiling() {
        assert_eq!(job_limits(512, 256), (512 * 1024 * 1024, 256));
        assert_eq!(job_limits(1, 1), (64 * 1024 * 1024, 16));
        assert_eq!(job_limits(512, 1_000_000).1, 8192);
    }

    #[test]
    fn run_jobs_are_named_per_sandbox_and_slot() {
        assert_eq!(job_name("8acba418-fd25-4980", 0), r"Local\SenClawSbxJob-8acba418fd254980-0");
        assert_ne!(job_name("a", 1), job_name("a", 2));
        assert_ne!(job_name("a", 1), job_name("b", 1));
    }

    #[test]
    fn elapsed_and_cpu_read_like_ps() {
        assert_eq!(ps_elapsed(5), "00:05");
        assert_eq!(ps_elapsed(3_725), "01:02:05");
        assert_eq!(ps_elapsed(90_061), "1-01:01:01");
        assert_eq!(cpu_percent(1_000_000, 2_000_000), 50.0);
        assert_eq!(cpu_percent(4_000_000, 2_000_000), 200.0, "two cores busy");
        assert_eq!(cpu_percent(1, 0), 0.0);
    }

    fn npm_like(t: &str) -> bool {
        matches!(t.trim_matches('"').to_ascii_lowercase().as_str(), "npm" | "npx" | "yarn")
    }

    #[test]
    fn each_line_that_runs_a_batch_file_gets_call() {
        assert_eq!(
            call_batch_commands("echo a\r\nnpm install\nnpm test\nnode -v", &npm_like),
            "echo a\ncall npm install\ncall npm test\nnode -v"
        );
        assert_eq!(
            call_batch_commands("cd app && npm install && echo ok || npx tsc & yarn", &npm_like),
            "cd app && call npm install && echo ok || call npx tsc & call yarn"
        );
        assert_eq!(call_batch_commands("@npm ci", &npm_like), "@call npm ci");
        assert_eq!(call_batch_commands("call npm install", &npm_like), "call npm install", "not twice");
    }

    #[test]
    fn quoted_escaped_and_argument_words_are_left_alone() {
        let s = r#"echo "npm & npm" ^& npm do npm"#;
        assert_eq!(call_batch_commands(s, &npm_like), s);
        assert_eq!(call_batch_commands("echo do npm", &npm_like), "echo do npm");
        let s = "for /f %i in ('npm ls') do @echo %i";
        assert_eq!(call_batch_commands(s, &npm_like), s);
    }

    #[test]
    fn blocks_for_loops_and_else_start_commands() {
        assert_eq!(
            call_batch_commands("if exist package.json (npm i) else (npm init -y)", &npm_like),
            "if exist package.json (call npm i) else (call npm init -y)"
        );
        assert_eq!(
            call_batch_commands("if exist package.json (\n  npm install\n) else (\n  npm init -y\n)", &npm_like),
            "if exist package.json (\n  call npm install\n) else (\n  call npm init -y\n)"
        );
        assert_eq!(
            call_batch_commands("for %i in (a b) do npm run %i", &npm_like),
            "for %i in (a b) do call npm run %i"
        );
    }

    fn fake_fs(files: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |p: &str| files.iter().any(|f| f.eq_ignore_ascii_case(p))
    }

    #[test]
    fn a_command_is_looked_up_the_way_cmd_does() {
        const EXT: &str = ".COM;.EXE;.BAT;.CMD";
        let path = r"C:\a;C:\b";
        let fs = fake_fs(&[r"C:\b\npm.cmd", r"C:\a\tool.exe", r"C:\b\tool.cmd", r"C:\a\both.exe", r"C:\a\both.cmd", r"C:\w\build.bat", r"C:\w\echo.cmd", r"C:\w\scripts\go.cmd"]);
        assert!(runs_a_batch_file("npm", r"C:\w", path, EXT, &fs));
        assert!(!runs_a_batch_file("tool", r"C:\w", path, EXT, &fs), "an .exe in an earlier PATH folder wins");
        assert!(!runs_a_batch_file("both", r"C:\w", path, EXT, &fs), ".EXE comes before .CMD in PATHEXT");
        assert!(runs_a_batch_file("build", r"C:\w", path, EXT, &fs), "the current folder is searched first");
        assert!(!runs_a_batch_file("echo", r"C:\w", path, EXT, &fs), "cmd's own command wins");
        assert!(runs_a_batch_file(r"scripts\go", r"C:\w", path, EXT, &fs));
        assert!(runs_a_batch_file("setup.CMD", r"C:\w", path, EXT, &fs));
        assert!(!runs_a_batch_file("node.exe", r"C:\w", path, EXT, &fs));
        assert!(!runs_a_batch_file("missing", r"C:\w", path, EXT, &fs));
        assert!(runs_a_batch_file(r#""C:\b\npm""#, r"C:\w", path, EXT, &fs));
    }

    #[test]
    fn node_is_told_not_to_walk_up_to_the_drive_root() {
        let mut env = Vec::new();
        node_without_realpath(&mut env);
        assert_eq!(get(&env, "NODE_PRESERVE_SYMLINKS"), Some("1"));
        assert_eq!(get(&env, "NODE_OPTIONS"), Some("--preserve-symlinks-main"));

        let mut env = vec![("NODE_OPTIONS".to_string(), "--max-old-space-size=512".to_string())];
        node_without_realpath(&mut env);
        assert_eq!(get(&env, "NODE_OPTIONS"), Some("--max-old-space-size=512 --preserve-symlinks-main"));
        node_without_realpath(&mut env);
        assert_eq!(get(&env, "NODE_OPTIONS"), Some("--max-old-space-size=512 --preserve-symlinks-main"), "not added twice");

        let mut env = vec![("NODE_PRESERVE_SYMLINKS".to_string(), "0".to_string())];
        node_without_realpath(&mut env);
        assert_eq!(get(&env, "NODE_PRESERVE_SYMLINKS"), Some("0"), "the sandbox's own choice wins");
    }

    #[test]
    fn a_too_long_local_appdata_is_replaced_and_a_short_one_kept() {
        let deep = format!(r"C:\{}", "d".repeat(200));
        let mut env = windows_env(Vec::new(), &deep, &host(HOST));
        assert!(shorten_local_appdata(&mut env, r"C:\Users\jane\AppData\Local\Packages\x\AC"));
        assert_eq!(get(&env, "LOCALAPPDATA"), Some(r"C:\Users\jane\AppData\Local\Packages\x\AC"));
        assert_eq!(get(&env, "APPDATA"), Some(format!(r"{deep}\AppData\Roaming").as_str()), "only LOCALAPPDATA moves");

        let mut env = windows_env(Vec::new(), r"C:\sb\one", &host(HOST));
        assert!(!shorten_local_appdata(&mut env, r"C:\other"));
        assert_eq!(get(&env, "LOCALAPPDATA"), Some(r"C:\sb\one\AppData\Local"));
    }

    #[test]
    fn windows_dir_is_detected_by_systemroot_or_the_fallback() {
        let h = host(HOST); // HOST has SystemRoot = C:\Windows
        assert!(is_windows_dir(r"C:\Windows", &h));
        assert!(is_windows_dir(r"C:\Windows\System32", &h));
        assert!(is_windows_dir(r"C:\WINDOWS\SysWOW64\cmd.exe", &h));
        assert!(!is_windows_dir(r"C:\Program Files\nodejs", &h));
        assert!(!is_windows_dir(r"C:\Users\jane\projects", &h));
        // When no env var is present the fallback (C:\Windows) still works.
        assert!(is_windows_dir(r"C:\Windows\System32", &|_| None));
        assert!(!is_windows_dir(r"C:\Program Files", &|_| None));
    }
}
