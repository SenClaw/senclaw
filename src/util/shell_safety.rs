//! Shell-command safety classification.
//!
//! Port of sema-core `util/shellSafety.ts` + the injection scanner from
//! `util/commands.ts`. Used by the permission manager to decide which Bash
//! commands may run without prompting ("readonly safe"), which must never be
//! covered by a saved prefix authorization, and which are deterministically
//! dangerous.
//!
//! All parsing is fail-closed: anything we cannot classify confidently is NOT
//! readonly-safe.

use std::collections::HashSet;
use std::sync::LazyLock;

/// Single commands whose first word alone proves they are read-only.
/// `find` is included but additionally screened for dangerous action flags.
static READONLY_COMMANDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "pwd", "tree", "date", "which", "find", "ls", "grep", "head", "tail", "cat", "du", "wc",
        "echo", "env", "printenv",
    ])
});

/// `find` action flags that execute commands (-exec/-execdir/-ok/-okdir),
/// delete (-delete), or write files (-fprintf/-fprint/-fls).
static DANGEROUS_FIND_FLAGS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprintf", "-fprint", "-fls",
    ])
});

/// Full commands allowed only on exact match (multi-word commands cannot be
/// allowed by first word; e.g. plain `git` is only safe with a readonly
/// subcommand).
static SAFE_FULL_COMMANDS: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| HashSet::from(["git status", "git diff", "git log", "git branch"]));

/// Read-only git subcommands: output only, no repo/worktree mutation.
/// Deliberately excludes config / tag / branch / remote / stash (they mutate
/// once given arguments) and help (spawns man/pager/browser).
static READONLY_GIT_SUBCOMMANDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "log",
        "show",
        "diff",
        "status",
        "blame",
        "reflog",
        "shortlog",
        "describe",
        "rev-parse",
        "rev-list",
        "ls-files",
        "ls-tree",
        "cat-file",
        "whatchanged",
        "name-rev",
        "grep",
        "cherry",
        "count-objects",
        "var",
        "version",
    ])
});

/// Pure filters: read stdin/files, write stdout only. `sort` and `uniq` can
/// still write files and get extra screening below.
static READONLY_FILTER_COMMANDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "sort", "uniq", "nl", "tac", "rev", "cut", "tr", "column", "comm",
    ])
});

/// First words whose danger lives in the arguments — a prefix authorization
/// like `rm:*` or `sudo:*` would wave through `rm -rf /`, so these never get
/// prefix auth and saved prefixes never cover them.
static DANGEROUS_PREFIX_COMMANDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([
        "rm", "rmdir", "dd", "shred", "truncate", "chmod", "chown", "chgrp", "kill", "killall",
        "pkill", "sudo", "doas", "su", "mv",
    ])
});

// ============================================================================
// Injection detection (port of commands.ts hasCommandInjection)
// ============================================================================

/// Command-injection markers: backticks / `$(` / newline / `&&` / `||` / `;`,
/// distinguished from quoted literals:
/// - inside single quotes: everything is literal → ignored
/// - inside double quotes: newline/;/&&/|| are literal, but `$(` and backtick
///   still substitute → only those two are flagged
/// - outside quotes: all markers are flagged
///
/// Backslash escapes skip the next character so `\"` / `\'` aren't mistaken
/// for quote boundaries.
pub fn has_command_injection(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();

        if in_single {
            if c == '\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }

        if c == '\\' {
            i += 2;
            continue;
        }

        if in_double {
            if c == '`' {
                return true;
            }
            if c == '$' && next == Some('(') {
                return true;
            }
            if c == '"' {
                in_double = false;
            }
            i += 1;
            continue;
        }

        match c {
            '\'' => in_single = true,
            '"' => in_double = true,
            '`' | '\n' | ';' => return true,
            '$' if next == Some('(') => return true,
            '&' | '|' if next == Some(c) => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

// ============================================================================
// Segment splitting
// ============================================================================

/// Split a command into subcommand segments at unquoted `|`, `|&`, `&`, `;`,
/// and newlines. (Callers that need `&&`/`||`/`$()` rejected run
/// [`has_command_injection`] first; this splitter also treats those pairs as
/// separators so [`has_dangerous_command`] sees each side.)
fn split_segments(command: &str) -> Vec<String> {
    let chars: Vec<char> = command.chars().collect();
    let mut segments = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;

    let flush = |cur: &mut String, segments: &mut Vec<String>| {
        let trimmed = cur.trim();
        if !trimmed.is_empty() {
            segments.push(trimmed.to_string());
        }
        cur.clear();
    };

    while i < chars.len() {
        let c = chars[i];
        if in_single {
            if c == '\'' {
                in_single = false;
            }
            cur.push(c);
            i += 1;
            continue;
        }
        if c == '\\' {
            cur.push(c);
            if let Some(&n) = chars.get(i + 1) {
                cur.push(n);
            }
            i += 2;
            continue;
        }
        if in_double {
            if c == '"' {
                in_double = false;
            }
            cur.push(c);
            i += 1;
            continue;
        }
        match c {
            '\'' => {
                in_single = true;
                cur.push(c);
            }
            '"' => {
                in_double = true;
                cur.push(c);
            }
            '|' => {
                flush(&mut cur, &mut segments);
                // Collapse || and |&
                if matches!(chars.get(i + 1), Some('|') | Some('&')) {
                    i += 1;
                }
            }
            '&' => {
                // `>&` / `<&` are fd-dup redirections, not the background
                // operator — keep them inside the segment (`ls 2>&1`).
                if cur.ends_with('>') || cur.ends_with('<') {
                    cur.push(c);
                } else {
                    flush(&mut cur, &mut segments);
                    if chars.get(i + 1) == Some(&'&') {
                        i += 1;
                    }
                }
            }
            ';' | '\n' => {
                flush(&mut cur, &mut segments);
                if c == ';' && chars.get(i + 1) == Some(&';') {
                    i += 1;
                }
            }
            _ => cur.push(c),
        }
        i += 1;
    }
    flush(&mut cur, &mut segments);
    segments
}

// ============================================================================
// Redirection handling
// ============================================================================

/// Whether the command contains any unquoted redirection (`>`, `>>`, `<`,
/// `>&`, …). Used for prefix-auth defense in depth: prefix matching only looks
/// at the first word, while the danger of `echo x > file` lives in the
/// redirection — so commands with redirections must not be covered by (or
/// offered) prefix authorization. Even `2>/dev/null` counts here.
pub fn has_redirection(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_single {
            if c == '\'' {
                in_single = false;
            }
        } else if c == '\\' {
            i += 2;
            continue;
        } else if in_double {
            if c == '"' {
                in_double = false;
            }
        } else {
            match c {
                '\'' => in_single = true,
                '"' => in_double = true,
                '<' | '>' => return true,
                _ => {}
            }
        }
        i += 1;
    }
    false
}

/// Strip side-effect-free redirections (discard to `/dev/null`, fd dup/close
/// like `2>&1` / `>&-`) from a segment. Returns `None` when any *other*
/// redirection remains — writing or reading a real file disqualifies the
/// segment from the readonly fast path.
fn strip_redirections(segment: &str) -> Option<String> {
    let chars: Vec<char> = segment.chars().collect();
    let mut out = String::with_capacity(segment.len());
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        if in_single {
            if c == '\'' {
                in_single = false;
            }
            out.push(c);
            i += 1;
            continue;
        }
        if c == '\\' {
            out.push(c);
            if let Some(&n) = chars.get(i + 1) {
                out.push(n);
            }
            i += 2;
            continue;
        }
        if in_double {
            if c == '"' {
                in_double = false;
            }
            out.push(c);
            i += 1;
            continue;
        }
        match c {
            '\'' => {
                in_single = true;
                out.push(c);
            }
            '"' => {
                in_double = true;
                out.push(c);
            }
            '<' | '>' => {
                // Optional fd digits directly before the operator belong to it
                // (e.g. `2>`); pop them off `out` so they don't linger.
                let mut fd_digits = 0;
                while out
                    .chars()
                    .last()
                    .map(|p| p.is_ascii_digit())
                    .unwrap_or(false)
                {
                    out.pop();
                    fd_digits += 1;
                }
                // But `file2 > x` — digits attached to a word are not an fd.
                if fd_digits > 0
                    && out
                        .chars()
                        .last()
                        .map(|p| !p.is_whitespace())
                        .unwrap_or(false)
                {
                    return None; // ambiguous, fail closed
                }

                // Read the full operator.
                let mut op = String::new();
                op.push(c);
                i += 1;
                while let Some(&n) = chars.get(i) {
                    if n == '>' || n == '<' || n == '&' || n == '|' {
                        op.push(n);
                        i += 1;
                    } else {
                        break;
                    }
                }

                // Read the target token (skip spaces first).
                while chars
                    .get(i)
                    .map(|n| *n == ' ' || *n == '\t')
                    .unwrap_or(false)
                {
                    i += 1;
                }
                let mut target = String::new();
                while let Some(&n) = chars.get(i) {
                    if n.is_whitespace() {
                        break;
                    }
                    target.push(n);
                    i += 1;
                }

                let safe = match op.as_str() {
                    ">&" | "<&" => {
                        target == "-"
                            || (!target.is_empty() && target.chars().all(|t| t.is_ascii_digit()))
                    }
                    ">" | ">>" | "<" => target == "/dev/null",
                    _ => false, // <<, <<<, >|, <> … never safe here
                };
                if !safe {
                    return None;
                }
                out.push(' ');
                continue;
            }
            _ => out.push(c),
        }
        i += 1;
    }
    Some(out)
}

// ============================================================================
// Per-segment readonly checks
// ============================================================================

fn tokens(segment: &str) -> Vec<&str> {
    segment.split_whitespace().collect()
}

/// Dangerous git flags: even readonly subcommands can execute commands or
/// write files with these (`-c core.pager=…`, `--exec-path`, `--ext-diff`,
/// `--output`, `git grep -O'sh -c …'`).
fn has_dangerous_git_flag(toks: &[&str]) -> bool {
    toks.iter().any(|t| {
        if *t == "-c" {
            return true;
        }
        for long in [
            "--exec-path",
            "--exec",
            "--ext-diff",
            "--output",
            "--open-files-in-pager",
        ] {
            if *t == long || t.starts_with(&format!("{long}=")) {
                return true;
            }
        }
        // Short-option cluster containing O (possibly with attached value):
        // -O, -Oless, -nO …
        if let Some(rest) = t.strip_prefix('-') {
            if !rest.starts_with('-') {
                let letters: String = rest
                    .chars()
                    .take_while(|ch| ch.is_ascii_alphabetic())
                    .collect();
                if letters.contains('O') {
                    return true;
                }
            }
        }
        false
    })
}

fn is_readonly_git_command(segment: &str) -> bool {
    let toks = tokens(segment);
    if toks.first() != Some(&"git") {
        return false;
    }
    // `git -C /path log` (value-taking global options between git and the
    // subcommand) is deliberately NOT recognized — fall back to confirmation.
    let Some(sub) = toks.get(1) else { return false };
    if !READONLY_GIT_SUBCOMMANDS.contains(sub) {
        return false;
    }
    !has_dangerous_git_flag(&toks)
}

/// Count uniq operands (INPUT/OUTPUT). A bare `-` is a valid stdin operand;
/// after `--` every token counts as an operand (otherwise `uniq -- in -out`
/// would sneak `-out` past the write-file screen).
fn count_uniq_operands(args: &[&str]) -> usize {
    if let Some(dd) = args.iter().position(|t| *t == "--") {
        let before = args[..dd]
            .iter()
            .filter(|t| **t == "-" || !t.starts_with('-'))
            .count();
        before + (args.len() - dd - 1)
    } else {
        args.iter()
            .filter(|t| **t == "-" || !t.starts_with('-'))
            .count()
    }
}

fn is_readonly_filter_command(segment: &str) -> bool {
    let toks = tokens(segment);
    let Some(first) = toks.first() else {
        return false;
    };
    if !READONLY_FILTER_COMMANDS.contains(first) {
        return false;
    }
    // sort -o/--output writes to a file; the short flag can hide in a cluster (-no).
    if *first == "sort" {
        let writes = toks[1..].iter().any(|t| {
            *t == "--output"
                || t.starts_with("--output=")
                || (t.starts_with('-')
                    && !t.starts_with("--")
                    && t[1..]
                        .chars()
                        .take_while(|c| c.is_ascii_alphabetic())
                        .any(|c| c == 'o'))
        });
        if writes {
            return false;
        }
    }
    // uniq [INPUT [OUTPUT]]: a second positional operand writes a file.
    if *first == "uniq" && count_uniq_operands(&toks[1..]) >= 2 {
        return false;
    }
    true
}

fn is_dangerous_segment(segment: &str) -> bool {
    let toks = tokens(segment);
    let Some(first) = toks.first() else {
        return false;
    };
    if DANGEROUS_PREFIX_COMMANDS.contains(first) {
        return true;
    }
    if first.starts_with("mkfs") {
        return true;
    }
    if *first == "find" && toks.iter().any(|t| DANGEROUS_FIND_FLAGS.contains(t)) {
        return true;
    }
    false
}

fn is_readonly_safe_segment(segment: &str) -> bool {
    // Strip harmless redirections first; any remaining redirection is a write
    // primitive (`echo x > /etc/passwd`) and disqualifies the segment.
    let Some(stripped) = strip_redirections(segment) else {
        return false;
    };
    let stripped = stripped.trim();
    if stripped.is_empty() {
        return false;
    }
    if is_readonly_git_command(stripped) {
        return true;
    }
    if is_readonly_filter_command(stripped) {
        return true;
    }
    let toks = tokens(stripped);
    let first = toks[0];
    if !READONLY_COMMANDS.contains(first) {
        return false;
    }
    if first == "find" && toks.iter().any(|t| DANGEROUS_FIND_FLAGS.contains(t)) {
        return false;
    }
    true
}

// ============================================================================
// Public classification API
// ============================================================================

/// Whether the whole command is provably read-only and may run without a
/// prompt. Pipe segments must each be readonly with no unsafe redirection;
/// `&&`/`||`/`;`/newlines/substitution defer to confirmation.
pub fn is_readonly_safe_command(command: &str) -> bool {
    let trimmed = command.trim();
    if SAFE_FULL_COMMANDS.contains(trimmed) {
        return true;
    }
    if has_command_injection(command) {
        return false;
    }
    let segments = split_segments(command);
    !segments.is_empty() && segments.iter().all(|s| is_readonly_safe_segment(s))
}

/// Whether the command contains a semantically dangerous subcommand
/// (rm/sudo/mv/… first words, mkfs*, or find with action flags). Deterministic
/// danger — never auto-run, go straight to the human.
pub fn has_dangerous_command(command: &str) -> bool {
    split_segments(command)
        .iter()
        .any(|s| is_dangerous_segment(s))
}

/// Whether the command must not be covered by (or offered) prefix
/// authorization: contains redirections or dangerous subcommands. Prefix
/// matching only sees the first word, so these need per-command confirmation.
pub fn is_unsafe_for_prefix_auth(command: &str) -> bool {
    has_redirection(command) || has_dangerous_command(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injection_detection() {
        assert!(has_command_injection("ls; rm -rf /"));
        assert!(has_command_injection("ls && rm x"));
        assert!(has_command_injection("ls || rm x"));
        assert!(has_command_injection("echo `whoami`"));
        assert!(has_command_injection("echo $(whoami)"));
        assert!(has_command_injection("ls\nrm x"));
        assert!(has_command_injection("echo \"$(whoami)\""));
        assert!(has_command_injection("echo \"`id`\""));

        assert!(!has_command_injection("echo 'a; b && c'"));
        assert!(!has_command_injection("echo \"a; b && c\""));
        assert!(!has_command_injection("echo 'multi\nline'"));
        assert!(!has_command_injection("grep -r foo ."));
        assert!(!has_command_injection("echo \\;"));
        assert!(!has_command_injection("cat a | grep b"));
    }

    #[test]
    fn readonly_single_commands() {
        assert!(is_readonly_safe_command("ls"));
        assert!(is_readonly_safe_command("ls -la /tmp"));
        assert!(is_readonly_safe_command("grep -rn foo src"));
        assert!(is_readonly_safe_command("git status"));
        assert!(is_readonly_safe_command("wc -l file.txt"));
        assert!(!is_readonly_safe_command("rm -rf /"));
        assert!(!is_readonly_safe_command("npm install"));
        assert!(!is_readonly_safe_command(""));
    }

    #[test]
    fn readonly_rejects_injection_and_redirection() {
        assert!(!is_readonly_safe_command("cat a; rm -rf /"));
        assert!(!is_readonly_safe_command("ls && rm x"));
        assert!(!is_readonly_safe_command("echo x > /etc/passwd"));
        assert!(!is_readonly_safe_command("cat secret > leak.txt"));
        assert!(!is_readonly_safe_command("head -1 < <(evil)"));
        assert!(!is_readonly_safe_command("cat <<EOF\nhi\nEOF"));
    }

    #[test]
    fn readonly_allows_safe_redirections() {
        assert!(is_readonly_safe_command("grep foo src 2>/dev/null"));
        assert!(is_readonly_safe_command("ls 2>&1"));
        assert!(is_readonly_safe_command("cat file 2>&1 | grep x"));
        assert!(is_readonly_safe_command("du -sh . >/dev/null"));
        // /dev/null lookalikes stay blocked
        assert!(!is_readonly_safe_command("echo x > /dev/nullx"));
        assert!(!is_readonly_safe_command(
            "echo x > /dev/null/../etc/passwd"
        ));
    }

    #[test]
    fn readonly_pipelines() {
        assert!(is_readonly_safe_command("cat file | grep foo | head -5"));
        assert!(is_readonly_safe_command("ls | sort | uniq"));
        assert!(!is_readonly_safe_command("cat file | sh"));
        assert!(!is_readonly_safe_command("ls | xargs rm"));
    }

    #[test]
    fn find_action_flags_blocked() {
        assert!(is_readonly_safe_command("find . -name '*.rs'"));
        assert!(!is_readonly_safe_command("find . -exec rm {} \\;"));
        assert!(!is_readonly_safe_command("find . -delete"));
        assert!(!is_readonly_safe_command("find . -execdir sh -c x \\;"));
        assert!(!is_readonly_safe_command("find . -fprintf out fmt"));
    }

    #[test]
    fn git_readonly_subcommands() {
        assert!(is_readonly_safe_command("git log --oneline -5"));
        assert!(is_readonly_safe_command("git show HEAD"));
        assert!(is_readonly_safe_command("git blame src/main.rs"));
        assert!(is_readonly_safe_command("git rev-parse HEAD"));
        // mutating or excluded subcommands
        assert!(!is_readonly_safe_command("git push"));
        assert!(!is_readonly_safe_command("git config user.name x"));
        assert!(!is_readonly_safe_command("git tag v1"));
        assert!(!is_readonly_safe_command("git help -w foo"));
        // value-taking global option between git and subcommand → not recognized
        assert!(!is_readonly_safe_command("git -C /path log"));
    }

    #[test]
    fn git_dangerous_flags_blocked() {
        assert!(!is_readonly_safe_command("git log -c core.pager=evil"));
        assert!(!is_readonly_safe_command("git diff --ext-diff"));
        assert!(!is_readonly_safe_command("git diff --output=/tmp/x"));
        assert!(!is_readonly_safe_command("git grep -Otouch foo"));
        assert!(!is_readonly_safe_command("git grep -nO foo"));
        assert!(!is_readonly_safe_command("git log --exec-path=/tmp"));
    }

    #[test]
    fn filter_command_write_paths_blocked() {
        assert!(is_readonly_safe_command("sort file.txt"));
        assert!(!is_readonly_safe_command("sort -o /etc/passwd file.txt"));
        assert!(!is_readonly_safe_command("sort --output=x file.txt"));
        assert!(!is_readonly_safe_command("sort -no x file.txt"));
        assert!(is_readonly_safe_command("uniq file.txt"));
        assert!(!is_readonly_safe_command("uniq in.txt out.txt"));
        assert!(!is_readonly_safe_command("uniq -- in -out"));
        assert!(is_readonly_safe_command("uniq -c file.txt"));
    }

    #[test]
    fn dangerous_command_detection() {
        assert!(has_dangerous_command("rm -rf /"));
        assert!(has_dangerous_command("sudo apt install x"));
        assert!(has_dangerous_command("mv a b"));
        assert!(has_dangerous_command("mkfs.ext4 /dev/sda1"));
        assert!(has_dangerous_command("find . -exec rm {} \\;"));
        assert!(has_dangerous_command("ls | kill 123"));
        assert!(has_dangerous_command("ls; rm -rf /"));
        assert!(!has_dangerous_command("ls -la"));
        assert!(!has_dangerous_command("npm test"));
    }

    #[test]
    fn prefix_auth_safety() {
        assert!(is_unsafe_for_prefix_auth("echo x > file"));
        assert!(is_unsafe_for_prefix_auth("npm test 2>/dev/null"));
        assert!(is_unsafe_for_prefix_auth("rm -rf node_modules"));
        assert!(is_unsafe_for_prefix_auth("sudo npm i"));
        assert!(!is_unsafe_for_prefix_auth("npm test"));
        assert!(!is_unsafe_for_prefix_auth("cargo build --release"));
    }

    #[test]
    fn quoted_operators_are_literal() {
        assert!(is_readonly_safe_command("grep 'a|b' file"));
        assert!(is_readonly_safe_command("echo 'x > y'"));
        assert!(is_readonly_safe_command("echo \"a && b\""));
        assert!(!has_redirection("echo 'x > y'"));
        assert!(has_redirection("echo x > y"));
    }
}
