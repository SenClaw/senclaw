//! Soul Core — who the *human* is, as opposed to who the agent is.
//!
//! SenClaw already had a `SOUL.md`, but it answers a different question: it is
//! the **agent's** persona, one file per agent folder under
//! `~/senclaw/agents/<folder>/`, ingested into the cognitive graph and edited
//! by the `PersonaUpdate` tool. Nothing anywhere recorded the owner's name,
//! how to address them, their timezone or their email — so every profile
//! started cold, and a machine with 34 agent folders would have needed the
//! same details entered 34 times.
//!
//! Soul Core lives at **`~/.senclaw/USER.md`** — `senclaw_home`, a different
//! tree from `senclaw_data` where the agents live. That placement is load
//! bearing rather than cosmetic: `spawn_soul_watcher` only walks `agents_dir`,
//! so the persona watcher, the persona ingest and `PersonaUpdate` all
//! structurally cannot touch this file.
//!
//! Two sibling flat files ride the same machinery:
//!
//! * **`TOOLS.md`** — local environment notes (SSH hosts, camera names, TTS
//!   voices). Kept out of skills so skills stay shareable without leaking
//!   somebody's infrastructure.
//! * **`AGENTS.md`** — user-editable operating rules, appended to the system
//!   prompt after the hardcoded base.
//!
//! # Reading the profile
//!
//! Always through [`render::render`]. It is the single place the public /
//! private tier rule is applied; a caller that parses the file itself is a
//! leak waiting to happen.

pub mod parse;
pub mod render;

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

pub use parse::{Directive, DirectiveStatus, Field, Tier, UserProfile};
pub use render::{MAX_PROFILE_CHARS, ProfileScope, render};

use crate::config::Config;

/// Cap for the flat companion files (`TOOLS.md`, `AGENTS.md`).
///
/// Mirrors OpenClaw's `bootstrapMaxChars` default. These land in the system
/// prompt on every turn, so an unbounded file is an unbounded per-turn cost —
/// this repo's own `CLAUDE.md` is 34 KB, which is what that would look like.
pub const MAX_FLAT_FILE_CHARS: usize = 20_000;

/// Process-wide cache of the parsed profile, keyed by the path it came from.
///
/// Read on every first turn of every session; re-reading and re-parsing from
/// disk each time would be wasteful and would make the watcher pointless.
///
/// Keyed rather than a bare `Option<UserProfile>` because the path is not
/// actually constant: tests point at temp dirs, and `SENCLAW_USER_PROFILE_PATH`
/// lets a caller relocate it. An unkeyed cache would serve one path's profile
/// for a request about another — silently, and with personal data.
type CacheEntry = (PathBuf, Option<std::time::SystemTime>, UserProfile);
static CACHE: RwLock<Option<CacheEntry>> = RwLock::new(None);

/// Read the cached profile, reloading when the file changed underneath us.
///
/// The mtime check is what makes this correct rather than merely fast. This
/// process is **not the only writer**: the agent edits the same file through
/// the `senclaw-profile` MCP server, which runs in a separate process. Serving
/// a purely in-memory cache meant a write from there stayed invisible until
/// the 30s watcher tick — long enough for the Settings screen to show stale
/// values and then save them back over the agent's change.
///
/// One `stat` per read; this is not a hot path (once per session for the
/// prompt, once per REST call).
pub fn get_or_load(path: &Path) -> UserProfile {
    let disk_mtime = mtime_of(path);
    if let Some(p) = CACHE.read().ok().and_then(|g| {
        g.as_ref()
            .filter(|(p, m, _)| p == path && *m == disk_mtime)
            .map(|(_, _, v)| v.clone())
    }) {
        return p;
    }
    reload(path)
}

/// Force a reload from disk, replacing the cache.
pub fn reload(path: &Path) -> UserProfile {
    let parsed = read_from_disk(path);
    if let Ok(mut g) = CACHE.write() {
        *g = Some((path.to_path_buf(), mtime_of(path), parsed.clone()));
    }
    parsed
}

/// Drop the cache. Used by tests and after a write from the REST layer.
pub fn invalidate() {
    if let Ok(mut g) = CACHE.write() {
        *g = None;
    }
}

fn read_from_disk(path: &Path) -> UserProfile {
    match std::fs::read_to_string(path) {
        Ok(text) => parse::parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => UserProfile::default(),
        Err(e) => {
            tracing::warn!("[user_profile] read {} failed: {e}", path.display());
            UserProfile::default()
        }
    }
}

/// Write the profile back to disk, then refresh the cache.
///
/// `USER.md` may hold an email and a home address, so it is owner-only like
/// the other secret-bearing files in `~/.senclaw/`.
pub fn save(path: &Path, profile: &UserProfile) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Temp + rename: a half-written USER.md would parse as a truncated
    // profile and silently drop fields.
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, parse::serialize(profile))?;
    std::fs::rename(&tmp, path)?;
    crate::util::file_perms::restrict_best_effort(path);
    reload(path);
    Ok(())
}

/// Render the profile block for a chat instance, or `None`.
///
/// The entry point every prompt-building path should use: it resolves the
/// scope from the JID and applies the tier rule in one step, so no caller has
/// to know either.
///
/// When the profile is empty in a private chat this returns a short prompt to
/// fill it rather than `None`. That case is not hypothetical — it is every
/// fresh install, and without the line the model has no evidence the profile
/// exists at all: told "remember my name", it reaches for whatever memory tool
/// it does know about, answers "noted", and `USER.md` stays blank. Group chats
/// still get `None`, because they cannot write to it anyway.
pub fn block_for_instance(cfg: &Config, instance_id: &str) -> Option<String> {
    let profile = get_or_load(&cfg.paths.user_profile_path);
    let scope = ProfileScope::for_instance(instance_id);
    if let Some(block) = render(&profile, scope) {
        return Some(block);
    }
    if scope == ProfileScope::Full {
        return Some(
            "<user_profile>\n\
             Chưa có thông tin nào về người dùng. Khi họ cho biết tên, cách xưng \
             hô, múi giờ hay sở thích cố định, hãy gọi `profile_update` để ghi lại \
             — công cụ nhớ thông thường KHÔNG ghi vào hồ sơ.\n\
             </user_profile>"
                .to_string(),
        );
    }
    None
}

// ===== Flat companion files =====

/// Read one of the flat markdown files, capped and trimmed.
///
/// `None` when absent or empty — an absent file must be indistinguishable
/// from "nothing configured", never an error the user has to dismiss.
pub fn read_flat_file(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(crate::util::text::truncate_on_char_boundary(text, MAX_FLAT_FILE_CHARS).to_string())
}

pub fn write_flat_file(path: &Path, content: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    crate::util::file_perms::restrict_best_effort(path);
    Ok(())
}

/// Wrap `AGENTS.md` for the system prompt.
///
/// Placed **after** the hardcoded base prompt, never before, and framed so the
/// safety section keeps precedence. This file is text the user types; if it
/// were spliced in ahead of the safety rules, a line saying "ignore the rules
/// above" would be read first.
pub fn operating_rules_block(cfg: &Config) -> Option<String> {
    let body = read_flat_file(&cfg.paths.agents_rules_path)?;
    Some(format!(
        "<user_operating_rules>\n\
         Quy tắc vận hành do chủ máy đặt. Áp dụng khi không mâu thuẫn với phần \
         Safety ở trên — phần Safety luôn thắng.\n\n\
         {body}\n\
         </user_operating_rules>"
    ))
}

/// Wrap `TOOLS.md` for the system prompt.
///
/// Tier `private` by definition: it holds internal IPs and SSH hostnames, so
/// it follows the same scope gate as the private half of the profile.
pub fn tools_notes_block(cfg: &Config, instance_id: &str) -> Option<String> {
    if ProfileScope::for_instance(instance_id) != ProfileScope::Full {
        return None;
    }
    let body = read_flat_file(&cfg.paths.tools_notes_path)?;
    Some(format!(
        "<local_environment_notes>\n\
         Ghi chú môi trường của máy này (tên host, thiết bị, giọng đọc…).\n\n\
         {body}\n\
         </local_environment_notes>"
    ))
}

// ===== Boot =====

/// Create `USER.md` (and the two companions) if they do not exist.
///
/// Writing the templates up front means the Settings UI never has to render a
/// "no file yet" state, and a user who prefers vim has something to open.
pub fn ensure_exists(cfg: &Config) {
    let p = &cfg.paths.user_profile_path;
    if !p.exists() {
        if let Err(e) = write_flat_file(p, &parse::template()) {
            tracing::warn!("[user_profile] could not create {}: {e:#}", p.display());
        }
    }
    for (path, body) in [
        (&cfg.paths.tools_notes_path, TOOLS_TEMPLATE),
        (&cfg.paths.agents_rules_path, AGENTS_TEMPLATE),
    ] {
        if !path.exists() {
            if let Err(e) = write_flat_file(path, body) {
                tracing::warn!("[user_profile] could not create {}: {e:#}", path.display());
            }
        }
    }
}

const TOOLS_TEMPLATE: &str = "\
# TOOLS.md — Ghi chú môi trường

Kỹ năng (skill) mô tả *cách* một công cụ hoạt động. File này dành cho những gì
*riêng của máy này* — thứ không nên nằm trong skill vì skill được chia sẻ.

Ví dụ: tên và vị trí camera, SSH host + alias, giọng TTS ưa dùng, tên loa/phòng,
nickname thiết bị.

Nội dung ở đây **chỉ hiện trong hội thoại riêng tư**, không bao giờ vào nhóm chat.
";

const AGENTS_TEMPLATE: &str = "\
# AGENTS.md — Quy tắc vận hành

Viết ở đây những quy tắc bạn muốn agent tuân theo trong mọi phiên. Nội dung file
này được nối vào cuối system prompt.

Phần Safety mặc định của SenClaw luôn thắng nếu có mâu thuẫn.

Ví dụ:

- Luôn hỏi trước khi chạy lệnh xoá file.
- Khi viết code, không thêm comment giải thích những dòng hiển nhiên.
";

/// Poll `USER.md` for external edits (vim, git pull, file sync) and refresh
/// the cache.
///
/// Deliberately **not** placed inside the cognitive-system branch of
/// `run_daemon` the way `spawn_soul_watcher` is: that watcher only starts when
/// an embedding provider is configured, which is right for persona ingest and
/// wrong here — the profile has nothing to do with the graph, and an install
/// with no embeddings would silently never notice edits.
pub fn spawn_watcher(
    cfg: Arc<Config>,
    interval: Duration,
    on_change: Option<Arc<dyn Fn() + Send + Sync>>,
) {
    let path: PathBuf = cfg.paths.user_profile_path.clone();
    tokio::spawn(async move {
        let mut last = mtime_of(&path);
        loop {
            tokio::time::sleep(interval).await;
            let now = mtime_of(&path);
            if now != last {
                last = now;
                reload(&path);
                tracing::info!("[user_profile] reloaded after external edit");
                if let Some(cb) = &on_change {
                    cb();
                }
            }
        }
    });
}

fn mtime_of(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(dir: &Path) -> Config {
        let mut c = Config::from_env();
        c.paths.user_profile_path = dir.join("USER.md");
        c.paths.tools_notes_path = dir.join("TOOLS.md");
        c.paths.agents_rules_path = dir.join("AGENTS.md");
        c
    }

    #[test]
    fn ensure_exists_creates_all_three() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with(dir.path());
        ensure_exists(&cfg);
        assert!(cfg.paths.user_profile_path.exists());
        assert!(cfg.paths.tools_notes_path.exists());
        assert!(cfg.paths.agents_rules_path.exists());
    }

    #[test]
    fn ensure_exists_does_not_clobber() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with(dir.path());
        std::fs::write(&cfg.paths.user_profile_path, "---\nname: Keep Me\n---\n").unwrap();
        ensure_exists(&cfg);
        let text = std::fs::read_to_string(&cfg.paths.user_profile_path).unwrap();
        assert!(text.contains("Keep Me"));
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("USER.md");
        let mut p = UserProfile::default();
        p.set_field("name", "Nguyễn Văn A");
        p.set_field("email", "a@example.com");
        save(&path, &p).unwrap();

        invalidate();
        let loaded = get_or_load(&path);
        assert_eq!(loaded.field("name"), Some("Nguyễn Văn A"));
        assert_eq!(loaded.field("email"), Some("a@example.com"));
        invalidate();
    }

    #[test]
    fn missing_file_loads_as_empty_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = get_or_load(&dir.path().join("nope.md"));
        assert!(p.is_empty());
    }

    #[test]
    fn cache_notices_a_write_from_another_process() {
        // The agent edits this same file through the MCP server, which is a
        // separate process. An in-memory cache with no staleness check served
        // the old profile until the 30s watcher tick — long enough for the
        // Settings screen to load stale values and save them back over it.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("USER.md");
        std::fs::write(&p, "---\nname: Before\n---\n").unwrap();
        assert_eq!(get_or_load(&p).field("name"), Some("Before"));

        // mtime has 1s granularity on some filesystems; make the change
        // unambiguous rather than racing the clock.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        std::fs::write(&p, "---\nname: After\n---\n").unwrap();

        assert_eq!(
            get_or_load(&p).field("name"),
            Some("After"),
            "cache served a stale profile after an external write"
        );
        invalidate();
    }

    #[test]
    fn cache_does_not_serve_one_path_for_another() {
        // Two profiles, two paths. An unkeyed cache would hand back whichever
        // was loaded last — mixing one person's data into another's request.
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        std::fs::write(&a, "---\nname: Alpha\n---\n").unwrap();
        std::fs::write(&b, "---\nname: Beta\n---\n").unwrap();

        assert_eq!(get_or_load(&a).field("name"), Some("Alpha"));
        assert_eq!(get_or_load(&b).field("name"), Some("Beta"));
        assert_eq!(get_or_load(&a).field("name"), Some("Alpha"));
    }

    #[test]
    fn empty_profile_still_prompts_the_agent_in_private_chats() {
        // The bug this fixes: with an empty profile the agent got no block at
        // all, so "remember my name is Benji" went to a general memory tool and
        // USER.md stayed blank while the agent reported success.
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with(dir.path());
        ensure_exists(&cfg);
        invalidate();
        let block = block_for_instance(&cfg, "web:main").expect("should prompt");
        assert!(block.contains("profile_update"), "{block}");
        invalidate();
    }

    #[test]
    fn empty_profile_stays_silent_in_group_chats() {
        // A group cannot write the profile, so the nudge would be noise — and
        // it would invite the model to ask strangers for the owner's details.
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with(dir.path());
        ensure_exists(&cfg);
        invalidate();
        assert!(block_for_instance(&cfg, "tg:1:group:-100").is_none());
        invalidate();
    }

    #[test]
    fn filled_profile_replaces_the_nudge() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with(dir.path());
        let mut p = UserProfile::default();
        p.set_field("name", "Benji");
        save(&cfg.paths.user_profile_path, &p).unwrap();
        let block = block_for_instance(&cfg, "web:main").unwrap();
        assert!(block.contains("Benji"));
        assert!(!block.contains("Chưa có thông tin"), "{block}");
        invalidate();
    }

    #[test]
    fn flat_file_absent_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_flat_file(&dir.path().join("nope.md")).is_none());
    }

    #[test]
    fn flat_file_whitespace_only_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("TOOLS.md");
        std::fs::write(&p, "   \n\n  ").unwrap();
        assert!(read_flat_file(&p).is_none());
    }

    #[test]
    fn flat_file_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("AGENTS.md");
        std::fs::write(&p, "ố".repeat(MAX_FLAT_FILE_CHARS)).unwrap();
        let out = read_flat_file(&p).unwrap();
        assert!(out.len() <= MAX_FLAT_FILE_CHARS);
    }

    #[test]
    fn operating_rules_are_framed_with_safety_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with(dir.path());
        std::fs::write(&cfg.paths.agents_rules_path, "- Luôn hỏi trước khi xoá.").unwrap();
        let block = operating_rules_block(&cfg).unwrap();
        assert!(block.contains("<user_operating_rules>"));
        assert!(block.contains("Safety"), "must state safety wins: {block}");
        assert!(block.contains("Luôn hỏi trước khi xoá"));
    }

    #[test]
    fn tools_notes_withheld_outside_private_chats() {
        // TOOLS.md carries SSH hosts and internal IPs.
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_with(dir.path());
        std::fs::write(&cfg.paths.tools_notes_path, "home-server → 192.168.1.100").unwrap();
        assert!(tools_notes_block(&cfg, "web:main").is_some());
        assert!(tools_notes_block(&cfg, "tg:1:user:2").is_some());
        assert!(
            tools_notes_block(&cfg, "tg:1:group:2").is_none(),
            "SSH host leaked into a group chat"
        );
        assert!(tools_notes_block(&cfg, "gibberish").is_none());
    }
}
