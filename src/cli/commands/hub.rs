//! `senclaw hub …` — publish Space Apps to the senclaw hub.
//!
//! The hub enforces immutable `name@version`, so the flow is deliberately
//! preflight-heavy: everything that can be checked locally or with one cheap
//! GET is checked *before* the artifact is uploaded. A 409 after a multi-megabyte
//! upload is a wasted minute and a confusing error; a "version 1.0.0 đã tồn tại"
//! before it starts is actionable.

use crate::marketplace::publish::{self, HubPackage, HUB_FILE, MAX_UPLOAD_BYTES};
use crate::marketplace::registry;
use anyhow::{bail, Context, Result};
use clap::Subcommand;
use std::path::{Path, PathBuf};

#[derive(Subcommand, Debug)]
pub enum HubCmd {
    /// Store a publish token (read from stdin, never from an argument)
    Login,
    /// Show which token file is in use, and whether it works
    Whoami,
    /// Create senclaw-hub.json for an app from its runtime manifest
    Init {
        /// Path to the app directory, e.g. apps/ai-office
        app: PathBuf,
        /// Initial version
        #[arg(long, default_value = "1.0.0")]
        version: String,
    },
    /// Raise the version in an app's senclaw-hub.json
    Bump {
        app: PathBuf,
        /// major | minor | patch
        #[arg(default_value = "patch")]
        part: String,
    },
    /// Show what would be published, without uploading
    Status { app: PathBuf },
    /// Publish an app version to the hub
    Publish {
        app: PathBuf,
        /// Check everything and stop before uploading
        #[arg(long)]
        dry_run: bool,
        /// Build the artifact first via the app's scripts/pack.sh
        #[arg(long)]
        pack: bool,
        /// Override the hub base URL
        #[arg(long)]
        hub: Option<String>,
    },
    /// Show a published package: versions, platforms, artifacts
    Info {
        /// `<scope>/<name>` or just `<name>` (scope defaults to senclaw)
        slug: String,
        #[arg(long)]
        hub: Option<String>,
    },
    /// Install a published app into the running daemon
    Install {
        /// `<scope>/<name>` or just `<name>`
        slug: String,
        /// Version to install; default is the `latest` dist-tag
        #[arg(long)]
        version: Option<String>,
        /// Artifact platform to fetch; default is this machine's
        #[arg(long)]
        platform: Option<String>,
        /// Download and verify, then stop without installing
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        hub: Option<String>,
    },
    /// List installed apps that have a newer version on the hub
    Outdated,
    /// Update installed apps to the hub's latest version (in place)
    Update {
        /// App id to update; omit with --all to update everything outdated
        id: Option<String>,
        /// Update every app that has an available update
        #[arg(long)]
        all: bool,
    },
}

/// The app's runtime manifest — the source of truth for id and description, so
/// those never drift between the two files.
#[derive(Debug)]
struct AppInfo {
    id: String,
    description: String,
    dir: PathBuf,
}

fn read_app(dir: &Path) -> Result<AppInfo> {
    let path = dir.join("senclaw-manifest.json");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("không đọc được {}", path.display()))?;
    let v: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("{} không phải JSON hợp lệ", path.display()))?;
    let id = v
        .get("id")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    if id.is_empty() {
        bail!("{} thiếu trường `id`", path.display());
    }
    Ok(AppInfo {
        id,
        description: v
            .get("description")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        dir: dir.to_path_buf(),
    })
}

fn hub_path(dir: &Path) -> PathBuf {
    dir.join(HUB_FILE)
}

fn read_hub_pkg(dir: &Path) -> Result<HubPackage> {
    let path = hub_path(dir);
    let raw = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "{} chưa có. Tạo bằng: senclaw hub init {}",
            path.display(),
            dir.display()
        )
    })?;
    let pkg: HubPackage =
        serde_json::from_str(&raw).with_context(|| format!("{} không hợp lệ", path.display()))?;
    Ok(pkg)
}

fn write_hub_pkg(dir: &Path, pkg: &HubPackage) -> Result<()> {
    let path = hub_path(dir);
    std::fs::write(&path, format!("{}\n", serde_json::to_string_pretty(pkg)?))?;
    Ok(())
}

/// Resolve the artifact path, defaulting to `<id>-app.zip` as pack.sh produces.
fn artifact_path(app: &AppInfo, pkg: &HubPackage) -> PathBuf {
    match &pkg.artifact {
        Some(rel) => app.dir.join(rel),
        None => app.dir.join(format!("{}-app.zip", app.id)),
    }
}

pub async fn run(cmd: HubCmd) -> Result<()> {
    match cmd {
        HubCmd::Login => login(),
        HubCmd::Whoami => whoami(),
        HubCmd::Init { app, version } => init(&app, &version),
        HubCmd::Bump { app, part } => bump(&app, &part),
        HubCmd::Status { app } => status(&app).await,
        HubCmd::Publish {
            app,
            dry_run,
            pack,
            hub,
        } => do_publish(&app, dry_run, pack, hub).await,
        HubCmd::Info { slug, hub } => info(&slug, hub).await,
        HubCmd::Install {
            slug,
            version,
            platform,
            dry_run,
            hub,
        } => install(&slug, version.as_deref(), platform, dry_run, hub).await,
        HubCmd::Outdated => outdated().await,
        HubCmd::Update { id, all } => update(id.as_deref(), all).await,
    }
}

/// Read the token from stdin so it never appears in shell history, `ps` output,
/// or a CI log of the command line.
fn login() -> Result<()> {
    use std::io::Read;
    eprintln!("Dán publish token (snc_pat_…) rồi Enter, hoặc pipe vào stdin:");
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    let path = publish::write_token(&buf)?;
    println!("Đã lưu token vào {} (chmod 600)", path.display());
    Ok(())
}

fn whoami() -> Result<()> {
    let path = publish::token_path();
    match publish::read_token() {
        Ok(t) => {
            // Never print the token. Enough to confirm which one is loaded.
            let tail = t.chars().rev().take(4).collect::<String>();
            let tail: String = tail.chars().rev().collect();
            let src = if std::env::var("SENCLAW_HUB_TOKEN").is_ok() {
                "biến môi trường SENCLAW_HUB_TOKEN".to_string()
            } else {
                path.display().to_string()
            };
            println!("Token đang dùng: …{tail}  (nguồn: {src})");
            println!("Hub: {}", publish::DEFAULT_HUB);
        }
        Err(e) => println!("{e}"),
    }
    Ok(())
}

fn init(dir: &Path, version: &str) -> Result<()> {
    read_app(dir)?; // validate the runtime manifest before writing anything
    let path = hub_path(dir);
    if path.exists() {
        bail!("{} đã tồn tại", path.display());
    }
    if !publish::is_semver(version) {
        bail!("`{version}` không phải semver (cần X.Y.Z)");
    }

    // Permissions are a security declaration shown to users before install, so
    // this scaffolds a NARROW default derived from the manifest rather than a
    // permissive guess. Widening it is a deliberate edit.
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("senclaw-manifest.json"))?)?;
    let start = manifest
        .get("runtime")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.as_str())
        .unwrap_or("./app")
        .to_string();

    let pkg = HubPackage {
        version: version.to_string(),
        category: None,
        keywords: vec![],
        permissions: Some(serde_json::json!({
            "network": ["127.0.0.1"],
            "exec": [start],
        })),
        homepage_url: None,
        repo_url: None,
        updater: "none".into(),
        platform: Some(publish::host_platform()),
        artifact: None,
    };
    write_hub_pkg(dir, &pkg)?;
    println!("Đã tạo {}", path.display());
    println!("  version:  {version}");
    println!("  platform: {}", publish::host_platform());
    println!(
        "\nHãy kiểm tra lại `permissions` trước khi publish — đó là phần người dùng \
         nhìn thấy trước khi cài. Thêm `category` và `keywords` nếu muốn dễ tìm hơn."
    );
    Ok(())
}

fn bump(dir: &Path, part: &str) -> Result<()> {
    let mut pkg = read_hub_pkg(dir)?;
    let old = pkg.version.clone();
    pkg.version = publish::bump(&old, part)?;
    write_hub_pkg(dir, &pkg)?;
    println!("{old} → {}", pkg.version);
    Ok(())
}

/// Everything `publish` checks, without uploading. Also the body of `--dry-run`.
async fn preflight(dir: &Path, hub: &str) -> Result<(AppInfo, HubPackage, PathBuf)> {
    let app = read_app(dir)?;
    let pkg = read_hub_pkg(dir)?;

    if !publish::is_semver(&pkg.version) {
        bail!(
            "version `{}` trong {} không phải semver — hub sẽ từ chối",
            pkg.version,
            hub_path(dir).display()
        );
    }
    if app.description.trim().is_empty() {
        bail!("senclaw-manifest.json thiếu `description` — hub bắt buộc có");
    }

    let artifact = artifact_path(&app, &pkg);
    if !artifact.exists() {
        bail!(
            "chưa có artifact {}. Chạy lại với --pack, hoặc: {}/scripts/pack.sh",
            artifact.display(),
            dir.display()
        );
    }
    let size = std::fs::metadata(&artifact)?.len();
    if size > MAX_UPLOAD_BYTES {
        bail!(
            "{} nặng {:.1} MB, vượt giới hạn {} MB của hub",
            artifact.display(),
            size as f64 / 1_048_576.0,
            MAX_UPLOAD_BYTES / 1_048_576
        );
    }

    // The expensive mistake is discovering the version is taken after uploading.
    match publish::published_versions(hub, "senclaw", &app.id).await {
        Ok(versions) => {
            if versions.iter().any(|v| v == &pkg.version) {
                bail!(
                    "senclaw/{}@{} đã publish rồi. name@version là BẤT BIẾN — \
                     chạy `senclaw hub bump {} patch` rồi thử lại.\n  Đã có: {}",
                    app.id,
                    pkg.version,
                    dir.display(),
                    versions.join(", ")
                );
            }
        }
        // A registry we cannot reach must not silently look like a free name;
        // say so and let the server be the authority.
        Err(e) => eprintln!("  ⚠ không kiểm tra được version đã có trên hub: {e}"),
    }

    Ok((app, pkg, artifact))
}

async fn status(dir: &Path) -> Result<()> {
    let hub = publish::DEFAULT_HUB.to_string();
    let app = read_app(dir)?;
    let pkg = read_hub_pkg(dir)?;
    let artifact = artifact_path(&app, &pkg);

    println!("senclaw/{}", app.id);
    println!("  version:   {}", pkg.version);
    println!(
        "  platform:  {}",
        pkg.platform.clone().unwrap_or_else(publish::host_platform)
    );
    println!(
        "  category:  {}",
        pkg.category.clone().unwrap_or_else(|| "—".into())
    );
    println!(
        "  keywords:  {}",
        if pkg.keywords.is_empty() {
            "—".into()
        } else {
            pkg.keywords.join(", ")
        }
    );
    if artifact.exists() {
        let bytes = std::fs::read(&artifact)?;
        println!(
            "  artifact:  {} ({:.2} MB)",
            artifact.display(),
            bytes.len() as f64 / 1_048_576.0
        );
        println!("  integrity: {}", publish::sha512_integrity(&bytes));
    } else {
        println!("  artifact:  {} (CHƯA CÓ)", artifact.display());
    }
    match publish::published_versions(&hub, "senclaw", &app.id).await {
        Ok(v) if v.is_empty() => println!("  trên hub:  chưa publish lần nào"),
        Ok(v) => println!("  trên hub:  {}", v.join(", ")),
        Err(e) => println!("  trên hub:  không kiểm tra được ({e})"),
    }
    Ok(())
}

async fn do_publish(dir: &Path, dry_run: bool, pack: bool, hub: Option<String>) -> Result<()> {
    let hub = hub.unwrap_or_else(|| publish::DEFAULT_HUB.to_string());

    if pack {
        let script = dir.join("scripts/pack.sh");
        if !script.exists() {
            bail!("không có {}", script.display());
        }
        println!("→ đóng gói…");
        let out = std::process::Command::new("bash").arg(&script).status()?;
        if !out.success() {
            bail!("pack.sh thất bại");
        }
    }

    let (app, pkg, artifact) = preflight(dir, &hub).await?;
    let bytes = std::fs::read(&artifact)?;
    let platform = pkg.platform.clone().unwrap_or_else(publish::host_platform);

    println!("senclaw/{}@{}", app.id, pkg.version);
    println!(
        "  artifact:  {} ({:.2} MB)",
        artifact.display(),
        bytes.len() as f64 / 1_048_576.0
    );
    println!("  platform:  {platform}");
    println!("  integrity: {}", publish::sha512_integrity(&bytes));

    if dry_run {
        println!("\n--dry-run: mọi kiểm tra đã qua, KHÔNG upload.");
        return Ok(());
    }

    // Read the token only once every local check has passed, so a missing token
    // is not the thing that hides a broken package.
    let token = publish::read_token()?;
    let readme = std::fs::read_to_string(dir.join("README.md")).ok();

    let extra = serde_json::json!({
        "app": { "updater": pkg.updater }
    });

    println!("\n→ đang upload…");
    let ok = publish::publish(publish::PublishRequest {
        hub: hub.clone(),
        token,
        kind: "app",
        name: &app.id,
        version: &pkg.version,
        description: &app.description,
        keywords: &pkg.keywords,
        category: pkg.category.as_deref(),
        permissions: pkg.permissions.as_ref(),
        repo_url: pkg.repo_url.as_deref(),
        homepage_url: pkg.homepage_url.as_deref(),
        readme,
        platform: Some(&platform),
        extra,
        artifact: &artifact,
    })
    .await?;

    println!("✓ đã publish {}@{}", ok.slug, ok.version);
    if !ok.url.is_empty() {
        println!("  {}", ok.url);
    }
    println!("  integrity (hub tự tính): {}", ok.integrity);
    Ok(())
}

// ── Install side ─────────────────────────────────────────────────────────────

async fn info(slug: &str, hub: Option<String>) -> Result<()> {
    let hub = hub.unwrap_or_else(|| publish::DEFAULT_HUB.to_string());
    let (scope, name) = registry::parse_slug(slug)?;
    let pkg = registry::fetch_package(&hub, &scope, &name).await?;

    println!("{}", pkg.slug);
    if let Some(d) = &pkg.description {
        println!("  {}", d.lines().next().unwrap_or_default());
    }
    println!(
        "  kind:   {}",
        pkg.kind.clone().unwrap_or_else(|| "—".into())
    );
    if let Some(latest) = pkg.dist_tags.get("latest") {
        println!("  latest: {latest}");
    }
    for v in pkg.versions_sorted() {
        let flags = match (v.yanked, v.deprecated.as_deref()) {
            (true, _) => "  [yanked]".to_string(),
            (_, Some(msg)) => format!("  [deprecated: {msg}]"),
            _ => String::new(),
        };
        println!("  {}{}", v.version, flags);
        for d in &v.dist {
            println!(
                "    {:<14} {:>8} KB  {}",
                d.platform.clone().unwrap_or_else(|| "any".into()),
                d.size.unwrap_or(0) / 1024,
                d.filename.clone().unwrap_or_default()
            );
        }
    }
    Ok(())
}

/// Where the daemon listens. The CLI does not extract the zip itself: the
/// daemon owns app registration, launching and MCP wiring, and duplicating that
/// here would produce apps it does not know about.
fn daemon_base() -> String {
    let cfg = crate::config::Config::from_env();
    format!("http://127.0.0.1:{}", cfg.ui_server.port)
}

async fn install(
    slug: &str,
    version: Option<&str>,
    platform: Option<String>,
    dry_run: bool,
    hub: Option<String>,
) -> Result<()> {
    let hub = hub.unwrap_or_else(|| publish::DEFAULT_HUB.to_string());
    let (scope, name) = registry::parse_slug(slug)?;
    let host = platform.unwrap_or_else(publish::host_platform);

    let pkg = registry::fetch_package(&hub, &scope, &name).await?;
    let ver = registry::resolve_version(&pkg, version)?;
    let dist = registry::select_dist(ver, &host)?;

    println!("{}@{}", pkg.slug, ver.version);
    println!(
        "  artifact: {} ({:.2} MB, {})",
        dist.filename.clone().unwrap_or_default(),
        dist.size.unwrap_or(0) as f64 / 1_048_576.0,
        dist.platform.clone().unwrap_or_else(|| "any".into())
    );
    if let Some(msg) = &ver.deprecated {
        println!("  ⚠ phiên bản này đã deprecated: {msg}");
    }

    println!("→ đang tải…");
    let bytes = registry::download_verified(dist).await?;
    println!("  integrity ✓ ({} bytes)", bytes.len());

    if dry_run {
        println!("--dry-run: đã tải và xác minh, KHÔNG cài.");
        return Ok(());
    }

    let base = daemon_base();
    let url = format!("{base}/api/space/apps/install-zip");
    let filename = dist
        .filename
        .clone()
        .unwrap_or_else(|| format!("{name}-app.zip"));
    // Stamp install provenance so `senclaw hub outdated` can later resolve the
    // source package and compare versions.
    let mut form = reqwest::multipart::Form::new()
        .part(
            "file",
            reqwest::multipart::Part::bytes(bytes).file_name(filename),
        )
        .text("slug", pkg.slug.clone())
        .text("version", ver.version.clone())
        .text("hub", hub.clone());
    if let Some(integrity) = dist.integrity.clone() {
        form = form.text("integrity", integrity);
    }

    println!("→ đang cài qua daemon {base}…");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(180))
        .build()?;
    let resp = client
        .post(&url)
        .multipart(form)
        .send()
        .await
        .with_context(|| {
            format!(
                "không gọi được {url} — daemon SenClaw có đang chạy không? \
             (đặt SENCLAW_UI_PORT nếu dùng cổng khác)"
            )
        })?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        let msg = body
            .get("error")
            .and_then(|v| v.as_str())
            .or_else(|| body.get("message").and_then(|v| v.as_str()))
            .unwrap_or("(daemon không kèm thông báo)");
        bail!("cài thất bại (HTTP {status}): {msg}");
    }

    let id = body
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or(name.as_str());
    println!("✓ đã cài {id}");
    Ok(())
}

/// Ask the daemon which installed apps have a newer version on the hub. The
/// daemon owns app state and hub access, so the CLI is a thin client over
/// `GET /api/space/apps/updates`.
async fn fetch_updates() -> Result<Vec<serde_json::Value>> {
    let base = daemon_base();
    let url = format!("{base}/api/space/apps/updates");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let resp =
        client.get(&url).send().await.with_context(|| {
            format!("không gọi được {url} — daemon SenClaw có đang chạy không?")
        })?;
    if !resp.status().is_success() {
        bail!("{url} trả về HTTP {}", resp.status());
    }
    Ok(resp
        .json::<Vec<serde_json::Value>>()
        .await
        .unwrap_or_default())
}

async fn outdated() -> Result<()> {
    let updates = fetch_updates().await?;
    let outdated: Vec<&serde_json::Value> = updates
        .iter()
        .filter(|u| {
            u.get("hasUpdate")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
        .collect();

    if outdated.is_empty() {
        println!("Mọi app đã ở phiên bản mới nhất.");
        // Surface apps we could not check, so a silent registry error is not
        // mistaken for "all up to date".
        for u in updates.iter().filter(|u| u.get("error").is_some()) {
            if let (Some(id), Some(err)) = (
                u.get("id").and_then(|v| v.as_str()),
                u.get("error").and_then(|v| v.as_str()),
            ) {
                println!("  ⚠ {id}: không kiểm tra được ({err})");
            }
        }
        return Ok(());
    }

    println!("Có bản mới:");
    for u in outdated {
        let id = u.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let cur = u
            .get("installed")
            .and_then(|v| v.as_str())
            .unwrap_or("chưa rõ");
        let latest = u.get("latest").and_then(|v| v.as_str()).unwrap_or("?");
        println!("  {id}: {cur} → {latest}");
    }
    println!("\nChạy `senclaw hub update <id>` hoặc `senclaw hub update --all`.");
    Ok(())
}

async fn update_one(base: &str, id: &str) -> Result<()> {
    let url = format!("{base}/api/space/apps/{id}/update");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;
    let resp = client
        .post(&url)
        .send()
        .await
        .with_context(|| format!("không gọi được {url}"))?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        let msg = body
            .get("error")
            .and_then(|v| v.as_str())
            .or_else(|| body.get("message").and_then(|v| v.as_str()))
            .unwrap_or("(daemon không kèm thông báo)");
        bail!("cập nhật {id} thất bại (HTTP {status}): {msg}");
    }
    if body
        .get("updated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let latest = body.get("latest").and_then(|v| v.as_str()).unwrap_or("?");
        println!("✓ {id} → {latest}");
    } else {
        println!("• {id} đã mới nhất, bỏ qua");
    }
    Ok(())
}

async fn update(id: Option<&str>, all: bool) -> Result<()> {
    let base = daemon_base();
    match (id, all) {
        (Some(id), _) => update_one(&base, id).await,
        (None, true) => {
            let updates = fetch_updates().await?;
            let ids: Vec<String> = updates
                .iter()
                .filter(|u| {
                    u.get("hasUpdate")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                })
                .filter_map(|u| u.get("id").and_then(|v| v.as_str()).map(str::to_string))
                .collect();
            if ids.is_empty() {
                println!("Không có app nào cần cập nhật.");
                return Ok(());
            }
            for id in ids {
                if let Err(e) = update_one(&base, &id).await {
                    eprintln!("  ⚠ {e}");
                }
            }
            Ok(())
        }
        (None, false) => {
            bail!("cần <id> hoặc --all. Xem `senclaw hub outdated` để biết app nào có bản mới.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_dir(id: &str, desc: &str) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("senclaw-manifest.json"),
            serde_json::json!({
                "id": id,
                "name": id,
                "description": desc,
                "runtime": { "kind": "server", "start": format!("./{id}"), "port": 4420 }
            })
            .to_string(),
        )
        .unwrap();
        d
    }

    #[test]
    fn init_derives_a_narrow_permission_declaration_from_the_manifest() {
        let d = app_dir("demo", "mô tả");
        init(d.path(), "1.0.0").unwrap();
        let pkg = read_hub_pkg(d.path()).unwrap();
        let perms = pkg.permissions.unwrap();
        // Loopback only, and exec limited to the app's own binary — a scaffold
        // that granted more would be a security declaration nobody reviewed.
        assert_eq!(perms["network"][0], "127.0.0.1");
        assert_eq!(perms["exec"][0], "./demo");
    }

    #[test]
    fn init_refuses_to_overwrite_an_existing_file() {
        let d = app_dir("demo", "mô tả");
        init(d.path(), "1.0.0").unwrap();
        assert!(init(d.path(), "2.0.0").is_err());
    }

    #[test]
    fn init_rejects_a_non_semver_version() {
        let d = app_dir("demo", "mô tả");
        assert!(init(d.path(), "1.0").is_err());
    }

    #[test]
    fn bump_rewrites_the_file_and_keeps_everything_else() {
        let d = app_dir("demo", "mô tả");
        init(d.path(), "1.2.3").unwrap();
        let mut pkg = read_hub_pkg(d.path()).unwrap();
        pkg.keywords = vec!["mcp".into()];
        pkg.category = Some("productivity".into());
        write_hub_pkg(d.path(), &pkg).unwrap();

        bump(d.path(), "minor").unwrap();
        let after = read_hub_pkg(d.path()).unwrap();
        assert_eq!(after.version, "1.3.0");
        assert_eq!(after.keywords, vec!["mcp".to_string()]);
        assert_eq!(after.category.as_deref(), Some("productivity"));
    }

    #[test]
    fn a_missing_hub_file_names_the_command_that_creates_it() {
        let d = app_dir("demo", "mô tả");
        let err = read_hub_pkg(d.path()).unwrap_err().to_string();
        assert!(err.contains("senclaw hub init"), "{err}");
    }

    #[tokio::test]
    async fn preflight_fails_when_the_artifact_is_missing() {
        let d = app_dir("demo", "mô tả");
        init(d.path(), "1.0.0").unwrap();
        let err = preflight(d.path(), "http://127.0.0.1:1")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("--pack"), "{err}");
    }

    #[tokio::test]
    async fn preflight_rejects_an_oversized_artifact_before_uploading() {
        let d = app_dir("demo", "mô tả");
        init(d.path(), "1.0.0").unwrap();
        std::fs::write(
            d.path().join("demo-app.zip"),
            vec![0u8; (MAX_UPLOAD_BYTES + 1) as usize],
        )
        .unwrap();
        let err = preflight(d.path(), "http://127.0.0.1:1")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("vượt giới hạn"), "{err}");
    }

    #[tokio::test]
    async fn preflight_requires_a_description_because_the_hub_does() {
        let d = app_dir("demo", "");
        init(d.path(), "1.0.0").unwrap();
        std::fs::write(d.path().join("demo-app.zip"), b"zip").unwrap();
        let err = preflight(d.path(), "http://127.0.0.1:1")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("description"), "{err}");
    }

    #[tokio::test]
    async fn an_unreachable_hub_does_not_block_the_preflight() {
        // The server is the authority on version conflicts; a network failure
        // must not be reported as "the name is free" nor abort the run.
        let d = app_dir("demo", "mô tả");
        init(d.path(), "1.0.0").unwrap();
        std::fs::write(d.path().join("demo-app.zip"), b"zip").unwrap();
        assert!(preflight(d.path(), "http://127.0.0.1:1").await.is_ok());
    }

    #[test]
    fn the_artifact_name_defaults_to_the_pack_script_output() {
        let d = app_dir("ai-office", "mô tả");
        init(d.path(), "1.0.0").unwrap();
        let app = read_app(d.path()).unwrap();
        let pkg = read_hub_pkg(d.path()).unwrap();
        assert!(artifact_path(&app, &pkg).ends_with("ai-office-app.zip"));
    }
}
