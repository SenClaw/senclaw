//! `senclaw scan` — run the pre-install security scan by hand.
//!
//! The same engine the install paths use, pointed at a package you have not
//! installed yet. Exists so "should I install this?" can be answered before
//! committing to the install, and so the check can run in CI.
//!
//! Exit status is the machine-readable part: `0` allow/warn, `1` block. That
//! makes `senclaw scan ./pkg || exit 1` a usable gate in a pipeline.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;

use crate::config::Config;
use crate::security::scan::{self, ScanPolicy, ScanReport, Verdict};

#[derive(Args, Debug)]
pub struct ScanCmd {
    /// Directory or `.zip` to inspect
    path: PathBuf,

    /// Emit the full report as JSON
    #[arg(long)]
    json: bool,

    /// Severity that counts as a block: info|low|medium|high|critical.
    /// Defaults to the daemon's SENCLAW_SCAN_BLOCK_LEVEL.
    #[arg(long)]
    block_level: Option<String>,
}

pub async fn run(cmd: ScanCmd) -> Result<()> {
    let mut policy = ScanPolicy::from_config(&Config::from_env());
    // `scan` is an explicit request to scan, so an operator who disabled the
    // automatic gate still gets a report here.
    policy.enabled = true;
    if let Some(level) = &cmd.block_level {
        policy.block_at = scan::Severity::parse(level)
            .with_context(|| format!("invalid --block-level {level:?}"))?;
    }

    let (report, _staging) = scan_path(&cmd.path)?;

    if cmd.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "target": report.target,
                "verdict": report.verdict(&policy),
                "risk": report.risk_score(),
                "filesScanned": report.files_scanned,
                "truncated": report.truncated,
                "findings": report.findings,
            }))?
        );
    } else {
        println!("{}", report.summary());
        println!();
        match report.verdict(&policy) {
            Verdict::Allow => println!("Verdict: allow — nothing flagged."),
            Verdict::Warn => println!(
                "Verdict: warn — installs by default, but review the findings above."
            ),
            Verdict::Block => println!(
                "Verdict: BLOCK — install refuses this package unless forced."
            ),
        }
    }

    if report.verdict(&policy) == Verdict::Block {
        std::process::exit(1);
    }
    Ok(())
}

/// Temp directory that cleans itself up. Held by the caller for the lifetime of
/// the report so an extracted zip is not deleted while still being read.
struct Staging(Option<PathBuf>);

impl Drop for Staging {
    fn drop(&mut self) {
        if let Some(dir) = &self.0 {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// What this daemon would do with command hooks shipped by a package of this
/// name, so a hand-run scan predicts the real install rather than a
/// hypothetical one.
fn hook_disposition(name: &str) -> scan::CommandHookDisposition {
    crate::agent::MarketplaceHookPolicy::from_config(&Config::from_env()).disposition_for(name)
}

fn scan_path(path: &Path) -> Result<(ScanReport, Staging)> {
    if !path.exists() {
        anyhow::bail!("No such path: {}", path.display());
    }

    if path.is_dir() {
        let name = package_name(path);
        return match read_manifest_from_dir(path) {
            Some(manifest) => Ok((
                scan::scan_space_app(path, &manifest, &name),
                Staging(None),
            )),
            None => Ok((
                scan::scan_plugin_dir_with(path, &name, hook_disposition(&name)),
                Staging(None),
            )),
        };
    }

    let is_zip = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"));
    if !is_zip {
        anyhow::bail!(
            "Expected a directory or a .zip, got {}",
            path.display()
        );
    }

    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let staging = std::env::temp_dir().join(format!("senclaw-scan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    crate::clawhub::lockfile::extract_zip_to_dir(&bytes, &staging)
        .with_context(|| format!("extract {}", path.display()))?;
    let guard = Staging(Some(staging.clone()));

    let name = package_name(path);
    let report = match read_manifest_from_dir(&staging) {
        Some(manifest) => scan::scan_space_app(&staging, &manifest, &name),
        None => scan::scan_plugin_dir_with(&staging, &name, hook_disposition(&name)),
    };
    Ok((report, guard))
}

/// A Space App is identified by its manifest; anything else is scanned as a
/// plugin tree. Getting this wrong only changes which manifest rules run — the
/// content rules are the same either way.
fn read_manifest_from_dir(dir: &Path) -> Option<serde_json::Value> {
    for name in ["senclaw-manifest.json", "senclaw-app.json"] {
        if let Ok(raw) = std::fs::read_to_string(dir.join(name)) {
            if let Ok(v) = serde_json::from_str(&raw) {
                return Some(v);
            }
        }
    }
    None
}

fn package_name(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::scan::TargetKind;
    use std::fs;

    fn tmpdir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("senclaw-scancli-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn plain_directory_scans_as_a_plugin() {
        let dir = tmpdir("plugin");
        fs::write(dir.join("README.md"), "nothing to see").unwrap();
        let (report, _g) = scan_path(&dir).unwrap();
        assert_eq!(report.kind, TargetKind::Plugin);
        assert!(report.findings.is_empty());
    }

    #[test]
    fn directory_with_manifest_scans_as_a_space_app() {
        let dir = tmpdir("app");
        fs::write(
            dir.join("senclaw-manifest.json"),
            r#"{"id":"demo","runtime":{"kind":"server","start":"node s.js"}}"#,
        )
        .unwrap();
        let (report, _g) = scan_path(&dir).unwrap();

        assert_eq!(report.kind, TargetKind::SpaceApp);
        // The manifest rule only runs on the Space App path — this is what
        // distinguishes the two modes.
        assert!(report.findings.iter().any(|f| f.rule == "EXEC001"));
    }

    #[test]
    fn missing_path_is_an_error_not_a_clean_report() {
        let missing = tmpdir("gone").join("nope");
        assert!(scan_path(&missing).is_err());
    }

    #[test]
    fn non_zip_file_is_rejected() {
        let dir = tmpdir("notzip");
        let f = dir.join("thing.tar.gz");
        fs::write(&f, "x").unwrap();
        assert!(scan_path(&f).is_err());
    }
}
