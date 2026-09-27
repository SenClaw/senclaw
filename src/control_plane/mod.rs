//! JEV v2.2 control plane — P0/P1 layer: rules first, then Jev (a typed
//! probability-answering decision model reached through
//! [`crate::decision::client`] → the `decision` runtime), then the LLM, then a
//! human.
//!
//! This module is additive by construction: it builds the vocabulary (the
//! decision [`ladder`], the file-based [`registry`], [`trace`],
//! [`workspace`], [`context_assembler`], [`loop_controller`],
//! [`tool_registry`], [`policy_gate`]) and wires it at the harness's existing
//! seams — the decision gate, the pre-skill router, the per-chat event bus.
//! Nothing here changes a default agent turn's outcome:
//!
//! - [`ControlPlaneSettings::jev_off`] (env `SENCLAW_JEV_OFF=1` wins) skips
//!   every Jev tier, including the pre-existing gate/router engine calls,
//!   which fall back to their no-engine behaviour — the ablation baseline.
//! - New shadow specs (`input.guard`, `clarify.needed`, `task.done`,
//!   `loop.next_step`) call the decision runtime only when
//!   [`ControlPlaneSettings::shadow`] is on or the spec's own mode is
//!   `active` — never on a default install, which would otherwise pay a
//!   1.2–1.7 GB Laya load on every chat turn just to collect labels.
//! - `route.skill` and `tool.risk` are *descriptive* wrappers over the
//!   existing pre-skill router and tool-call gate: their mode and behaviour
//!   stay exactly what `decisionConfig.skills`/`decisionConfig.gate` already
//!   drive (see [`registry::Spec::wraps_existing`]).
//! - [`trace`] and [`policy_gate`] are the two pieces that *are* always on
//!   (§4's "layers that cannot be switched off" — trace records metadata
//!   only, policy_gate is a pure fail-closed classifier already on the
//!   permission path).

pub mod context_assembler;
pub mod ladder;
pub mod loop_controller;
pub mod policy_gate;
pub mod registry;
pub mod settings;
pub mod tool_registry;
pub mod trace;
pub mod workspace;

pub use settings::ControlPlaneSettings;

/// `SENCLAW_JEV_OFF=1` wins over `controlPlane.jevOff` — explicit env intent
/// for a one-off ablation run beats whatever is saved in `config.json`.
pub fn jev_off(settings: &ControlPlaneSettings) -> bool {
    std::env::var("SENCLAW_JEV_OFF")
        .map(|v| v == "1")
        .unwrap_or(false)
        || settings.jev_off
}

/// Same resolution as `Config::from_env`'s `global_config_path` — used by
/// event-bus subscribers that have no injected `Config` to hand
/// (`agent::agent_pool::engine::ZenCoreApi` gets one only through
/// `set_runtime_config`, which is a documented no-op: "config is passed via
/// environment"). Reading this per event is a small, deliberate cost for a
/// setting the engine bridge otherwise has no path to.
pub(crate) fn default_config_path() -> std::path::PathBuf {
    match std::env::var("SENCLAW_CONFIG_PATH") {
        Ok(v) if !v.is_empty() => std::path::PathBuf::from(v),
        _ => senclaw_home().join("config.json"),
    }
}

/// `~/.senclaw` — the same resolution every other module in the crate uses
/// (`crate::trajectory::root`, `crate::failures`), so a scratch `HOME` for a
/// live check redirects control-plane state exactly like everything else.
pub(crate) fn senclaw_home() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".senclaw")
}

/// A jid, mangled into a path component. Mirrors `crate::trajectory::safe`,
/// plus injectivity: two different inputs must never mangle to the same
/// output. Folding every non-`[A-Za-z0-9_-]` character to `_` alone is not
/// injective — `"a:b"` and `"a_b"` both mangle to `"a_b"`, so their traces and
/// workspaces would intermix (a real finding: two different chat jids sharing
/// one trace directory silently merges their metadata). When mangling
/// actually changed the string, a short hash of the *original* input is
/// appended so the two can no longer collide; an input that was already safe
/// is returned unchanged (so directories created before this fix keep
/// resolving to the same name).
pub(crate) fn safe_id(jid: &str) -> String {
    let mangled: String = jid
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if mangled == jid {
        return mangled;
    }
    format!("{mangled}_{}", short_hash(jid))
}

/// First 4 bytes (8 hex chars) of SHA-256 — enough to make [`safe_id`]
/// injective in practice without turning every mangled directory name into a
/// wall of hex. Not used anywhere a full cryptographic digest is required.
fn short_hash(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(&hasher.finalize()[..4])
}

/// Whether `part` is exactly what [`safe_id`] would produce from itself —
/// i.e. it is already in mangled form. Any character `safe_id` would have
/// replaced (`.`, `/`, `\`, `:`, …) makes this `false`, so this single check
/// rejects `..`, an absolute path, and a nested separator in one place. Used
/// to validate a path component that arrived from outside the process
/// (an HTTP path param) before it is joined onto a directory that must stay
/// confined — `list_all`'s own output always satisfies this, by construction,
/// for every id it hands out, so a legitimate id never gets rejected here.
pub(crate) fn is_safe_path_component(part: &str) -> bool {
    !part.is_empty() && safe_id(part) == part
}

/// Canonicalize `path` and confirm it did not escape `root` — the
/// defense-in-depth half of validating a file path built from external input.
/// [`is_safe_path_component`] already makes escaping impossible for a
/// component built through `safe_id`'s character set, but this check does not
/// depend on that invariant holding elsewhere too, so a future change to the
/// character set (or a new caller that skips the component check) cannot
/// reopen the traversal on its own. `canonicalize` requires the path to
/// exist, which every caller here already needs to be true to read anything.
pub(crate) fn path_stays_under(root: &std::path::Path, path: &std::path::Path) -> bool {
    let (Ok(canon_root), Ok(canon_path)) = (std::fs::canonicalize(root), std::fs::canonicalize(path)) else {
        return false;
    };
    canon_path.starts_with(&canon_root)
}

#[cfg(unix)]
pub(crate) fn restrict_dir(p: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
}
#[cfg(not(unix))]
pub(crate) fn restrict_dir(_p: &std::path::Path) {}

#[cfg(unix)]
pub(crate) fn restrict_file(p: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
pub(crate) fn restrict_file(_p: &std::path::Path) {}

/// Every test in this module tree that redirects a `SENCLAW_*` env var to a
/// temp dir (or flips `SENCLAW_JEV_OFF`) must hold this for the duration —
/// `cargo test` runs `#[test]` functions in parallel by default, and two
/// tests racing to set/unset the *same* process-global env var is exactly
/// the kind of flake that looks like a real bug (a write lands in test A's
/// temp dir, a concurrent `remove_var` from test B makes the next read fall
/// back to the real default path instead). Poisoning is ignored on purpose:
/// one panicking test must not cascade-fail every other test that needs
/// this lock.
#[cfg(test)]
pub(crate) fn env_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_wins_over_config_and_config_wins_over_default() {
        let _guard = env_test_guard();
        let mut s = ControlPlaneSettings::default();
        assert!(!jev_off(&s));
        s.jev_off = true;
        assert!(jev_off(&s));

        // SAFETY: tests in this crate serialize env mutation per-test via
        // `#[test]`'s own isolation is not guaranteed across threads, but this
        // var is control-plane-only and no other test reads it concurrently.
        std::env::set_var("SENCLAW_JEV_OFF", "1");
        s.jev_off = false;
        assert!(jev_off(&s), "env must win even when the config says off");
        std::env::remove_var("SENCLAW_JEV_OFF");
        assert!(!jev_off(&s));
    }

    #[test]
    fn safe_id_is_injective_for_inputs_that_used_to_collide() {
        // "a:b" and "a_b" both fold to "a_b" under plain character
        // replacement — the actual bug. They must no longer be equal.
        let a = safe_id("a:b");
        let b = safe_id("a_b");
        assert_ne!(a, b, "two different jids must never map to the same directory name");
        // An already-safe input is returned unchanged (old directories keep resolving).
        assert_eq!(b, "a_b");
        // Determinism: the same input always mangles to the same output.
        assert_eq!(safe_id("a:b"), a);
    }

    #[test]
    fn safe_id_output_is_a_fixed_point_of_itself() {
        // `is_safe_path_component` relies on this: any id `list_all` hands
        // out (always a `safe_id` output) must itself satisfy
        // `safe_id(part) == part`, including the hash-suffixed form.
        for input in ["plain-id_1", "a:b", "../../etc/passwd", "web:code:abc", ""] {
            let once = safe_id(input);
            let twice = safe_id(&once);
            assert_eq!(once, twice, "safe_id({input:?}) = {once:?} is not a fixed point");
        }
    }

    #[test]
    fn is_safe_path_component_rejects_traversal_shapes() {
        assert!(is_safe_path_component("plain-id_1"));
        assert!(is_safe_path_component(&safe_id("a:b")));
        for bad in ["..", "../../etc/passwd", "a/b", "a\\b", ".", "/etc", ""] {
            assert!(!is_safe_path_component(bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn path_stays_under_rejects_escape_and_accepts_a_real_child() {
        let tmp = tempfile::tempdir().unwrap();
        let child = tmp.path().join("child.txt");
        std::fs::write(&child, "x").unwrap();
        assert!(path_stays_under(tmp.path(), &child));

        let outside = tmp.path().parent().unwrap().join("definitely-not-under.txt");
        // Not asserting the outside path exists — canonicalize failing (it
        // usually won't exist) must also be treated as "not confirmed safe",
        // which `path_stays_under` already does by returning `false`.
        assert!(!path_stays_under(tmp.path(), &outside));
    }
}
