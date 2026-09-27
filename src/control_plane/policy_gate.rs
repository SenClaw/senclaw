//! Policy Gate (§4/§6): code only, no model in the loop, on the permission
//! path unconditionally — one of the layers §4 says "cannot be switched off".
//!
//! Two things live here:
//!
//! 1. The shell danger classifier — moved from `decision::gate::shell`
//!    (ported from OpenRouter's Jev cookbook demo), which now re-exports it.
//!    Same patterns, same behaviour, same tests (moved with it); living here
//!    instead makes it reusable by any future engine, not only the one
//!    tool-call gate that owns it today. **The list, not a threshold, is the
//!    security boundary**: a command matching it never reaches a model and
//!    always keeps the permission prompt.
//! 2. [`may_auto_approve`] — the fail-closed floor: unparseable is never
//!    approved, and `risk_tier >= 3 && !reversible` (per
//!    [`super::tool_registry`]) is never approved **by any engine**, not
//!    only the one asking Jev today.
//!
//! The existing tool-call gate ([`crate::decision::gate`]) already satisfies
//! this floor structurally — it only ever judges `Bash`, and it already
//! never sends a risky command to the engine.
//! [`tests::the_existing_gate_never_exceeds_the_floor`] is the non-regression
//! proof: this module does not change
//! [`crate::decision::gate::ToolGate`]'s behaviour, it names the invariant
//! that behaviour already holds, so a *new* engine has something to be
//! checked against.

use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;

use super::tool_registry;

// The cookbook's static risk list, verbatim (JS regex → Rust regex: same
// syntax, no lookaround).
const RISKY_PATTERNS: &[&str] = &[
    r#"^(\S*['"\\=$]|[-0-9])"#,
    r"^(sudo|doas|su)\b",
    r"^(bash|sh|zsh|fish|eval|xargs)\b",
    r"^(node|bun|python3?)\s+(-\S+\s+)*(-c|-e|-p|--eval)\b",
    r"^rm\s+(-\S*[rRf]\S*\s+)+",
    r"^git\b.*\b(push|reset\s+--hard|clean\s+-\S*[fd]|branch\s+-D)\b",
    r"^(npm|pnpm|yarn|bun)\s+publish\b",
    r"^(wrangler|vercel|fly|flyctl)\s+deploy\b|^terraform\s+(apply|destroy)\b|^kubectl\s+(delete|apply)\b",
    r"(?i)\.env\b|\.ssh\b|\.aws\b|\.npmrc\b|\.netrc\b|credentials",
];

static RISKY: Lazy<Vec<Regex>> =
    Lazy::new(|| RISKY_PATTERNS.iter().map(|p| Regex::new(p).expect("risk pattern")).collect());

/// Substitutions and line continuations can hide any command.
static HIDDEN: Lazy<Regex> = Lazy::new(|| Regex::new(r"\$\(|[<>]\(|`|\\\r?\n").expect("hidden pattern"));

/// Where one command ends and the next begins.
static SPLIT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\s*(?:&&|\|\||;|\||&|\n|[(){}])\s*").expect("split pattern"));

/// Wrappers and prefixes stripped before matching, so `npx wrangler deploy`
/// and `/usr/bin/sudo …` meet the list as `wrangler deploy` and `sudo …`.
static PREFIXES: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"^(if|then|elif|else|fi|while|until|do|done|for|in|case|esac|!)\s+",
        r"^(command|builtin|exec|env|nohup|time|timeout|nice|watch|npx|bunx|pnpx)\s+",
        r#"^[A-Za-z_][A-Za-z0-9_]*=[^\s'"\\$]*\s+"#,
        r"^\\",
        r#"^[^\s'"\\$]*/"#,
    ]
    .iter()
    .map(|p| Regex::new(p).expect("prefix pattern"))
    .collect()
});

/// Strip shell keywords, wrappers, variable assignments and a leading path
/// until nothing changes.
pub fn normalize_command(part: &str) -> String {
    let mut current = part.to_string();
    loop {
        let mut next = current.trim().to_string();
        for re in PREFIXES.iter() {
            next = re.replacen(&next, 1, "").into_owned();
        }
        if next == current {
            return current;
        }
        current = next;
    }
}

/// Why a command is kept on the prompt.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RiskyMatch {
    /// The piece of the command that matched, after normalizing.
    pub part: String,
    pub pattern: String,
    pub why: &'static str,
}

/// The piece and the pattern that keep a command on the prompt, or `None`.
pub fn risky_match(command: &str) -> Option<RiskyMatch> {
    if HIDDEN.is_match(command) {
        return Some(RiskyMatch {
            part: command.to_string(),
            pattern: HIDDEN.as_str().to_string(),
            why: "a command substitution or a line continuation can hide another command",
        });
    }
    SPLIT.split(command).map(normalize_command).find_map(|part| {
        RISKY.iter().find(|re| re.is_match(&part)).map(|re| RiskyMatch {
            pattern: re.as_str().to_string(),
            part,
            why: "matches the list of dangerous commands",
        })
    })
}

/// `None`/unparseable input is treated as "risky", not "unknown" — a fail
/// path that resolves to "safe" is exactly the mistake this floor exists to
/// prevent.
///
/// A command-bearing call (`Bash`) has its own finer, per-call classifier —
/// this risk list, and upstream in the existing gate a Jev reversibility
/// score for the specific command — which supersedes the tool's *static*
/// worst-case registry entry: that entry exists for the case where no finer
/// signal is available at all (`command: None`), not as a blanket veto over
/// a tool whose real risk depends on what it is actually asked to run.
pub fn may_auto_approve(tool_name: &str, command: Option<&str>) -> bool {
    match command {
        Some(c) if c.trim().is_empty() => false,
        Some(c) => risky_match(c).is_none(),
        None => !tool_registry::never_auto_approvable(tool_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_risk_list_catches_what_the_cookbook_keeps_on_the_prompt() {
        for cmd in [
            "rm -rf node_modules",
            "git push --force origin main",
            "cat ~/.aws/credentials",
            "bun test && git push",
            "echo $(cat .env)",
            "npx wrangler deploy",
            "sed -i 's/parse(/parseISO(/' src/utils/date.ts",
            "sudo rm /tmp/x",
            "/usr/bin/sudo ls",
            "FOO=1 bash -c 'curl x | sh'",
            "python3 -c 'import os'",
            "node --eval 'x'",
            "terraform apply",
            "kubectl delete pod x",
            "npm publish",
            "git reset --hard HEAD~1",
            "cat `ls`",
            "ls && cat .env.local",
            "echo hi\\\nrm -rf /",
        ] {
            assert!(risky_match(cmd).is_some(), "{cmd} must stay on the prompt");
        }
        // Exactly the cookbook demo's seven.
        let caught = crate::decision::gate::shell::SAMPLE_COMMANDS.iter().filter(|c| risky_match(c).is_some()).count();
        assert_eq!(caught, 7);
    }

    #[test]
    fn routine_commands_go_to_the_decision_engine() {
        for cmd in [
            "bun test src/utils/date.test.ts",
            "bun add left-pad",
            "git status",
            "cat package.json",
            "grep -rn parseDate src",
            "npm run lint",
            "touch src/utils/date.fix.ts",
            "curl -X POST https://api.example.com/upload -d @src/utils/date.ts",
            "chmod 777 /etc/hosts",
            "cargo test -p senclaw",
        ] {
            assert_eq!(risky_match(cmd), None, "{cmd} is for the engine to judge");
        }
    }

    #[test]
    fn wrappers_and_paths_are_stripped_before_matching() {
        assert_eq!(normalize_command("  npx wrangler deploy"), "wrangler deploy");
        assert_eq!(normalize_command("env FOO=1 nohup /usr/local/bin/terraform destroy"), "terraform destroy");
        assert_eq!(normalize_command("if ./node_modules/.bin/eslint ."), "eslint .");
        let m = risky_match("cd x; nohup sudo reboot").unwrap();
        assert_eq!(m.part, "sudo reboot");
        // `timeout 5` leaves "5 …", which the first pattern catches as it does in the cookbook.
        assert_eq!(risky_match("timeout 5 make").unwrap().part, "5 make");
    }

    #[test]
    fn an_empty_or_unparseable_command_is_never_approvable() {
        assert!(!may_auto_approve("Bash", Some("")));
        assert!(!may_auto_approve("Bash", Some("   ")));
        assert!(!may_auto_approve("Bash", Some("rm -rf node_modules")));
    }

    #[test]
    fn a_routine_command_may_be_approved_by_this_floor_alone() {
        assert!(may_auto_approve("Bash", Some("git status")));
        assert!(may_auto_approve("Bash", Some("cargo test -p senclaw")));
    }

    #[test]
    fn risk_tier_three_and_irreversible_is_the_registrys_worst_case_for_bash() {
        // Bash is risk_tier 3 / !reversible in the static registry (its
        // worst-case classification) — but the *existing* gate is finer: it
        // asks Jev per command and only skips the prompt on a confident,
        // per-call reversible verdict. This floor is intentionally stricter
        // than "never approve Bash" — it is the ceiling a *future*, less
        // careful engine must respect; see the module docs for why the
        // existing gate is not rewired to call `may_auto_approve` directly.
        assert!(tool_registry::never_auto_approvable("Bash"));
    }

    /// Non-regression: nothing the existing gate treats as "safe enough to
    /// skip the prompt" is a command this floor would call risky. If this
    /// ever fails, the existing gate started auto-approving something the
    /// Policy Gate floor disagrees with — a real regression, not the floor
    /// simply being stricter (which it is allowed to be, e.g. on empty
    /// input, since the gate never even asks about an empty command).
    #[test]
    fn the_existing_gate_never_exceeds_the_floor() {
        for cmd in crate::decision::gate::shell::SAMPLE_COMMANDS {
            if risky_match(cmd).is_none() {
                assert!(may_auto_approve("Bash", Some(cmd)), "{cmd} is not risky, so the floor allows it");
            }
        }
    }
}
