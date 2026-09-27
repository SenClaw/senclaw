//! Soul Core MCP server — lets the agent read and update what it knows about
//! its human.
//!
//! Naming per CLAUDE.md: server `senclaw-profile`, tool prefix `profile_`.
//!
//! Unlike the notes/calendar half of `senclaw-space`, this server touches the
//! file **directly** rather than calling back into the daemon over loopback.
//! It can, because everything it needs is in its own env: the path to
//! `USER.md` and the chat JID (`SENCLAW_CHAT_JID`, already injected for the
//! schedule and background servers), which is what decides the tier. The
//! vault is the opposite case — its key lives in the daemon's memory, so it
//! has no choice but to call back.
//!
//! ## Scope
//!
//! `profile_get` returns exactly what the current chat is allowed to see, via
//! the same [`crate::user_profile::render`] used by prompt injection. A group
//! chat asking "what's my owner's email" gets nothing, not a refusal message —
//! there is nothing to refuse with, because the private half never reaches
//! this process's output.

use anyhow::{Context, Result};
use rmcp::ServiceExt;
use std::path::PathBuf;

use crate::user_profile::{self, ProfileScope, Tier};

// ───────────────────────── param structs ─────────────────────────

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct UpdateParams {
    /// Profile field to set, e.g. `name`, `preferred_name`, `email`,
    /// `location`, `timezone`, `language`, `occupation`. Omit when recording
    /// a directive instead.
    #[serde(default)]
    field: Option<String>,
    /// New value for `field`.
    #[serde(default)]
    value: Option<String>,
    /// A behaviour rule to record, phrased as an imperative starting with
    /// Always / Never / Prefer. One rule per call.
    #[serde(default)]
    directive: Option<String>,
    /// When the new directive replaces an existing preference, pass a
    /// distinctive substring of the OLD one. Every active directive containing
    /// it is marked `superseded` in the same write.
    ///
    /// Use this whenever the user CHANGES their mind. Appending a
    /// contradictory rule instead leaves two active directives and the model
    /// then follows whichever it reads first — usually the stale one.
    #[serde(default)]
    supersedes: Option<String>,
    /// `public` (default for directives) or `private`. Public entries are
    /// visible in group chats too, so keep anything identifying private.
    #[serde(default)]
    tier: Option<String>,
}

// ───────────────────────── MCP server ─────────────────────────

#[derive(Clone)]
pub struct McpUserProfileServer {
    path: PathBuf,
    /// Chat JID this MCP session belongs to; decides the tier ceiling.
    /// Empty (unset env) is treated as an unknown context and therefore
    /// public-only, matching the fail-closed rule everywhere else.
    chat_jid: String,
}

impl McpUserProfileServer {
    /// Build from `SENCLAW_USER_PROFILE_PATH`, or `None` when it is absent.
    /// See [`crate::mcp::wiki_server::McpWikiServer::from_env`] for why an
    /// unconfigured child is `None` rather than an error.
    pub fn from_env() -> Result<Option<Self>> {
        let Ok(path) = std::env::var("SENCLAW_USER_PROFILE_PATH") else {
            return Ok(None);
        };
        if path.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            path: PathBuf::from(path),
            chat_jid: std::env::var("SENCLAW_CHAT_JID").unwrap_or_default(),
        }))
    }

    fn scope(&self) -> ProfileScope {
        ProfileScope::for_instance(&self.chat_jid)
    }

    fn get_impl(&self) -> Result<String> {
        let profile = user_profile::reload(&self.path);
        let scope = self.scope();
        match user_profile::render(&profile, scope) {
            Some(block) => Ok(serde_json::json!({
                "profile": block,
                "scope": format!("{scope:?}"),
            })
            .to_string()),
            None => Ok(serde_json::json!({
                "profile": null,
                "scope": format!("{scope:?}"),
                "note": "Chưa có thông tin hồ sơ nào áp dụng cho ngữ cảnh này. \
                         Hỏi người dùng thay vì đoán.",
            })
            .to_string()),
        }
    }

    fn update_impl(&self, p: UpdateParams) -> Result<String> {
        // Writing personal data from a room with strangers in it is not a
        // thing to do: anyone in a group could otherwise dictate the owner's
        // profile, and it would then follow them into every private session.
        if self.scope() != ProfileScope::Full {
            return Ok(serde_json::json!({
                "ok": false,
                "error": "Hồ sơ người dùng chỉ sửa được trong hội thoại riêng tư.",
            })
            .to_string());
        }

        let mut profile = user_profile::reload(&self.path);
        let mut changed: Vec<String> = Vec::new();

        if let Some(field) = p.field.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            let value = p.value.clone().unwrap_or_default();
            profile.set_field(field, value.trim());
            if let Some(t) = p.tier.as_deref() {
                let tier = parse_tier(t);
                if let Some(f) = profile.fields.iter_mut().find(|f| f.key == field) {
                    f.tier = tier;
                }
            }
            changed.push(format!("field:{field}"));
        }

        if let Some(text) = p
            .directive
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let tier = p.tier.as_deref().map(parse_tier).unwrap_or(Tier::Public);
            let observed = chrono::Utc::now().format("%Y-%m-%d").to_string();
            profile.add_directive(text, &observed, tier, p.supersedes.as_deref());
            changed.push("directive".to_string());
        }

        if changed.is_empty() {
            return Ok(serde_json::json!({
                "ok": false,
                "error": "Không có gì để cập nhật: truyền `field`+`value`, hoặc `directive`.",
            })
            .to_string());
        }

        user_profile::save(&self.path, &profile).context("save USER.md")?;
        Ok(serde_json::json!({
            "ok": true,
            "changed": changed,
            "path": self.path.to_string_lossy(),
        })
        .to_string())
    }
}

fn parse_tier(s: &str) -> Tier {
    if s.eq_ignore_ascii_case("public") {
        Tier::Public
    } else {
        Tier::Private
    }
}

fn err_json(e: anyhow::Error) -> String {
    serde_json::json!({ "error": e.to_string() }).to_string()
}

#[rmcp::tool_router(server_handler, vis = "pub")]
impl McpUserProfileServer {
    #[rmcp::tool(
        description = "Đọc hồ sơ người dùng (tên, xưng hô, múi giờ, sở thích…) áp dụng cho \
                       cuộc hội thoại hiện tại. Gọi khi cần biết về chủ mà thông tin chưa có \
                       sẵn trong ngữ cảnh. Trong nhóm chat chỉ trả về phần công khai — \
                       không có nghĩa là thiếu dữ liệu, mà là dữ liệu đó không dành cho nhóm."
    )]
    fn profile_get(&self) -> String {
        self.get_impl().unwrap_or_else(err_json)
    }

    #[rmcp::tool(
        description = "Ghi vào hồ sơ người dùng. Dùng cho SỞ THÍCH ỔN ĐỊNH và SỰ THẬT HỒ SƠ \
                       (tên, xưng hô, múi giờ, cách muốn được trả lời) — không dùng cho quan sát \
                       vụn (dùng memory_save) hay việc đúng giờ (dùng schedule_task). \
                       Khi người dùng ĐỔI Ý, truyền `supersedes` với một đoạn của quy tắc cũ; \
                       nếu chỉ thêm quy tắc mới mâu thuẫn thì model sẽ theo nhầm quy tắc cũ. \
                       Chỉ hoạt động trong hội thoại riêng tư."
    )]
    fn profile_update(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            UpdateParams,
        >,
    ) -> String {
        self.update_impl(p).unwrap_or_else(err_json)
    }
}

/// Start the user-profile MCP server over stdio. Reads config from the env set
/// by [`crate::mcp::helper::user_profile_mcp_config`].
pub async fn run_stdio_server() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let server =
        McpUserProfileServer::from_env()?.context("SENCLAW_USER_PROFILE_PATH not set")?;
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_for(jid: &str, dir: &std::path::Path) -> McpUserProfileServer {
        McpUserProfileServer {
            path: dir.join("USER.md"),
            chat_jid: jid.to_string(),
        }
    }

    fn seed(dir: &std::path::Path) {
        std::fs::write(
            dir.join("USER.md"),
            "---\nname: Nguyễn Văn A\nemail: a@example.com\n---\n",
        )
        .unwrap();
    }

    #[test]
    fn get_in_private_chat_includes_contact_details() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        user_profile::invalidate();
        let out = server_for("web:main", dir.path()).get_impl().unwrap();
        assert!(out.contains("a@example.com"), "{out}");
        user_profile::invalidate();
    }

    #[test]
    fn get_in_group_chat_withholds_contact_details() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        user_profile::invalidate();
        let out = server_for("tg:1:group:-100", dir.path()).get_impl().unwrap();
        assert!(out.contains("Nguyễn Văn A"), "public field missing: {out}");
        assert!(!out.contains("a@example.com"), "email leaked: {out}");
        user_profile::invalidate();
    }

    #[test]
    fn update_is_refused_in_group_chats() {
        // Otherwise anyone in a group could dictate the owner's profile, and
        // it would follow them into every private session afterwards.
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        user_profile::invalidate();
        let out = server_for("tg:1:group:-100", dir.path())
            .update_impl(UpdateParams {
                field: Some("name".into()),
                value: Some("Kẻ Giả Mạo".into()),
                directive: None,
                supersedes: None,
                tier: None,
            })
            .unwrap();
        assert!(out.contains("\"ok\":false"), "{out}");
        let on_disk = std::fs::read_to_string(dir.path().join("USER.md")).unwrap();
        assert!(!on_disk.contains("Kẻ Giả Mạo"));
        user_profile::invalidate();
    }

    #[test]
    fn update_writes_field_in_private_chat() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        user_profile::invalidate();
        let out = server_for("web:main", dir.path())
            .update_impl(UpdateParams {
                field: Some("timezone".into()),
                value: Some("Asia/Ho_Chi_Minh".into()),
                directive: None,
                supersedes: None,
                tier: None,
            })
            .unwrap();
        assert!(out.contains("\"ok\":true"), "{out}");
        let p = user_profile::reload(&dir.path().join("USER.md"));
        assert_eq!(p.field("timezone"), Some("Asia/Ho_Chi_Minh"));
        user_profile::invalidate();
    }

    #[test]
    fn directive_supersedes_the_old_one() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        user_profile::invalidate();
        let s = server_for("web:main", dir.path());
        s.update_impl(UpdateParams {
            field: None,
            value: None,
            directive: Some("Prefer báo cáo chi tiết.".into()),
            supersedes: None,
            tier: None,
        })
        .unwrap();
        s.update_impl(UpdateParams {
            field: None,
            value: None,
            directive: Some("Prefer báo cáo ngắn gọn.".into()),
            supersedes: Some("báo cáo".into()),
            tier: None,
        })
        .unwrap();

        let p = user_profile::reload(&dir.path().join("USER.md"));
        let actives: Vec<_> = p.active_directives().map(|d| d.text.as_str()).collect();
        assert_eq!(actives, vec!["Prefer báo cáo ngắn gọn."], "{actives:?}");
        user_profile::invalidate();
    }

    #[test]
    fn empty_update_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        seed(dir.path());
        user_profile::invalidate();
        let out = server_for("web:main", dir.path())
            .update_impl(UpdateParams {
                field: None,
                value: None,
                directive: None,
                supersedes: None,
                tier: None,
            })
            .unwrap();
        assert!(out.contains("\"ok\":false"), "{out}");
        user_profile::invalidate();
    }

    #[test]
    fn missing_env_yields_none() {
        // Same contract as every other bundled server: unconfigured is not an
        // error, it just means the aggregator skips this child.
        let saved = std::env::var("SENCLAW_USER_PROFILE_PATH").ok();
        std::env::remove_var("SENCLAW_USER_PROFILE_PATH");
        assert!(McpUserProfileServer::from_env().unwrap().is_none());
        if let Some(v) = saved {
            std::env::set_var("SENCLAW_USER_PROFILE_PATH", v);
        }
    }
}
