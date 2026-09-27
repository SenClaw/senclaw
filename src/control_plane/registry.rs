//! File-based spec registry (§5/§12, P1): every Jev decision point in the
//! daemon is one row here — a typed question, its confidence bands, its
//! mode and lifecycle — not a hand-built request scattered across call
//! sites.
//!
//! Bundled specs live in `assets/specs/*.json`, walked and `include_str!`'d
//! by `build.rs` into [`BUNDLED_SPECS`] the same way `assets/patterns` is
//! (`build.rs::emit_bundled_patterns`) — a fresh install has the full set
//! offline, and adding a spec is dropping a file in. A file under
//! `~/.senclaw/registry/specs/<id>.json` fully replaces the bundled
//! definition of that id (or defines a new candidate spec the user authors);
//! there is no field-level merge, so every override file is a complete, valid
//! spec on its own.
//!
//! `route.skill` and `tool.risk` are *descriptive* wrappers ([`Spec::wraps_existing`]):
//! their `mode` mirrors whatever `decisionConfig.skills`/`decisionConfig.gate`
//! already say, and [`SpecRegistry::set_mode`] refuses to touch them —
//! change those through `/api/decision/skills` / `/api/decision/gate`, the
//! single source of truth for behaviour that predates this registry.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::decision::json::Json;

include!(concat!(env!("OUT_DIR"), "/bundled_specs.rs"));

/// A spec id is a dot-namespaced identifier (`"input.guard.override"`), not a
/// filesystem path — but [`SpecRegistry::set_mode`] and the bundled-spec
/// loader both use it directly as a filename component (`<id>.json`), so it
/// must be validated as one. `.` is the legitimate namespace separator (a
/// plain [`super::is_safe_path_component`] would wrongly reject every real
/// spec id), so this checks per-segment instead: every `.`-separated segment
/// must be non-empty and contain only `[A-Za-z0-9_-]`. That single rule
/// rejects a path separator (never a valid segment character), an absolute
/// path (a leading `/` cannot start a valid segment), and `..` (which
/// decomposes into an empty segment either on its own or next to a real one)
/// all in one check — the same reasoning `is_safe_path_component` uses for a
/// non-namespaced id, adapted for the one place a dot is expected.
fn is_valid_spec_id(id: &str) -> bool {
    !id.is_empty()
        && id.split('.').all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpecType {
    Noul,
    Choice,
    Score,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SpecMode {
    #[default]
    Off,
    Shadow,
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Candidate,
    Shadow,
    Active,
    Decaying,
    Retired,
    Invalid,
}

/// Confidence bands (§5 "Băng tin cậy"): `< fallback` → review/human,
/// `fallback..act` → fallback to the LLM, `>= act` → act on the answer.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bands {
    pub act: f64,
    pub fallback: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    Act,
    Fallback,
    Review,
}

impl Band {
    pub fn as_str(self) -> &'static str {
        match self {
            Band::Act => "act",
            Band::Fallback => "fallback",
            Band::Review => "review",
        }
    }
}

impl Bands {
    pub fn classify(&self, confidence: f64) -> Band {
        if confidence >= self.act {
            Band::Act
        } else if confidence >= self.fallback {
            Band::Fallback
        } else {
            Band::Review
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    pub id: String,
    pub version: u32,
    #[serde(rename = "type")]
    pub qtype: SpecType,
    pub question: String,
    #[serde(default)]
    pub options: Option<Json>,
    #[serde(default)]
    pub levels: Option<Json>,
    /// Documentary only: names what the caller must put in
    /// `state`. Not structurally enforced — the caller builds `state` itself
    /// (see `ladder::ask`).
    #[serde(default)]
    pub state_fields: Vec<String>,
    pub bands: Bands,
    pub on_uncertain: String,
    #[serde(default)]
    pub mode: SpecMode,
    pub lifecycle: Lifecycle,
    pub lang: String,
    /// True for `route.skill` / `tool.risk`: this row documents an existing
    /// decision point (the pre-skill router, the tool-call gate) rather than
    /// owning one — `options`/`levels` may be absent (built dynamically per
    /// call from the live skill catalog / tool kind list) and `mode` is a
    /// read-only mirror of the settings that actually drive it.
    #[serde(default)]
    pub wraps_existing: bool,
}

impl Spec {
    pub fn qtype_str(&self) -> &'static str {
        match self.qtype {
            SpecType::Noul => "noul",
            SpecType::Choice => "choice",
            SpecType::Score => "score",
        }
    }

    /// Strict validation — a spec that fails this is dropped with a named
    /// reason rather than silently accepted half-broken (§3 "Registry dạng
    /// ACE": entries are ID-stable and merged by deterministic logic, not by
    /// an LLM rewriting the block, which is exactly why a syntax mistake must
    /// be loud instead of producing a spec that answers with garbage bands).
    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("spec id must not be empty".into());
        }
        if !is_valid_spec_id(&self.id) {
            return Err(format!(
                "{:?}: id must be dot-separated segments of [A-Za-z0-9_-] only (no '/', '\\\\', or empty segment \
                 like '..') — it is used as a filename (`<id>.json`)",
                self.id
            ));
        }
        if self.version == 0 {
            return Err(format!("{}: version must be >= 1", self.id));
        }
        if self.question.trim().is_empty() {
            return Err(format!("{}: question must not be empty", self.id));
        }
        if !self.wraps_existing {
            match self.qtype {
                SpecType::Choice => {
                    let n = self.options.as_ref().and_then(Json::as_object).map(<[_]>::len).unwrap_or(0);
                    if n == 0 {
                        return Err(format!("{}: a choice spec needs at least one option", self.id));
                    }
                }
                SpecType::Score => {
                    let n = match &self.levels {
                        Some(Json::Array(items)) => items.len(),
                        _ => 0,
                    };
                    if n == 0 {
                        return Err(format!("{}: a score spec needs at least one level", self.id));
                    }
                }
                SpecType::Noul => {}
            }
        }
        for (name, v) in [("act", self.bands.act), ("fallback", self.bands.fallback)] {
            if !(0.0..=1.0).contains(&v) {
                return Err(format!("{}: bands.{name} must be in [0, 1], got {v}", self.id));
            }
        }
        if self.bands.fallback > self.bands.act {
            return Err(format!(
                "{}: bands.fallback ({}) must be <= bands.act ({})",
                self.id, self.bands.fallback, self.bands.act
            ));
        }
        if self.on_uncertain.trim().is_empty() {
            return Err(format!("{}: on_uncertain must say what happens below the fallback band", self.id));
        }
        if self.lang.trim().is_empty() {
            return Err(format!("{}: lang must not be empty", self.id));
        }
        Ok(())
    }
}

pub struct BundledSpec {
    pub id: &'static str,
    pub json: &'static str,
}

fn user_specs_dir() -> PathBuf {
    std::env::var("SENCLAW_REGISTRY_SPECS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| super::senclaw_home().join("registry").join("specs"))
}

#[derive(Debug, Default)]
pub struct SpecRegistry {
    by_id: BTreeMap<String, Spec>,
    /// Parse/validation failures kept for `GET /api/control-plane/specs` —
    /// visible instead of a silently smaller registry.
    pub errors: Vec<String>,
}

fn parse_and_validate(id_hint: &str, text: &str) -> Result<Spec, String> {
    let spec: Spec = serde_json::from_str(text).map_err(|e| format!("{id_hint}: {e}"))?;
    spec.validate()?;
    Ok(spec)
}

impl SpecRegistry {
    /// Bundled specs, then user overrides layered on top by id — full
    /// replace, never a field merge (see module docs).
    pub fn load() -> SpecRegistry {
        let mut reg = SpecRegistry::default();
        for b in BUNDLED_SPECS {
            match parse_and_validate(b.id, b.json) {
                Ok(spec) => {
                    reg.by_id.insert(spec.id.clone(), spec);
                }
                Err(e) => reg.errors.push(format!("bundled {e}")),
            }
        }
        reg.load_user_overrides(&user_specs_dir());
        reg
    }

    fn load_user_overrides(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return; // no override dir yet — bundled-only is a normal state
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        files.sort();
        for path in files {
            let name = path.display().to_string();
            match std::fs::read_to_string(&path) {
                Ok(text) => match parse_and_validate(&name, &text) {
                    Ok(spec) => {
                        self.by_id.insert(spec.id.clone(), spec);
                    }
                    Err(e) => self.errors.push(format!("override {e}")),
                },
                Err(e) => self.errors.push(format!("override {name}: {e}")),
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<&Spec> {
        self.by_id.get(id)
    }

    pub fn list(&self) -> Vec<&Spec> {
        self.by_id.values().collect()
    }

    /// Persist a mode change as a full user-override file (§ module docs).
    /// Refuses for a `wraps_existing` spec — its mode is not this registry's
    /// to set.
    /// `id` reaches here as a raw axum path param (`PUT
    /// /api/control-plane/specs/:id/mode`), the same class of input as the
    /// trace-by-id route —
    /// `self.by_id.get(id)` already constrains it to an id that passed
    /// [`Spec::validate`] (every entry in `by_id` came from there, either a
    /// bundled spec or a previously-loaded override), so `id` is provably
    /// already filename-safe by the time it reaches the join below. The
    /// explicit re-check is defense in depth: it does not rely on that
    /// invariant holding forever, and it fails with a clear message instead
    /// of a `write` erroring on an unexpected path if it ever does not.
    pub fn set_mode(&mut self, id: &str, mode: SpecMode) -> Result<(), String> {
        let spec = self
            .by_id
            .get(id)
            .ok_or_else(|| format!("no spec {id:?} in the registry"))?;
        if spec.wraps_existing {
            return Err(format!(
                "{id:?} wraps an existing decision point — change its mode at /api/decision/gate \
                 or /api/decision/skills, not here"
            ));
        }
        if !is_valid_spec_id(id) {
            return Err(format!("{id:?} is not a valid spec id"));
        }
        let mut updated = spec.clone();
        updated.mode = mode;
        let dir = user_specs_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
        let path = dir.join(format!("{id}.json"));
        if path.parent() != Some(dir.as_path()) {
            return Err(format!("{id:?} would not write inside {}", dir.display()));
        }
        let text = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
        std::fs::write(&path, text).map_err(|e| format!("could not write {}: {e}", path.display()))?;
        self.by_id.insert(id.to_string(), updated);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noul(id: &str, mode: SpecMode) -> Spec {
        Spec {
            id: id.into(),
            version: 1,
            qtype: SpecType::Noul,
            question: "does it hold?".into(),
            options: None,
            levels: None,
            state_fields: vec!["query".into()],
            bands: Bands { act: 0.8, fallback: 0.5 },
            on_uncertain: "narrow permissions".into(),
            mode,
            lifecycle: Lifecycle::Shadow,
            lang: "en".into(),
            wraps_existing: false,
        }
    }

    #[test]
    fn every_bundled_spec_parses_and_validates() {
        // `SpecRegistry::load()` also reads `SENCLAW_REGISTRY_SPECS_DIR` (or
        // the real `~/.senclaw/registry/specs`) — held so a concurrently
        // running test cannot redirect it mid-read (see
        // `control_plane::env_test_guard`'s docs).
        let _guard = super::super::env_test_guard();
        let reg = SpecRegistry::load();
        assert!(reg.errors.is_empty(), "bundled spec errors: {:?}", reg.errors);
        assert!(reg.list().len() >= 6, "expected at least the 6 §5 specs, got {}", reg.list().len());
        for id in ["route.skill", "tool.risk", "clarify.needed", "task.done", "loop.next_step"] {
            assert!(reg.get(id).is_some(), "missing bundled spec {id:?}");
        }
    }

    #[test]
    fn route_skill_and_tool_risk_wrap_existing_behaviour() {
        let _guard = super::super::env_test_guard();
        let reg = SpecRegistry::load();
        for id in ["route.skill", "tool.risk"] {
            let s = reg.get(id).unwrap();
            assert!(s.wraps_existing, "{id} must be flagged wraps_existing");
        }
        for id in ["input.guard.override", "input.guard.scope", "input.guard.sensitive", "clarify.needed", "task.done", "loop.next_step"] {
            let s = reg.get(id).unwrap_or_else(|| panic!("missing {id}"));
            assert_eq!(s.mode, SpecMode::Shadow, "{id} must ship in shadow, not active");
        }
    }

    #[test]
    fn bands_classify_at_the_documented_edges() {
        let b = Bands { act: 0.8, fallback: 0.5 };
        assert_eq!(b.classify(0.9), Band::Act);
        assert_eq!(b.classify(0.8), Band::Act);
        assert_eq!(b.classify(0.6), Band::Fallback);
        assert_eq!(b.classify(0.5), Band::Fallback);
        assert_eq!(b.classify(0.1), Band::Review);
    }

    #[test]
    fn validation_rejects_the_documented_mistakes() {
        let mut s = noul("x", SpecMode::Off);
        s.bands.fallback = 0.9;
        s.bands.act = 0.5;
        assert!(s.validate().unwrap_err().contains("fallback"));

        let mut s = noul("x", SpecMode::Off);
        s.on_uncertain.clear();
        assert!(s.validate().unwrap_err().contains("on_uncertain"));

        let mut s = Spec { qtype: SpecType::Choice, ..noul("x", SpecMode::Off) };
        assert!(s.validate().unwrap_err().contains("at least one option"));
        s.options = Some(Json::Object(vec![("a".into(), Json::Null)]));
        assert!(s.validate().is_ok());

        let mut s = Spec { qtype: SpecType::Score, ..noul("x", SpecMode::Off) };
        assert!(s.validate().unwrap_err().contains("at least one level"));
        s.levels = Some(Json::Array(vec![Json::String("low".into())]));
        assert!(s.validate().is_ok());
    }

    #[test]
    fn is_valid_spec_id_accepts_real_ids_and_rejects_traversal_shapes() {
        for good in ["route.skill", "tool.risk", "input.guard.override", "loop.next_step", "a", "a-b_c"] {
            assert!(is_valid_spec_id(good), "{good:?} must be accepted");
        }
        for bad in ["", "..", "../../etc/evil", "a/b", "a\\b", "a..b", ".hidden", "trailing.", "a.", ".a", "/etc"] {
            assert!(!is_valid_spec_id(bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn validate_rejects_a_spec_id_shaped_like_a_traversal() {
        // `id` is used verbatim as a filename (`<id>.json`) by
        // both the bundled-spec loader and `set_mode`. A spec whose `id`
        // could escape that join must never parse successfully, regardless
        // of where the JSON came from.
        let bad = Spec { id: "../../etc/evil".into(), ..noul("x", SpecMode::Off) };
        let err = bad.validate().unwrap_err();
        assert!(err.contains("../../etc/evil"), "{err}");
    }

    #[test]
    fn a_malicious_override_file_is_rejected_not_loaded_into_the_registry() {
        // Simulates a manually-placed file in `~/.senclaw/registry/specs/`
        // whose *content* (not filename) carries a traversal-shaped id — the
        // scenario that would otherwise let `set_mode` be reached with an
        // unsafe id at all. `parse_and_validate` must refuse it before it
        // ever reaches `by_id`.
        let _guard = super::super::env_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("evil.json"),
            r#"{"id": "../../etc/evil", "version": 1, "type": "noul", "question": "x",
                "bands": {"act": 0.8, "fallback": 0.5}, "on_uncertain": "no",
                "lifecycle": "shadow", "lang": "en", "mode": "active"}"#,
        )
        .unwrap();
        std::env::set_var("SENCLAW_REGISTRY_SPECS_DIR", tmp.path());
        let reg = SpecRegistry::load();
        assert!(reg.get("../../etc/evil").is_none(), "a traversal-shaped id must never enter the registry");
        assert!(reg.errors.iter().any(|e| e.contains("../../etc/evil")), "the rejection must be visible: {:?}", reg.errors);
        std::env::remove_var("SENCLAW_REGISTRY_SPECS_DIR");
    }

    #[test]
    fn a_wrapping_choice_spec_may_omit_options() {
        let s = Spec { qtype: SpecType::Choice, wraps_existing: true, ..noul("route.skill", SpecMode::Active) };
        assert!(s.validate().is_ok());
    }

    #[test]
    fn set_mode_persists_a_full_override_and_refuses_for_wrapped_specs() {
        let _guard = super::super::env_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("SENCLAW_REGISTRY_SPECS_DIR", tmp.path());
        let mut reg = SpecRegistry::load();

        assert!(reg.set_mode("tool.risk", SpecMode::Active).unwrap_err().contains("/api/decision/gate"));

        reg.set_mode("clarify.needed", SpecMode::Active).unwrap();
        assert_eq!(reg.get("clarify.needed").unwrap().mode, SpecMode::Active);
        let path = tmp.path().join("clarify.needed.json");
        assert!(path.is_file());
        let reloaded = SpecRegistry::load();
        assert_eq!(reloaded.get("clarify.needed").unwrap().mode, SpecMode::Active, "survives a fresh load");

        std::env::remove_var("SENCLAW_REGISTRY_SPECS_DIR");
    }

    #[test]
    fn a_broken_override_file_is_reported_not_silently_dropped() {
        let _guard = super::super::env_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("broken.json"), "{not json").unwrap();
        std::env::set_var("SENCLAW_REGISTRY_SPECS_DIR", tmp.path());
        let reg = SpecRegistry::load();
        assert!(reg.errors.iter().any(|e| e.contains("broken.json")));
        std::env::remove_var("SENCLAW_REGISTRY_SPECS_DIR");
    }
}
