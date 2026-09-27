//! What one Space App is *doing right now* — the data behind the monitor tab.
//!
//! A Space App is a process the daemon launched and then mostly forgets about.
//! When one misbehaves the questions are always the same: is it even running,
//! since when, how many times has it restarted, what is it burning, who is it
//! talking to, and what does its log say. Each answer lives somewhere different
//! (the launcher's child map, the OS process table, `lsof`, the runtime log), so
//! this module collects them into one snapshot rather than making the UI stitch
//! four endpoints together.
//!
//! Everything here is read-only and best-effort: a missing `lsof`, a process
//! that exits mid-sample, or a health endpoint that times out each degrade to a
//! note in the payload instead of failing the request. A monitor that 500s when
//! the thing it monitors is broken is the one moment it had a job to do.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;
use serde_json::{json, Value};

use super::core::{AppError, UiState};
use super::space::{installed_app_dir_from_manifest, space_app_manifest, valid_space_app_id};

/// One socket the app holds. Deliberately not "connections to the internet":
/// the listening socket is what proves the app is actually serving, and the
/// loopback ones show it talking to the daemon or to another app.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Conn {
    pub pid: u32,
    pub command: String,
    /// `TCP` — UDP is listed too when a runtime uses it (DNS), same shape.
    pub proto: String,
    pub local: String,
    /// `None` for a listening socket.
    pub remote: Option<String>,
    /// `LISTEN`, `ESTABLISHED`, `CLOSE_WAIT`… — as the OS reports it.
    pub state: String,
}

/// Parse `lsof -nP -i` output.
///
/// Split from the process call because this is where the bugs are: the NAME
/// column is positional-ish but the command name can contain spaces, so the
/// parse works from the *end* of the line, not the start.
pub fn parse_lsof(out: &str) -> Vec<Conn> {
    let mut v = Vec::new();
    for line in out.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 8 || f[0] == "COMMAND" {
            continue;
        }
        let Ok(pid) = f[1].parse::<u32>() else {
            continue; // header, or a line lsof mangled
        };
        // The tail is either `TCP <name> (STATE)` or `TCP <name>`; UDP rows have
        // no state at all.
        let (name, state) = match f.last() {
            Some(last) if last.starts_with('(') && last.ends_with(')') => (
                f.get(f.len() - 2).copied().unwrap_or_default(),
                last.trim_matches(['(', ')']).to_string(),
            ),
            Some(last) => (*last, String::new()),
            None => continue,
        };
        let proto = if line.contains(" TCP ") {
            "TCP"
        } else if line.contains(" UDP ") {
            "UDP"
        } else {
            continue;
        };
        let (local, remote) = match name.split_once("->") {
            Some((l, r)) => (l.to_string(), Some(r.to_string())),
            None => (name.to_string(), None),
        };
        v.push(Conn {
            pid,
            command: f[0].to_string(),
            proto: proto.to_string(),
            local,
            remote,
            state,
        });
    }
    v
}

/// Sockets held by these pids. Empty (with a note from the caller) when `lsof`
/// is missing or refuses — never an error, and never a fatal one.
///
/// Read-only by construction: this passes explicit pids and parses the output.
/// It must never grow into `lsof -t … | kill`, which is how a previous incident
/// took the daemon down by killing client sockets' owners along with the server.
pub async fn connections(pids: &[u32]) -> (Vec<Conn>, Option<String>) {
    if pids.is_empty() {
        return (Vec::new(), None);
    }
    if cfg!(windows) {
        return (
            Vec::new(),
            Some("connection listing is not implemented on Windows".into()),
        );
    }
    let list = pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let run = tokio::process::Command::new("lsof")
        .args(["-nP", "-a", "-p", &list, "-i"])
        .stdin(std::process::Stdio::null())
        .output();
    match tokio::time::timeout(Duration::from_secs(4), run).await {
        Ok(Ok(out)) => {
            let mut conns = parse_lsof(&String::from_utf8_lossy(&out.stdout));
            // A busy app can hold hundreds of sockets; the monitor is a
            // diagnostic, not a packet log.
            let note = (conns.len() > 200).then(|| {
                format!("{} sockets, showing the first 200", conns.len())
            });
            conns.truncate(200);
            (conns, note)
        }
        Ok(Err(e)) => (
            Vec::new(),
            Some(format!("cannot run lsof: {e} — install it to see connections")),
        ),
        Err(_) => (Vec::new(), Some("lsof timed out".into())),
    }
}

/// Every TCP port something is listening on, mapped to the pid holding it.
///
/// One `lsof` for the whole machine rather than one per app: the fleet view
/// refreshes every few seconds with dozens of apps on it.
///
/// Read-only, and it must stay that way — `lsof -t … | xargs kill` on a port is
/// how a previous incident killed the *clients* of a socket along with its
/// server and took the daemon down with it.
pub async fn listening_ports() -> std::collections::HashMap<u16, u32> {
    let mut map = std::collections::HashMap::new();
    if cfg!(windows) {
        return map;
    }
    let run = tokio::process::Command::new("lsof")
        .args(["-nP", "-iTCP", "-sTCP:LISTEN"])
        .stdin(std::process::Stdio::null())
        .output();
    let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(5), run).await else {
        return map;
    };
    for c in parse_lsof(&String::from_utf8_lossy(&out.stdout)) {
        if let Some(port) = c.local.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()) {
            // First writer wins: a port bound on both v4 and v6 lists twice, and
            // both rows are the same process.
            map.entry(port).or_insert(c.pid);
        }
    }
    map
}

/// The port an app serves on, as far as the stored manifest knows: the fixed
/// `runtime.port`, else the origin the daemon recorded after a successful
/// launch (`runtime.url`).
///
/// Needed because an app this daemon *adopted* has no child record to ask.
pub fn app_port(manifest: &Value) -> Option<u16> {
    let rt = manifest.get("runtime")?;
    if let Some(p) = rt.get("port").and_then(Value::as_u64) {
        if p > 0 && p <= u16::MAX as u64 {
            return Some(p as u16);
        }
    }
    let url = rt.get("url").and_then(Value::as_str)?;
    url.rsplit_once(':')
        .and_then(|(_, tail)| tail.trim_end_matches('/').split('/').next()?.parse().ok())
}

/// `ps` elapsed time (`MM:SS`, `HH:MM:SS`, `D-HH:MM:SS`) in milliseconds, so an
/// adopted process can report an uptime like a launched one.
pub fn etime_ms(etime: &str) -> Option<u64> {
    let (days, rest) = match etime.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, etime),
    };
    let parts: Vec<u64> = rest.split(':').map(|p| p.trim().parse().ok()).collect::<Option<_>>()?;
    let secs = match parts.as_slice() {
        [m, s] => m * 60 + s,
        [h, m, s] => h * 3600 + m * 60 + s,
        _ => return None,
    };
    Some((days * 86_400 + secs) * 1000)
}

/// A server app that is up but was not launched by this daemon.
///
/// `ensure_server_running` reuses a healthy fixed port instead of double
/// launching, so an app left over from a previous daemon run — or started by
/// hand — serves normally while the launcher holds no record of it. Reporting
/// that as "not running" is simply wrong, and it is the common case for any app
/// that outlives a daemon restart.
struct Adopted {
    pid: u32,
    port: u16,
}

async fn adopted_process(
    manifest: &Value,
    ports: &std::collections::HashMap<u16, u32>,
) -> Option<Adopted> {
    let port = app_port(manifest)?;
    let pid = *ports.get(&port)?;
    Some(Adopted { pid, port })
}

/// `GET /api/space/apps/:id/runtime` — one snapshot of a running app.
pub(crate) async fn space_app_runtime(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    if !valid_space_app_id(&id) {
        return Err(AppError(StatusCode::BAD_REQUEST, "Invalid app id".into()));
    }
    let Some(launcher) = s.space_mcp_launcher.as_ref() else {
        return Err(AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            "App runtime not available".into(),
        ));
    };
    let manifest = space_app_manifest(&s, &id).unwrap_or_default();
    let app_dir = installed_app_dir_from_manifest(&s, &id, Some(&manifest))?;
    let info = launcher.runtime_info(&id).await;
    let launches = launcher.launch_count(&id).await;
    // Not tracked does not mean not running: a healthy fixed port is adopted
    // rather than double-launched, which is what every app that outlived a
    // daemon restart looks like.
    let adopted = match &info {
        Some(_) => None,
        None => adopted_process(&manifest, &listening_ports().await).await,
    };

    // ── the process, if there is one ────────────────────────────────────────
    let (resources, conns, conn_note) = match (&info, &adopted) {
        (Some(i), _) if i.pgid > 0 => {
            let stats = crate::sandbox::monitor::stats_for_groups(&[i.pgid as u32]).await;
            let mut pids: Vec<u32> = stats.processes.iter().map(|p| p.pid).collect();
            if !pids.contains(&i.pid) {
                pids.push(i.pid);
            }
            let (c, note) = connections(&pids).await;
            (Some(stats), c, note)
        }
        (None, Some(a)) => {
            let by_pid = crate::sandbox::monitor::stats_by_pid(&[a.pid]).await;
            let stats = by_pid.get(&a.pid).cloned();
            let pids: Vec<u32> = stats
                .as_ref()
                .map(|s| s.processes.iter().map(|p| p.pid).collect())
                .unwrap_or_else(|| vec![a.pid]);
            let (c, note) = connections(&pids).await;
            (stats, c, note)
        }
        _ => (None, Vec::new(), None),
    };

    // ── is it answering? ────────────────────────────────────────────────────
    // "Tracked" and "working" are different claims, and a crash-looping app is
    // tracked for a second at a time.
    let health_port = info.as_ref().map(|i| i.port).or(adopted.as_ref().map(|a| a.port));
    let health = match health_port {
        Some(port) => {
            let path = manifest
                .get("runtime")
                .and_then(|r| r.get("healthPath"))
                .and_then(Value::as_str)
                .unwrap_or("/");
            let url = format!("http://127.0.0.1:{port}{path}");
            let started = std::time::Instant::now();
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap_or_default();
            match client.get(&url).send().await {
                Ok(r) => json!({
                    "url": url,
                    "ok": r.status().is_success(),
                    "status": r.status().as_u16(),
                    "ms": started.elapsed().as_millis() as u64,
                }),
                Err(e) => json!({ "url": url, "ok": false, "error": e.to_string() }),
            }
        }
        None => Value::Null,
    };

    // ── the log, as metadata only; the contents have their own endpoint ─────
    let log_path = info
        .as_ref()
        .map(|i| i.log_path.clone())
        .unwrap_or_else(|| super::space_mcp::app_runtime_log_path(&app_dir));
    let log_meta = tokio::fs::metadata(&log_path).await.ok();

    let sb = crate::sandbox::shared_db()
        .map(|db| crate::sandbox::app_policy::load(&db, &id))
        .unwrap_or_default();

    let spec = crate::apps::RuntimeSpec::parse(&manifest);
    // Only probed when the app actually declared requirements — otherwise this
    // is a `which` per panel refresh for every app that needs nothing.
    let requires = crate::apps::Requires::parse(&manifest);
    let requirements = if requires.is_empty() {
        Value::Null
    } else {
        serde_json::to_value(crate::apps::requirements::check(&requires).await)
            .unwrap_or(Value::Null)
    };

    let now = std::time::SystemTime::now();
    // An adopted process reports the same shape, minus what only the launcher
    // knows. `isolation: "unknown"` is the honest value: this daemon did not
    // build that process's profile, so it cannot claim the app is confined.
    let adopted_proc = adopted.as_ref().map(|a| {
        let elapsed = resources
            .as_ref()
            .and_then(|s| s.processes.iter().find(|p| p.pid == a.pid))
            .map(|p| p.elapsed.clone());
        json!({
            "pid": a.pid,
            "pgid": null,
            "port": a.port,
            "url": format!("http://127.0.0.1:{}", a.port),
            "uptimeMs": elapsed.as_deref().and_then(etime_ms),
            "isolation": "unknown",
            "adopted": true,
        })
    });

    Ok(Json(json!({
        "appId": id,
        "running": info.is_some() || adopted.is_some(),
        "adopted": adopted.is_some(),
        "process": info.as_ref().map(|i| json!({
            "pid": i.pid,
            "pgid": i.pgid,
            "port": i.port,
            "url": format!("http://127.0.0.1:{}", i.port),
            "uptimeMs": now.duration_since(i.started_at).map(|d| d.as_millis() as u64).unwrap_or(0),
            "isolation": i.isolation,
            "adopted": false,
        })).or(adopted_proc),
        // Launch count, not "restarts": the first start counts as one, and the
        // difference matters when reading "3" on an app installed a minute ago.
        "launches": launches,
        // Which lifecycle this app is on, and — for a session app — how close it
        // is to being stopped for being idle. Without this, "not running" reads
        // as a fault when for most apps it is the normal resting state.
        "lifecycle": {
            "mode": spec.mode.as_str(),
            "runner": spec.runner.as_str(),
            "idleTimeoutSecs": if spec.mode.is_background() { 0 } else { spec.idle_timeout_secs },
            "idleSecs": info.as_ref().map(|i| i.idle_secs),
            "stoppedByUser": launcher.is_user_stopped(&id).await,
        },
        "requirements": requirements,
        "health": health,
        "resources": resources,
        "network": {
            "connections": conns,
            "note": conn_note,
            "proxy": info.as_ref().and_then(|i| i.proxy.as_ref()).map(|(port, stats)| json!({
                "port": port,
                "stats": stats,
            })),
        },
        "sandbox": {
            "enabled": sb.enabled,
            "readMode": sb.read_mode.as_str(),
            "network": sb.network.as_str(),
            "hosts": sb.hosts,
        },
        "log": {
            "path": log_path.to_string_lossy(),
            "bytes": log_meta.as_ref().map(|m| m.len()).unwrap_or(0),
        },
        // Enough to reproduce the launch by hand in a terminal, which is what
        // "debug this app" usually turns into.
        "launch": {
            "cwd": app_dir.to_string_lossy(),
            "command": manifest.get("runtime").and_then(|r| r.get("start")).and_then(Value::as_str),
            "env": launch_env(&info, s.config.ui_server.port, s.config.space_api_version),
        },
    })))
}

/// `GET /api/space/apps/sandbox-overview` — every server app at once: is it
/// confined, is it running, what is it using, what is its proxy refusing.
///
/// The fleet view the Sandbox screen needs. Two things it deliberately reports
/// separately: what each app is **configured** to get, and what the process that
/// is **actually running** was given — they differ the moment someone edits the
/// settings without restarting, and only the second one is true right now.
pub(crate) async fn space_apps_sandbox_overview(
    State(s): State<Arc<UiState>>,
) -> Result<Json<Value>, AppError> {
    let Some(launcher) = s.space_mcp_launcher.as_ref() else {
        return Err(AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            "App runtime not available".into(),
        ));
    };
    let db = s.db.as_deref().ok_or_else(|| {
        AppError(StatusCode::SERVICE_UNAVAILABLE, "Database not available".into())
    })?;
    let rows: Vec<(String, Value)> = db
        .with_conn(|conn| {
            let mut st = conn.prepare("SELECT id, manifest FROM space_apps ORDER BY id")?;
            let it = st.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            Ok(it
                .filter_map(|x| x.ok())
                .filter_map(|(id, raw)| {
                    serde_json::from_str::<Value>(&raw).ok().map(|m| (id, m))
                })
                .collect())
        })
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let sb_db = crate::sandbox::shared_db();
    let caps = crate::sandbox::caps::direct_caps(false).await;

    // Collect first, sample once: a dozen apps on a panel that refreshes every
    // few seconds should not mean a dozen process listings per tick.
    let mut infos = Vec::new();
    for (id, manifest) in &rows {
        if manifest
            .get("runtime")
            .and_then(|r| r.get("kind"))
            .and_then(Value::as_str)
            != Some("server")
        {
            continue; // a static app has no process to report on
        }
        let info = launcher.runtime_info(id).await;
        let launches = launcher.launch_count(id).await;
        infos.push((id.clone(), manifest.clone(), info, launches));
    }
    // Apps this daemon adopted rather than launched — the common case after a
    // daemon restart, and previously reported as "not running" while they were
    // serving perfectly well. One `lsof` for the whole machine answers it for
    // every app at once.
    let ports = if infos.iter().any(|(_, _, i, _)| i.is_none()) {
        listening_ports().await
    } else {
        std::collections::HashMap::new()
    };
    let adopted: std::collections::HashMap<String, Adopted> = infos
        .iter()
        .filter(|(_, _, i, _)| i.is_none())
        .filter_map(|(id, m, _, _)| {
            let port = app_port(m)?;
            let pid = *ports.get(&port)?;
            Some((id.clone(), Adopted { pid, port }))
        })
        .collect();

    let groups: Vec<u32> = infos
        .iter()
        .filter_map(|(_, _, i, _)| i.as_ref().filter(|i| i.pgid > 0).map(|i| i.pgid as u32))
        .collect();
    let stats = crate::sandbox::monitor::stats_by_group(&groups).await;
    let adopted_pids: Vec<u32> = adopted.values().map(|a| a.pid).collect();
    let adopted_stats = crate::sandbox::monitor::stats_by_pid(&adopted_pids).await;

    let now = std::time::SystemTime::now();
    let apps: Vec<Value> = infos
        .into_iter()
        .map(|(id, manifest, info, launches)| {
            let cfg = sb_db
                .as_ref()
                .map(|db| crate::sandbox::app_policy::load(db, &id))
                .unwrap_or_default();
            let ad = adopted.get(&id);
            let st = match (&info, ad) {
                (Some(i), _) => stats.get(&(i.pgid as u32)),
                (None, Some(a)) => adopted_stats.get(&a.pid),
                _ => None,
            };
            json!({
                "id": id,
                "name": manifest.get("name").and_then(Value::as_str).unwrap_or(&id),
                "icon": manifest.get("icon").and_then(Value::as_str),
                "config": {
                    "enabled": cfg.enabled,
                    "readMode": cfg.read_mode.as_str(),
                    "network": cfg.network.as_str(),
                    "hosts": cfg.hosts,
                    "daemonApi": cfg.daemon_api,
                    "folders": cfg.folders.len(),
                    // The app's manifest requires the sandbox — the row is not
                    // one the user can switch off.
                    "forced": cfg.forced,
                },
                // A session app that is not running is idle, not broken. The
                // fleet view says so, or half this list reads as an outage.
                "mode": crate::apps::RuntimeSpec::parse(&manifest).mode.as_str(),
                "running": info.is_some() || ad.is_some(),
                // True when the process was already up and this daemon adopted
                // it: the launcher never built its profile, so nothing here can
                // claim it is confined.
                "adopted": ad.is_some(),
                // What the *running* process got. `none` while the settings say
                // enabled means the app has not been restarted since;
                // `unknown` means this daemon did not launch it at all.
                "isolation": match (&info, ad) {
                    (Some(i), _) => Some(i.isolation.clone()),
                    (None, Some(_)) => Some("unknown".to_string()),
                    _ => None,
                },
                "pid": info.as_ref().map(|i| i.pid).or(ad.map(|a| a.pid)),
                "port": info.as_ref().map(|i| i.port).or(ad.map(|a| a.port)),
                "uptimeMs": match (&info, ad) {
                    (Some(i), _) => now
                        .duration_since(i.started_at)
                        .map(|d| d.as_millis() as u64)
                        .ok(),
                    (None, Some(a)) => st
                        .and_then(|s| s.processes.iter().find(|p| p.pid == a.pid))
                        .and_then(|p| etime_ms(&p.elapsed)),
                    _ => None,
                },
                "launches": launches,
                "cpu": st.map(|s| s.cpu),
                "rssMb": st.map(|s| s.rss_mb),
                "processes": st.map(|s| s.processes.len()),
                "proxy": info.as_ref().and_then(|i| i.proxy.as_ref()).map(|(port, stats)| json!({
                    "port": port,
                    "stats": stats,
                })),
            })
        })
        .collect();

    Ok(Json(json!({
        "apps": apps,
        "caps": {
            "isolation": caps.kind.as_str(),
            "enforceable": caps.kind.is_enforced() && !cfg!(windows),
            "networkEnforceable": matches!(caps.kind, crate::sandbox::caps::DirectKind::Seatbelt),
        },
    })))
}

/// The environment the daemon adds on top of its own — the part that explains
/// behaviour ("why is TMPDIR there", "why does it use a proxy").
fn launch_env(
    info: &Option<super::space_mcp::RuntimeInfo>,
    daemon_port: u16,
    api_version: u32,
) -> Vec<(String, String)> {
    let mut env = vec![(
        "SENCLAW_BASE_URL".to_string(),
        format!("http://127.0.0.1:{daemon_port}"),
    )];
    if let Some(i) = info {
        env.insert(0, ("PORT".to_string(), i.port.to_string()));
        if let Some((p, _)) = &i.proxy {
            env.push(("HTTPS_PROXY".to_string(), format!("http://127.0.0.1:{p}")));
        }
    }
    env.push((crate::apps::token::ENV_API_VERSION.to_string(), api_version.to_string()));
    // Named but not shown. This panel polls every three seconds and ends up in
    // screenshots; the access token is fetched deliberately from
    // `GET /api/space/apps/<id>/token` when someone actually needs it to run the
    // app by hand. Listing the variable still answers "does this app have an
    // identity", which is the question the panel is for.
    env.push((
        crate::apps::token::ENV_APP_TOKEN.to_string(),
        "sca_… (GET /api/space/apps/<id>/token)".to_string(),
    ));
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real `lsof -nP -a -p … -i` output, including the two rows that matter: a
    // listening socket and an established one.
    const SAMPLE: &str = "\
COMMAND     PID  USER   FD   TYPE             DEVICE SIZE/OFF NODE NAME
node      35182 benji   20u  IPv4 0x9a1b2c3d4e5f      0t0  TCP 127.0.0.1:4740 (LISTEN)
node      35182 benji   23u  IPv4 0x1122334455667      0t0  TCP 192.168.1.5:54321->142.250.185.78:443 (ESTABLISHED)
Google\\x20Chrome 35183 benji 8u  IPv4 0xaaa            0t0  UDP *:5353
";

    #[test]
    fn a_listening_socket_and_a_peer_are_told_apart() {
        let c = parse_lsof(SAMPLE);
        assert_eq!(c.len(), 3, "got {c:#?}");
        assert_eq!(c[0].local, "127.0.0.1:4740");
        assert_eq!(c[0].remote, None, "a listener has no peer");
        assert_eq!(c[0].state, "LISTEN");
        assert_eq!(c[0].pid, 35182);
        assert_eq!(c[1].remote.as_deref(), Some("142.250.185.78:443"));
        assert_eq!(c[1].local, "192.168.1.5:54321");
        assert_eq!(c[1].state, "ESTABLISHED");
        // UDP rows carry no state and must not be dropped or mislabelled.
        assert_eq!(c[2].proto, "UDP");
        assert_eq!(c[2].state, "");
    }

    #[test]
    fn the_header_and_junk_lines_are_skipped() {
        assert!(parse_lsof("COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME").is_empty());
        assert!(parse_lsof("lsof: WARNING: can't stat() nfs file system\n").is_empty());
        assert!(parse_lsof("").is_empty());
    }

    #[test]
    fn an_adopted_app_is_found_by_the_port_the_manifest_records() {
        // `runtime.url` is what the daemon writes back after a launch, and for an
        // app it merely adopted that is the only trace of which port is its.
        let m = serde_json::json!({"runtime": {"kind": "server", "url": "http://127.0.0.1:4491"}});
        assert_eq!(app_port(&m), Some(4491));
        // A fixed port in the manifest wins — it is the authoritative one.
        let fixed = serde_json::json!({"runtime": {"port": 4740, "url": "http://127.0.0.1:9999"}});
        assert_eq!(app_port(&fixed), Some(4740));
        // Nothing to go on is not an error, just no answer.
        assert_eq!(app_port(&serde_json::json!({"runtime": {"kind": "server"}})), None);
        assert_eq!(app_port(&serde_json::json!({})), None);
        assert_eq!(app_port(&serde_json::json!({"runtime": {"port": 0}})), None);
    }

    #[test]
    fn ps_elapsed_times_convert_in_every_shape_ps_prints() {
        assert_eq!(etime_ms("00:07"), Some(7_000));
        assert_eq!(etime_ms("01:35"), Some(95_000));
        assert_eq!(etime_ms("02:03:04"), Some((2 * 3600 + 3 * 60 + 4) * 1000));
        assert_eq!(etime_ms("1-04:05:06"), Some((86_400 + 4 * 3600 + 5 * 60 + 6) * 1000));
        assert_eq!(etime_ms("nonsense"), None);
        assert_eq!(etime_ms(""), None);
    }

    #[test]
    fn listening_sockets_map_to_the_pid_holding_the_port() {
        // Both address families list the same server; the map must not flip
        // between them from one refresh to the next.
        let out = "\
COMMAND    PID  USER   FD   TYPE DEVICE SIZE/OFF NODE NAME
deepwiki 18274 benji   6u  IPv4  0xaaa      0t0  TCP 127.0.0.1:4491 (LISTEN)
deepwiki 18274 benji   7u  IPv6  0xbbb      0t0  TCP [::1]:4491 (LISTEN)
node      4242 benji   8u  IPv4  0xccc      0t0  TCP *:3000 (LISTEN)";
        let mut map = std::collections::HashMap::new();
        for c in parse_lsof(out) {
            if let Some(port) = c.local.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()) {
                map.entry(port).or_insert(c.pid);
            }
        }
        assert_eq!(map.get(&4491), Some(&18274));
        assert_eq!(map.get(&3000), Some(&4242));
    }

    #[tokio::test]
    async fn no_pids_means_no_work_and_no_note() {
        let (c, note) = connections(&[]).await;
        assert!(c.is_empty());
        assert!(note.is_none(), "an empty ask is not a failure");
    }

    #[tokio::test]
    async fn this_very_process_shows_up_in_its_own_connections() {
        // Proves the command and the parser agree on a real machine, not just on
        // the captured sample above.
        if cfg!(windows) {
            return;
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (conns, note) = connections(&[std::process::id()]).await;
        if note.as_deref().map(|n| n.contains("cannot run lsof")) == Some(true) {
            return; // no lsof on this machine: nothing to assert
        }
        assert!(
            conns.iter().any(|c| c.local.ends_with(&format!(":{port}")) && c.state == "LISTEN"),
            "the port this test just opened must appear: {conns:#?}"
        );
    }
}
