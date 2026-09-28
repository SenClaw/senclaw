//! `config.json` → `browserAgent`. Read per request (no restart), every field
//! optional with a safe default: local decisions, managed Chrome, no hosted
//! domains.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::decide::Bands;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Engine {
    /// v2 when the `sen-browser` runtime is installed, else the legacy extension flow.
    #[default]
    Auto,
    Legacy,
    V2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Driver {
    /// A Chrome the runtime launches with its own profile.
    #[default]
    Managed,
    /// The person's Chrome through the SenClaw extension.
    Extension,
}

impl Driver {
    pub fn as_str(self) -> &'static str {
        match self {
            Driver::Managed => "managed",
            Driver::Extension => "extension",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum DecisionBackend {
    /// Local laya-browser everywhere; hosted Jev only for `hostedDomains`.
    #[default]
    Auto,
    Local,
    Hosted,
    /// No decision model: the LLM picks every step (the ablation baseline).
    LlmOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BrowserSettings {
    pub engine: Engine,
    pub default_driver: Driver,
    pub decision_backend: DecisionBackend,
    /// Checkpoint id `sen-sysone` loads for local decisions.
    pub local_model: String,
    /// Upstream model for hosted decisions (`None` = the one configured in sen-sysone).
    pub hosted_model: Option<String>,
    /// Domains whose page text may go to the hosted decision model.
    pub hosted_domains: Vec<String>,
    /// Extra domains treated as sensitive (always local, stricter policy).
    pub sensitive_domains: Vec<String>,
    /// Domain → driver, e.g. `{"mail.google.com": "extension"}`.
    pub domain_drivers: BTreeMap<String, Driver>,
    /// LLM config id for writing field values (`None` = the active chat model).
    pub text_model: Option<String>,
    /// LLM config id for the fallback tier, verification and answers.
    pub fallback_model: Option<String>,
    pub bands_local: Bands,
    pub bands_hosted: Bands,
    pub max_steps: u32,
    pub headless: bool,
    pub profile: String,
    /// Where a task without a URL starts.
    pub start_url: String,
}

impl Default for BrowserSettings {
    fn default() -> Self {
        BrowserSettings {
            engine: Engine::Auto,
            default_driver: Driver::Managed,
            decision_backend: DecisionBackend::Auto,
            local_model: "laya-browser".into(),
            hosted_model: None,
            hosted_domains: Vec::new(),
            sensitive_domains: Vec::new(),
            domain_drivers: BTreeMap::new(),
            text_model: None,
            fallback_model: None,
            bands_local: Bands { act: 0.6, fallback: 0.2 },
            bands_hosted: Bands { act: 0.5, fallback: 0.25 },
            max_steps: 40,
            headless: true,
            profile: "default".into(),
            start_url: "https://duckduckgo.com/".into(),
        }
    }
}

impl BrowserSettings {
    /// Refuse values the loop cannot use, with a message for a person.
    pub fn validated(self) -> Result<BrowserSettings, String> {
        let name_ok = |s: &str| !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
        if !name_ok(&self.profile) {
            return Err("profile: use lowercase letters, digits, - or _ (at most 64)".into());
        }
        if !(1..=120).contains(&self.max_steps) {
            return Err("maxSteps: between 1 and 120".into());
        }
        for (name, b) in [("bandsLocal", self.bands_local), ("bandsHosted", self.bands_hosted)] {
            if !(0.0..=1.0).contains(&b.act) || !(0.0..=1.0).contains(&b.fallback) || b.fallback > b.act {
                return Err(format!("{name}: 0 ≤ fallback ≤ act ≤ 1"));
            }
        }
        if self.local_model.trim().is_empty() {
            return Err("localModel: name the decision checkpoint (for example laya-browser)".into());
        }
        let start = self.start_url.trim();
        if !(start.starts_with("https://") || start.starts_with("http://")) {
            return Err("startUrl: an http(s) address".into());
        }
        let domain_ok = |d: &String| !d.trim().is_empty() && !d.contains('/') && !d.contains(' ');
        if !self.hosted_domains.iter().all(domain_ok) || !self.sensitive_domains.iter().all(domain_ok) || !self.domain_drivers.keys().all(domain_ok) {
            return Err("domains: host names only (example.com), no paths or spaces".into());
        }
        Ok(self)
    }
}

/// Read `browserAgent` from `config.json`; anything missing or malformed is the default.
pub fn load(config_path: &Path) -> BrowserSettings {
    std::fs::read_to_string(config_path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("browserAgent").cloned())
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

/// Whether the `sen-browser` runtime is installed under `senclaw_home`.
pub fn runtime_installed(senclaw_home: &Path) -> bool {
    let dir = senclaw_home.join("runtimes").join("sen-browser");
    std::fs::read_dir(dir)
        .map(|entries| entries.flatten().any(|e| e.path().join("senclaw-runtime.json").is_file()))
        .unwrap_or(false)
}

/// The engine the agent's browser tools use, with `auto` resolved.
pub fn resolved_engine(config_path: &Path, senclaw_home: &Path) -> Engine {
    match load(config_path).engine {
        Engine::Auto if runtime_installed(senclaw_home) => Engine::V2,
        Engine::Auto => Engine::Legacy,
        other => other,
    }
}

/// [`resolved_engine`] for a daemon whose home is the directory holding `config_path`.
pub fn engine_at(config_path: &Path) -> Engine {
    resolved_engine(config_path, config_path.parent().unwrap_or(Path::new(".")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_a_person_cannot_mean_are_refused() {
        assert!(BrowserSettings::default().validated().is_ok());
        let bad = |f: fn(&mut BrowserSettings)| {
            let mut s = BrowserSettings::default();
            f(&mut s);
            s.validated().is_err()
        };
        assert!(bad(|s| s.profile = "../x".into()));
        assert!(bad(|s| s.max_steps = 0));
        assert!(bad(|s| s.bands_local = Bands { act: 0.3, fallback: 0.5 }));
        assert!(bad(|s| s.start_url = "file:///etc".into()));
        assert!(bad(|s| s.hosted_domains = vec!["example.com/path".into()]));
        assert!(bad(|s| s.local_model = " ".into()));
    }

    #[test]
    fn defaults_are_local_and_managed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        assert_eq!(load(&path).decision_backend, DecisionBackend::Auto);
        std::fs::write(&path, r#"{"browserAgent": {"defaultDriver": "extension", "hostedDomains": ["example.com"], "maxSteps": 12}}"#).unwrap();
        let s = load(&path);
        assert_eq!(s.default_driver, Driver::Extension);
        assert_eq!(s.hosted_domains, vec!["example.com"]);
        assert_eq!(s.max_steps, 12);
        assert_eq!(s.local_model, "laya-browser");
        std::fs::write(&path, r#"{"browserAgent": "garbage"}"#).unwrap();
        assert_eq!(load(&path).max_steps, 40);
        assert_eq!(resolved_engine(&path, dir.path()), Engine::Legacy);
        let pkg = dir.path().join("runtimes/sen-browser/0.1.0");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("senclaw-runtime.json"), "{}").unwrap();
        assert_eq!(resolved_engine(&path, dir.path()), Engine::V2);
    }
}
