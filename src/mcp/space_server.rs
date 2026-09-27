//! Space MCP server — personal productivity tools for the SenClaw Space feature.
//!
//! Tools cover: Notes (CRUD + FTS), Calendar (events + reminders),
//! external sync (Google Calendar/Apple Calendar/Apple Notes), and recurring
//! schedule helpers that wrap the TaskScheduler.
//!
//! Tool namespace: `space:*`

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use rmcp::ServiceExt;
use rusqlite::params;
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

use crate::db::Db;
use crate::mcp::schedule_server::ToolResult;
use crate::types::{AgentMode, ContextMode, ScheduleType, ScheduledTask, TaskStatus};

/// Validate an event's "open this" link.
///
/// A calendar event is a button the user taps without reading — often straight
/// off a notification, often half-awake at the scheduled hour. That makes the
/// link field a phishing surface if any app (including one installed from the
/// hub) can point it anywhere. So only **internal Space-App routes** are
/// accepted: `/space/app/<id>` optionally with a query string.
///
/// Rejections are errors, never silent drops — an event whose button quietly
/// does nothing is worse than one that failed to be created.
pub fn sanitize_event_link(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("link rỗng".into());
    }
    if s.len() > 1_000 {
        return Err("link quá dài".into());
    }
    // Reject anything that could resolve off-origin: a scheme, a
    // protocol-relative `//host`, or a backslash the browser normalises to `/`.
    if s.contains(':') && !s.starts_with("/space/app/") {
        return Err(format!(
            "link phải là đường dẫn nội bộ /space/app/…, nhận được: {s}"
        ));
    }
    if s.starts_with("//") || s.contains('\\') {
        return Err("link không được trỏ ra ngoài ứng dụng".into());
    }
    if !s.starts_with("/space/app/") {
        return Err(format!(
            "link phải bắt đầu bằng /space/app/, nhận được: {s}"
        ));
    }
    let path = s.split(['?', '#']).next().unwrap_or("");
    if path.contains("..") {
        return Err("link không được chứa `..`".into());
    }
    let app = path.trim_start_matches("/space/app/");
    let app_id = app.split('/').next().unwrap_or("");
    if app_id.is_empty()
        || !app_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!("id app trong link không hợp lệ: `{app_id}`"));
    }
    Ok(s.to_string())
}

/// Parse a wall-clock datetime string in the HOST-LOCAL timezone into Unix
/// milliseconds. Accepts `YYYY-MM-DD HH:MM[:SS]` (a `T` separator is fine)
/// and bare `YYYY-MM-DD` (midnight). Returns None if unparsable.
pub(crate) fn parse_local_datetime_ms(s: &str) -> Option<i64> {
    use chrono::{Local, NaiveDate, NaiveDateTime};
    let s = s.trim().replace('T', " ");
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(&s, fmt) {
            if let Some(local) = chrono::TimeZone::from_local_datetime(&Local, &dt).earliest() {
                return Some(local.timestamp_millis());
            }
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(&s, "%Y-%m-%d") {
        let dt = d.and_hms_opt(0, 0, 0)?;
        if let Some(local) = chrono::TimeZone::from_local_datetime(&Local, &dt).earliest() {
            return Some(local.timestamp_millis());
        }
    }
    None
}

// ─── Params ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct NoteCreateParams {
    title: String,
    body: String,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    folder_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct NoteUpdateParams {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct NoteSearchParams {
    query: String,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct NoteListParams {
    #[serde(default)]
    folder_id: Option<String>,
    #[serde(default)]
    tag: Option<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct NoteIdParams {
    id: String,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct EventCreateParams {
    title: String,
    /// PREFERRED: start time in the SYSTEM LOCAL timezone,
    /// `YYYY-MM-DD HH:MM` (e.g. "2026-07-03 22:00"). Overrides `start_at`.
    #[serde(default)]
    start_local: Option<String>,
    /// PREFERRED: end time in the SYSTEM LOCAL timezone, `YYYY-MM-DD HH:MM`.
    /// Optional — defaults to 1 hour after the start. Overrides `end_at`.
    #[serde(default)]
    end_local: Option<String>,
    /// Unix milliseconds (UTC epoch). Use `start_local` instead when the time
    /// came from natural language — it avoids timezone math mistakes.
    #[serde(default)]
    start_at: Option<i64>,
    /// Unix milliseconds. Optional — defaults to start + 1 hour.
    #[serde(default)]
    end_at: Option<i64>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    all_day: Option<bool>,
    /// Minutes before event to send reminder (None = no reminder)
    #[serde(default)]
    reminder_min: Option<i64>,
    /// Repeat reminder every N minutes while event is ongoing (None = no re-notification)
    #[serde(default)]
    renotify_min: Option<i64>,
    #[serde(default)]
    color: Option<String>,
    /// Where opening this event should take the user, as an INTERNAL Space-App
    /// route — e.g. `/space/app/study?session=abc`. The calendar shows an
    /// "Open" button when it is set. External URLs are rejected.
    #[serde(default)]
    link: Option<String>,
    /// Id of the Space App that owns `link`.
    #[serde(default)]
    app_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct EventListParams {
    /// Unix ms — range start
    from: i64,
    /// Unix ms — range end
    to: i64,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct EventUpdateParams {
    /// ID of the event to update
    event_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    /// PREFERRED: new start in the SYSTEM LOCAL timezone, `YYYY-MM-DD HH:MM`.
    /// Overrides `start_at`.
    #[serde(default)]
    start_local: Option<String>,
    /// PREFERRED: new end in the SYSTEM LOCAL timezone, `YYYY-MM-DD HH:MM`.
    /// Overrides `end_at`.
    #[serde(default)]
    end_local: Option<String>,
    /// Unix milliseconds
    #[serde(default)]
    start_at: Option<i64>,
    /// Unix milliseconds
    #[serde(default)]
    end_at: Option<i64>,
    #[serde(default)]
    location: Option<String>,
    #[serde(default)]
    all_day: Option<bool>,
    #[serde(default)]
    color: Option<String>,
    /// Internal Space-App route to open from this event, e.g.
    /// `/space/app/study?session=abc`. External URLs are rejected.
    #[serde(default)]
    link: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
    /// Minutes before event to send reminder
    #[serde(default)]
    reminder_min: Option<i64>,
    /// Repeat reminder every N minutes while event is ongoing
    #[serde(default)]
    renotify_min: Option<i64>,
    /// Force-reset the reminder so it fires again (e.g. after changing reminder_min)
    #[serde(default)]
    reset_reminder: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct EventSearchParams {
    /// Keyword to search in title, description, location (leave empty to search by date only)
    #[serde(default)]
    query: Option<String>,
    /// Natural-language or ISO date string for a specific day, e.g. "today", "tomorrow",
    /// "2026-05-10", "next Monday". If provided, only events on that day are returned.
    #[serde(default)]
    date: Option<String>,
    /// Unix ms — search window start (overrides `date` if both given)
    #[serde(default)]
    from: Option<i64>,
    /// Unix ms — search window end (overrides `date` if both given)
    #[serde(default)]
    to: Option<i64>,
    /// Max results (default 50)
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct EventIdParams {
    event_id: String,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct SetReminderParams {
    event_id: String,
    /// Minutes before event
    reminder_min: i64,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct SyncProviderParams {
    /// OAuth2 access token or service credential
    token: String,
    #[serde(default)]
    /// Sync window in days (default 30)
    days: Option<u32>,
    /// CalDAV only: the account the password belongs to (an Apple ID on
    /// iCloud). Basic auth needs both halves; a token alone cannot identify
    /// the principal to discover.
    #[serde(default)]
    username: Option<String>,
    /// CalDAV only: server root. Defaults to iCloud.
    #[serde(default)]
    #[serde(rename = "baseUrl")]
    base_url: Option<String>,
    /// Apple Notes only: how many notes to read (default 500).
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct ScheduleActivityParams {
    /// Human-readable description of the activity / what the agent should do
    prompt: String,
    /// Cron expression (e.g. "0 7 * * *" = every day at 7am)
    cron: String,
    /// Group folder for the scheduled task
    group_folder: String,
    chat_jid: String,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct ListSpaceSchedulesParams {
    group_folder: String,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct RecurringCreateParams {
    /// Yêu cầu gửi cho agent vào mỗi lần lịch chạy.
    prompt: String,
    /// Tên hiển thị cho lịch và chat session đi kèm.
    label: Option<String>,
    /// Giờ chạy theo giờ máy ("HH:MM", 24h). Bắt buộc khi không dùng cron_advanced.
    time_local: Option<String>,
    /// Ngày chạy ("YYYY-MM-DD", giờ máy) — chỉ dùng cho once/once_delete để hẹn
    /// một ngày cụ thể (vd 3 ngày sau, 1 tuần sau). Bỏ trống thì lấy lần kế tiếp
    /// của time_local (hôm nay hoặc mai).
    date_local: Option<String>,
    /// "daily" | "weekdays" | "weekly" | "monthly" | "once" (chạy 1 lần rồi
    /// đánh dấu completed) | "once_delete" (chạy 1 lần rồi xoá luôn). Với
    /// once/once_delete, dùng date_local + time_local để hẹn mốc chạy. Mặc định "daily".
    frequency: Option<String>,
    /// 0=Chủ nhật .. 6=Thứ Bảy, dùng khi frequency = "weekly".
    weekday: Option<u32>,
    /// 1..28, dùng khi frequency = "monthly".
    day_of_month: Option<u32>,
    /// Cron 5 trường (phút giờ ngày tháng thứ). Ghi đè time_local/frequency.
    cron_advanced: Option<String>,
    /// Chế độ chạy: "agent" (mặc định) | "dag" | "plan".
    agent_mode: Option<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct RecurringIdParams {
    /// ID của lịch định kỳ.
    id: String,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct RecurringUpdateParams {
    id: String,
    prompt: Option<String>,
    label: Option<String>,
    /// "active" | "paused" | "completed"
    status: Option<String>,
    time_local: Option<String>,
    /// Ngày chạy ("YYYY-MM-DD") cho once/once_delete.
    date_local: Option<String>,
    frequency: Option<String>,
    weekday: Option<u32>,
    day_of_month: Option<u32>,
    cron_advanced: Option<String>,
    /// "agent" | "dag" | "plan"
    agent_mode: Option<String>,
    /// Folder của agent profile chạy lịch này (vd "ssh"). Quyết định persona,
    /// skills và MCP servers mà agent có khi lịch chạy. Chuỗi rỗng = về Default.
    agent_folder: Option<String>,
    /// LLM config id chạy lịch này. Chuỗi rỗng = dùng model đang active.
    model_id: Option<String>,
}

// ─── Space App management params ─────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct AppListParams {
    /// Lọc theo tên hoặc id (khớp một phần, không phân biệt hoa thường).
    #[serde(default)]
    query: Option<String>,
    /// "all" (mặc định) | "running" | "stopped".
    #[serde(default)]
    status: Option<String>,
    /// true = hỏi thẳng cổng của từng app xem có trả lời không (chậm hơn,
    /// chính xác hơn). Mặc định false: chỉ đọc sổ sách của daemon.
    #[serde(default)]
    probe: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct AppIdParams {
    /// Id app đã cài, lấy từ `space_app_list` (vd "kanban", "luna-calendar").
    app_id: String,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct AppMcpListParams {
    /// Chỉ xem một app. Bỏ trống = toàn bộ app có khai báo MCP.
    #[serde(default)]
    app_id: Option<String>,
    /// Kèm tên từng tool. Mặc định: có khi hỏi một app, không khi liệt kê tất cả.
    #[serde(default)]
    include_tools: Option<bool>,
}

// ─── MCP server struct ────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct McpSpaceServer {
    db: Arc<Db>,
    group_folder: String,
    chat_jid: String,
    /// Talks to the daemon's REST API for the `space_app_*` tools; see
    /// [`crate::mcp::space_apps`] for why app lifecycle cannot be done in-process.
    apps: Arc<crate::mcp::space_apps::SpaceAppsClient>,
}

impl McpSpaceServer {
    /// Build from the DB + chat env trio, or `None` when any is absent. See
    /// [`crate::mcp::wiki_server::McpWikiServer::from_env`] for why an
    /// unconfigured child is `None` rather than an error.
    pub fn from_env() -> Result<Option<Self>> {
        let (Ok(group_folder), Ok(chat_jid)) = (
            std::env::var("SENCLAW_GROUP_FOLDER"),
            std::env::var("SENCLAW_CHAT_JID"),
        ) else {
            return Ok(None);
        };
        let Some(db) = crate::mcp::helper::shared_env_db()? else {
            return Ok(None);
        };
        Ok(Some(Self {
            db,
            group_folder,
            chat_jid,
            apps: Arc::new(crate::mcp::space_apps::SpaceAppsClient::from_env()),
        }))
    }

    fn inner(&self) -> SpaceServer {
        SpaceServer {
            db: self.db.clone(),
        }
    }
}

#[rmcp::tool_router(server_handler, vis = "pub")]
impl McpSpaceServer {
    // ── Notes ──────────────────────────────────────────────────────────────

    #[rmcp::tool(
        description = "Tạo ghi chú mới trong Space. Create a new note with title, body (Markdown), optional tags and folder."
    )]
    fn space_note_create(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            NoteCreateParams,
        >,
    ) -> String {
        self.inner()
            .note_create(p.title, p.body, p.tags, p.folder_id)
            .content
    }

    #[rmcp::tool(description = "Cập nhật ghi chú. Update an existing note by id.")]
    fn space_note_update(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            NoteUpdateParams,
        >,
    ) -> String {
        self.inner()
            .note_update(p.id, p.title, p.body, p.tags)
            .content
    }

    #[rmcp::tool(
        description = "Tìm kiếm ghi chú full-text, có thể lọc theo tag. Full-text search across all notes, optionally filtered by tag."
    )]
    fn space_note_search(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            NoteSearchParams,
        >,
    ) -> String {
        self.inner()
            .note_search(p.query, p.tag, p.limit.unwrap_or(20))
            .content
    }

    #[rmcp::tool(
        description = "Danh sách ghi chú. List notes, optionally filtered by folder or tag."
    )]
    fn space_note_list(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            NoteListParams,
        >,
    ) -> String {
        self.inner().note_list(p.folder_id, p.tag).content
    }

    #[rmcp::tool(description = "Xóa ghi chú (soft delete). Soft-delete a note by id.")]
    fn space_note_delete(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            NoteIdParams,
        >,
    ) -> String {
        self.inner().note_delete(p.id).content
    }

    // ── Calendar ───────────────────────────────────────────────────────────

    #[rmcp::tool(
        description = "Tạo sự kiện lịch mới. Create a calendar event with optional reminder. \
                       Prefer start_local/end_local ('YYYY-MM-DD HH:MM', interpreted in the \
                       system's LOCAL timezone). end time defaults to start + 1 hour."
    )]
    fn space_event_create(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            EventCreateParams,
        >,
    ) -> String {
        // Local-time strings take priority — the model passes wall-clock times
        // straight through and the daemon does the (host-local) epoch math.
        let start = match (&p.start_local, p.start_at) {
            (Some(s), _) => match parse_local_datetime_ms(s) {
                Some(ms) => ms,
                None => {
                    return format!(
                        "Error: cannot parse start_local '{s}' — expected 'YYYY-MM-DD HH:MM'"
                    )
                }
            },
            (None, Some(ms)) => ms,
            (None, None) => return "Error: provide start_local or start_at".to_string(),
        };
        let end = match (&p.end_local, p.end_at) {
            (Some(s), _) => match parse_local_datetime_ms(s) {
                Some(ms) => ms,
                None => {
                    return format!(
                        "Error: cannot parse end_local '{s}' — expected 'YYYY-MM-DD HH:MM'"
                    )
                }
            },
            // No end time given → default to a 1-hour event.
            (None, other) => other.unwrap_or(start + 60 * 60 * 1000),
        };
        self.inner()
            .event_create(
                p.title,
                start,
                end,
                p.description,
                p.location,
                p.all_day.unwrap_or(false),
                p.reminder_min,
                p.renotify_min,
                p.color,
                p.link,
                p.app_id,
                &self.group_folder,
                &self.chat_jid,
            )
            .content
    }

    #[rmcp::tool(
        description = "Lấy danh sách sự kiện trong khoảng thời gian. List events between from..to (unix ms)."
    )]
    fn space_event_list(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            EventListParams,
        >,
    ) -> String {
        self.inner().event_list(p.from, p.to).content
    }

    #[rmcp::tool(
        description = "Cập nhật sự kiện lịch. Update any field of an existing calendar event by id. \
                       Only provided fields are changed — omit fields you don't want to modify. \
                       Prefer start_local/end_local ('YYYY-MM-DD HH:MM', system LOCAL timezone); \
                       start_at/end_at are Unix milliseconds."
    )]
    fn space_event_update(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            EventUpdateParams,
        >,
    ) -> String {
        let start_at = p
            .start_local
            .as_deref()
            .and_then(parse_local_datetime_ms)
            .or(p.start_at);
        let end_at = p
            .end_local
            .as_deref()
            .and_then(parse_local_datetime_ms)
            .or(p.end_at);
        self.inner()
            .event_update(
                p.event_id,
                p.title,
                p.description,
                start_at,
                end_at,
                p.location,
                p.all_day,
                p.color,
                p.reminder_min,
                p.renotify_min,
                p.link,
                p.app_id,
                p.reset_reminder.unwrap_or(false),
            )
            .content
    }

    #[rmcp::tool(description = "Tìm kiếm sự kiện theo từ khóa và/hoặc ngày. \
                       Search events by keyword (title/description/location) and/or date. \
                       `date` accepts natural language: 'today', 'tomorrow', 'yesterday', \
                       'hôm nay', 'ngày mai', or ISO format 'YYYY-MM-DD'. \
                       `query` filters by keyword within the matched date range. \
                       Examples: {date:'today'}, {query:'họp'}, {query:'react', date:'2026-05-10'}.")]
    fn space_event_search(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            EventSearchParams,
        >,
    ) -> String {
        self.inner()
            .event_search(p.query, p.date, p.from, p.to, p.limit.unwrap_or(50))
            .content
    }

    #[rmcp::tool(
        description = "Xóa sự kiện và hủy nhắc nhở. Delete a calendar event and cancel its reminder task."
    )]
    fn space_event_delete(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            EventIdParams,
        >,
    ) -> String {
        self.inner().event_delete(p.event_id).content
    }

    #[rmcp::tool(
        description = "Đặt nhắc nhở cho sự kiện. Set or update the reminder for an existing event (minutes before start)."
    )]
    fn space_set_reminder(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            SetReminderParams,
        >,
    ) -> String {
        self.inner()
            .set_reminder(
                p.event_id,
                p.reminder_min,
                &self.group_folder,
                &self.chat_jid,
            )
            .content
    }

    #[rmcp::tool(
        description = "Lấy giờ hệ thống local hiện tại. Get the current local system time with full context: \
                       unix timestamp (ms), ISO datetime, Vietnamese formatted string, timezone offset, \
                       day-of-week, and pre-computed start/end ms for today, this week, and this month. \
                       ALWAYS call this first before any query that involves relative time \
                       (hôm nay, tuần này, ngày mai, lúc mấy giờ, etc.)."
    )]
    fn space_current_time(&self) -> String {
        use chrono::{Datelike, Duration, Local, Timelike};
        let now = Local::now();
        let today_start = now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .and_then(|dt| Local.from_local_datetime(&dt).single())
            .map(|dt| dt.timestamp_millis())
            .unwrap_or(now.timestamp_millis());
        let today_end = now
            .date_naive()
            .and_hms_opt(23, 59, 59)
            .and_then(|dt| Local.from_local_datetime(&dt).single())
            .map(|dt| dt.timestamp_millis())
            .unwrap_or(now.timestamp_millis());

        // Start of week (Sunday = 0)
        let days_from_sun = now.weekday().num_days_from_sunday() as i64;
        let week_start_date = now.date_naive() - Duration::days(days_from_sun);
        let week_end_date = week_start_date + Duration::days(6);
        let week_start_ms = week_start_date
            .and_hms_opt(0, 0, 0)
            .and_then(|dt| Local.from_local_datetime(&dt).single())
            .map(|dt| dt.timestamp_millis())
            .unwrap_or(today_start);
        let week_end_ms = week_end_date
            .and_hms_opt(23, 59, 59)
            .and_then(|dt| Local.from_local_datetime(&dt).single())
            .map(|dt| dt.timestamp_millis())
            .unwrap_or(today_end);

        // Start/end of month
        let month_start = now.date_naive().with_day(1).unwrap_or(now.date_naive());
        let month_end = if now.month() == 12 {
            chrono::NaiveDate::from_ymd_opt(now.year() + 1, 1, 1)
        } else {
            chrono::NaiveDate::from_ymd_opt(now.year(), now.month() + 1, 1)
        }
        .map(|d| d.pred_opt().unwrap_or(d))
        .unwrap_or(now.date_naive());
        let month_start_ms = month_start
            .and_hms_opt(0, 0, 0)
            .and_then(|dt| Local.from_local_datetime(&dt).single())
            .map(|dt| dt.timestamp_millis())
            .unwrap_or(today_start);
        let month_end_ms = month_end
            .and_hms_opt(23, 59, 59)
            .and_then(|dt| Local.from_local_datetime(&dt).single())
            .map(|dt| dt.timestamp_millis())
            .unwrap_or(today_end);

        let day_names_vi = [
            "Chủ nhật",
            "Thứ hai",
            "Thứ ba",
            "Thứ tư",
            "Thứ năm",
            "Thứ sáu",
            "Thứ bảy",
        ];
        let dow_vi = day_names_vi[now.weekday().num_days_from_sunday() as usize];
        let tz_offset = now.offset().local_minus_utc() / 3600;
        let tz_sign = if tz_offset >= 0 { "+" } else { "" };

        let result = serde_json::json!({
            "now_ms": now.timestamp_millis(),
            "iso": now.format("%Y-%m-%dT%H:%M:%S").to_string(),
            "display": format!("{}, {:02}/{:02}/{} {:02}:{:02}",
                dow_vi, now.day(), now.month(), now.year(),
                now.hour(), now.minute()),
            "timezone": format!("UTC{tz_sign}{tz_offset}"),
            "year": now.year(),
            "month": now.month(),
            "day": now.day(),
            "hour": now.hour(),
            "minute": now.minute(),
            "day_of_week": now.weekday().num_days_from_sunday(),
            "day_of_week_vi": dow_vi,
            "today": {
                "start_ms": today_start,
                "end_ms": today_end,
                "iso_date": now.format("%Y-%m-%d").to_string(),
            },
            "this_week": {
                "start_ms": week_start_ms,
                "end_ms": week_end_ms,
                "start_date": week_start_date.format("%Y-%m-%d").to_string(),
                "end_date": week_end_date.format("%Y-%m-%d").to_string(),
            },
            "this_month": {
                "start_ms": month_start_ms,
                "end_ms": month_end_ms,
                "start_date": month_start.format("%Y-%m-%d").to_string(),
                "end_date": month_end.format("%Y-%m-%d").to_string(),
            },
            "tomorrow": {
                "start_ms": today_start + 86_400_000,
                "end_ms": today_end + 86_400_000,
                "iso_date": (now.date_naive() + Duration::days(1)).format("%Y-%m-%d").to_string(),
            },
            "yesterday": {
                "start_ms": today_start - 86_400_000,
                "end_ms": today_end - 86_400_000,
                "iso_date": (now.date_naive() - Duration::days(1)).format("%Y-%m-%d").to_string(),
            },
        });
        result.to_string()
    }

    #[rmcp::tool(
        description = "Tóm tắt hôm nay: sự kiện, nhắc nhở, ghi chú gần đây. Today summary: events, reminders, recent notes."
    )]
    fn space_today_summary(&self) -> String {
        self.inner().today_summary().content
    }

    // ── External sync ──────────────────────────────────────────────────────

    #[rmcp::tool(
        description = "Đồng bộ Google Calendar vào lịch Space. Sync Google Calendar into the Space \
                       calendar. `token` is a Google OAuth access token with the calendar scope. \
                       Incremental after the first run. Returns {synced, created, updated, errors, \
                       needsReauth}; when needsReauth is true the token is dead — ask the user to \
                       re-authorise instead of calling again."
    )]
    async fn space_sync_google_calendar(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            SyncProviderParams,
        >,
    ) -> String {
        self.inner()
            .sync_google_calendar(p.token, p.days.unwrap_or(30))
            .await
            .content
    }

    #[rmcp::tool(
        description = "Đồng bộ Apple Calendar (CalDAV) vào lịch Space. Sync a CalDAV calendar \
                       (iCloud by default) into the Space calendar. Needs `username` (the Apple ID) \
                       and `token` (an app-specific password — iCloud rejects the account \
                       password). Read-only: events are imported, never edited back."
    )]
    async fn space_sync_apple_calendar(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            SyncProviderParams,
        >,
    ) -> String {
        self.inner()
            .sync_apple_calendar(
                p.username.unwrap_or_default(),
                p.token,
                p.base_url,
                p.days.unwrap_or(30),
            )
            .await
            .content
    }

    #[rmcp::tool(
        description = "Đồng bộ Apple Notes vào ghi chú Space. Import notes from the local Notes.app \
                       into Space notes. **macOS only** — iCloud Notes is not reachable over IMAP, \
                       so on other systems this reports that rather than returning an empty \
                       success. Needs Automation permission for Notes.app."
    )]
    async fn space_sync_apple_notes(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            SyncProviderParams,
        >,
    ) -> String {
        self.inner()
            .sync_apple_notes(p.limit.unwrap_or(500) as usize)
            .await
            .content
    }

    // ── Recurring schedule ─────────────────────────────────────────────────

    #[rmcp::tool(
        description = "Lên lịch hoạt động định kỳ (ngày/tuần). Schedule a recurring agent activity using a cron expression. Example cron: '0 7 * * *' = every day at 7am."
    )]
    async fn space_schedule_activity(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            ScheduleActivityParams,
        >,
    ) -> String {
        self.inner()
            .schedule_activity(p.prompt, p.cron, p.group_folder, p.chat_jid)
            .await
            .content
    }

    #[rmcp::tool(
        description = "Danh sách lịch định kỳ Space. List all Space recurring schedules for a group."
    )]
    fn space_list_schedules(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            ListSpaceSchedulesParams,
        >,
    ) -> String {
        self.inner().list_schedules(p.group_folder).content
    }

    // ── Recurring schedules (new model: each schedule owns a chat session) ─

    #[rmcp::tool(description = "Tạo lịch định kỳ tự động cho agent. \
Mỗi lịch sẽ tự tạo một chat session riêng và mỗi lần đến giờ agent sẽ chạy prompt trong chat đó. \
Dùng `time_local` (HH:MM, giờ máy) + `frequency` (daily/weekdays/weekly/monthly/once/once_delete), hoặc `cron_advanced` (5 trường). \
once = chạy 1 lần rồi giữ lại (completed); once_delete = chạy 1 lần rồi tự xoá lịch. \
VD: prompt='Tìm giá vàng SJC hôm nay', time_local='07:00', frequency='daily'.")]
    async fn space_recurring_create(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            RecurringCreateParams,
        >,
    ) -> String {
        self.inner()
            .recurring_create(
                p.prompt,
                p.label,
                p.time_local,
                p.date_local,
                p.frequency,
                p.weekday,
                p.day_of_month,
                p.cron_advanced,
                p.agent_mode,
                None,
                None,
            )
            .await
            .content
    }

    #[rmcp::tool(
        description = "Liệt kê toàn bộ lịch định kỳ tự động (mỗi mục có id, label, prompt, chat_jid, schedule_value, status, next_run, last_run, last_status)."
    )]
    fn space_recurring_list(&self) -> String {
        self.inner().recurring_list().content
    }

    #[rmcp::tool(description = "Lấy chi tiết một lịch định kỳ kèm lịch sử 20 lần chạy gần nhất.")]
    fn space_recurring_get(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            RecurringIdParams,
        >,
    ) -> String {
        self.inner().recurring_get(&p.id).content
    }

    #[rmcp::tool(
        description = "Cập nhật lịch định kỳ. Có thể đổi prompt, label, lịch (time_local+frequency hoặc cron_advanced), status (active/paused/completed), và agent_folder (profile chạy lịch)."
    )]
    fn space_recurring_update(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            RecurringUpdateParams,
        >,
    ) -> String {
        self.inner()
            .recurring_update(
                &p.id,
                p.prompt,
                p.label,
                p.status,
                p.time_local,
                p.date_local,
                p.frequency,
                p.weekday,
                p.day_of_month,
                p.cron_advanced,
                p.agent_mode,
                p.agent_folder,
                p.model_id,
            )
            .content
    }

    #[rmcp::tool(
        description = "Xoá lịch định kỳ và chat session đi kèm. Hành động này không thể hoàn tác."
    )]
    fn space_recurring_delete(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            RecurringIdParams,
        >,
    ) -> String {
        self.inner().recurring_delete(&p.id).content
    }

    // ── Space App lifecycle ────────────────────────────────────────────────

    #[rmcp::tool(description = "Liệt kê Space App đã cài kèm trạng thái chạy. \
List installed Space Apps with lifecycle state: id, name, kind, mode, running, userStopped, port, launches, idle. \
Dùng tool này TRƯỚC khi start/stop để lấy đúng `app_id`. \
Lưu ý: app `session` KHÔNG chạy là trạng thái bình thường — nó tự bật khi được mở hoặc khi một tool của nó được gọi; \
chỉ app `background` mới được daemon giữ chạy liên tục. \
`running` là sổ sách của daemon; đặt probe=true để hỏi thẳng cổng xem app có thực sự trả lời.")]
    async fn space_app_list(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            AppListParams,
        >,
    ) -> String {
        self.apps
            .list(p.query, p.status, p.probe.unwrap_or(false))
            .await
            .content
    }

    #[rmcp::tool(description = "Bật (khởi động) một Space App ngay và chờ nó trả lời. \
Start a Space App's server process now and wait until it is healthy. \
Cũng đăng ký lại MCP server của app, và xoá cờ 'người dùng đã tắt' nếu có. \
Không sao nếu app đang chạy sẵn. Nếu khởi động lỗi, kết quả kèm phần đuôi log của app.")]
    async fn space_app_start(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            AppIdParams,
        >,
    ) -> String {
        self.apps.start(&p.app_id).await.content
    }

    #[rmcp::tool(
        description = "Tắt một Space App ngay. Stop a Space App's server process now. \
App `session` sẽ tự bật lại khi được mở hoặc khi tool của nó được gọi — tắt chỉ là làm sớm việc bộ dọn rác sẽ làm. \
App `background` sẽ NẰM IM cho tới khi start lại, kể cả sau khi daemon khởi động lại theo chu kỳ giám sát; \
nếu app đó đang trực một kênh (poll tin nhắn, chạy lịch), việc trực sẽ dừng. \
Hãy xác nhận với người dùng trước khi tắt một app `background`."
    )]
    async fn space_app_stop(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            AppIdParams,
        >,
    ) -> String {
        self.apps.stop(&p.app_id).await.content
    }

    #[rmcp::tool(
        description = "Khởi động lại một Space App: giết tiến trình cũ (kể cả tiến trình mồ côi đang giữ cổng), \
đợi cổng được nhả, rồi chạy lại và đăng ký lại MCP. Restart a Space App. \
Chạy được cả khi app đang tắt. Dùng khi app treo, trả lời sai, hoặc vừa được cập nhật."
    )]
    async fn space_app_restart(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            AppIdParams,
        >,
    ) -> String {
        self.apps.restart(&p.app_id).await.content
    }

    #[rmcp::tool(
        description = "Liệt kê MCP server theo từng Space App: tên server, trạng thái kết nối, số tool (và tên tool khi hỏi một app). \
List the MCP server each Space App registers, with connection status and tool count. \
Đây là cách tra tên tool đầy đủ để gọi: `mcp__<mcpName>__<tool>` (vd `mcp__ssh-manager-mcp__ssh_execute_command`). \
Tên server lấy từ manifest của app, KHÔNG suy ra từ id app. \
App đang tắt vẫn giữ tool trong danh sách — lần gọi đầu tiên sẽ tự bật app."
    )]
    async fn space_app_mcp_list(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            AppMcpListParams,
        >,
    ) -> String {
        self.apps.mcp_list(p.app_id, p.include_tools).await.content
    }
}

// ─── Business logic ──────────────────────────────────────────────────────────

pub struct SpaceServer {
    db: Arc<Db>,
}

impl SpaceServer {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }

    // ── Notes ──────────────────────────────────────────────────────────────

    pub fn note_create(
        &self,
        title: String,
        body: String,
        tags: Option<Vec<String>>,
        folder_id: Option<String>,
    ) -> ToolResult {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().timestamp_millis();
        let tags_json = serde_json::to_string(&tags.unwrap_or_default()).unwrap_or_default();

        let result = self.db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO space_notes (id, title, body, tags, folder_id, pinned, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?6)",
                params![id, title, body, tags_json, folder_id, now],
            )?;
            Ok(())
        });

        match result {
            Ok(_) => ToolResult::ok(
                serde_json::json!({ "success": true, "id": id, "created_at": now }).to_string(),
            ),
            Err(e) => ToolResult::err(format!("Failed to create note: {e}")),
        }
    }

    pub fn note_update(
        &self,
        id: String,
        title: Option<String>,
        body: Option<String>,
        tags: Option<Vec<String>>,
    ) -> ToolResult {
        let now = Utc::now().timestamp_millis();
        let result = self.db.with_conn(|conn| {
            if let Some(t) = &title {
                conn.execute("UPDATE space_notes SET title=?1, updated_at=?2 WHERE id=?3 AND deleted_at IS NULL", params![t, now, id])?;
            }
            if let Some(b) = &body {
                conn.execute("UPDATE space_notes SET body=?1, updated_at=?2 WHERE id=?3 AND deleted_at IS NULL", params![b, now, id])?;
            }
            if let Some(tg) = &tags {
                let j = serde_json::to_string(tg).unwrap_or_default();
                conn.execute("UPDATE space_notes SET tags=?1, updated_at=?2 WHERE id=?3 AND deleted_at IS NULL", params![j, now, id])?;
            }
            Ok(())
        });
        match result {
            Ok(_) => ToolResult::ok(serde_json::json!({ "success": true, "id": id }).to_string()),
            Err(e) => ToolResult::err(format!("Failed to update note: {e}")),
        }
    }

    pub fn note_search(&self, query: String, tag: Option<String>, limit: u32) -> ToolResult {
        let result = self.db.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT n.id, n.title, n.tags,
                        snippet(space_notes_fts, 2, '<b>', '</b>', '...', 20) AS excerpt
                 FROM space_notes_fts f
                 JOIN space_notes n ON n.id = f.id
                 WHERE f.space_notes_fts MATCH ?1 AND n.deleted_at IS NULL
                 ORDER BY rank LIMIT ?2",
            )?;
            let rows: Vec<serde_json::Value> = stmt
                .query_map(params![query, limit], |row| {
                    Ok(serde_json::json!({
                        "id": row.get::<_,String>(0)?,
                        "title": row.get::<_,String>(1)?,
                        "tags": serde_json::from_str::<serde_json::Value>(
                            &row.get::<_,String>(2).unwrap_or_default()
                        ).unwrap_or_default(),
                        "excerpt": row.get::<_,String>(3)?,
                    }))
                })?
                .filter_map(|r| r.ok())
                .filter(|v| {
                    if let Some(t) = &tag {
                        v["tags"]
                            .as_array()
                            .map(|arr| arr.iter().any(|x| x.as_str() == Some(t.as_str())))
                            .unwrap_or(false)
                    } else {
                        true
                    }
                })
                .collect();
            Ok(rows)
        });
        match result {
            Ok(rows) => ToolResult::ok(serde_json::to_string_pretty(&rows).unwrap_or_default()),
            Err(e) => ToolResult::err(format!("Search failed: {e}")),
        }
    }

    pub fn note_list(&self, folder_id: Option<String>, tag: Option<String>) -> ToolResult {
        let result = self.db.with_conn(|conn| {
            let sql = match (&folder_id, &tag) {
                (Some(_), _) => "SELECT id, title, tags, created_at, updated_at FROM space_notes WHERE deleted_at IS NULL AND folder_id=?1 ORDER BY pinned DESC, updated_at DESC LIMIT 100",
                _ => "SELECT id, title, tags, created_at, updated_at FROM space_notes WHERE deleted_at IS NULL ORDER BY pinned DESC, updated_at DESC LIMIT 100",
            };
            let mut stmt = conn.prepare(sql)?;
            let param: &[&dyn rusqlite::ToSql] = if folder_id.is_some() {
                &[&folder_id]
            } else {
                &[]
            };
            let rows: Vec<serde_json::Value> = stmt
                .query_map(param, |row| {
                    Ok(serde_json::json!({
                        "id": row.get::<_,String>(0)?,
                        "title": row.get::<_,String>(1)?,
                        "tags": row.get::<_,String>(2)?,
                        "created_at": row.get::<_,i64>(3)?,
                        "updated_at": row.get::<_,i64>(4)?,
                    }))
                })?
                .filter_map(|r| r.ok())
                .filter(|v| {
                    // client-side tag filter (tags stored as JSON array)
                    if let Some(t) = &tag {
                        v["tags"].as_str().unwrap_or("[]").contains(t.as_str())
                    } else {
                        true
                    }
                })
                .collect();
            Ok(rows)
        });
        match result {
            Ok(rows) => ToolResult::ok(serde_json::to_string_pretty(&rows).unwrap_or_default()),
            Err(e) => ToolResult::err(format!("List notes failed: {e}")),
        }
    }

    pub fn note_delete(&self, id: String) -> ToolResult {
        let now = Utc::now().timestamp_millis();
        let result = self.db.with_conn(|conn| {
            conn.execute(
                "UPDATE space_notes SET deleted_at=?1 WHERE id=?2",
                params![now, id],
            )?;
            Ok(())
        });
        match result {
            Ok(_) => ToolResult::ok(serde_json::json!({ "success": true, "id": id }).to_string()),
            Err(e) => ToolResult::err(format!("Delete note failed: {e}")),
        }
    }

    // ── Calendar ───────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub fn event_create(
        &self,
        title: String,
        start_at: i64,
        end_at: i64,
        description: Option<String>,
        location: Option<String>,
        all_day: bool,
        reminder_min: Option<i64>,
        renotify_min: Option<i64>,
        color: Option<String>,
        link: Option<String>,
        app_id: Option<String>,
        group_folder: &str,
        chat_jid: &str,
    ) -> ToolResult {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().timestamp_millis();
        // A rejected link must not silently become a no-op button: refuse the
        // whole create so the caller fixes its route.
        let link = match link.as_deref().map(sanitize_event_link) {
            Some(Err(e)) => return ToolResult::err(e),
            Some(Ok(v)) => Some(v),
            None => None,
        };

        let result = self.db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO space_events (id, title, description, start_at, end_at, all_day, location, color, reminder_min, renotify_min, link, app_id, source, created_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,'manual',?13,?13)",
                params![id, title, description, start_at, end_at, all_day as i32, location, color, reminder_min, renotify_min, link, app_id, now],
            )?;
            Ok(())
        });

        if let Err(e) = result {
            return ToolResult::err(format!("Failed to create event: {e}"));
        }

        // If reminder requested, register a scheduled_task of type notify
        if let Some(min) = reminder_min {
            let run_at_ms = start_at - min * 60 * 1000;
            let run_at = chrono::DateTime::from_timestamp_millis(run_at_ms)
                .map(|t| t.to_rfc3339())
                .unwrap_or_default();
            let prompt = format!("Nhắc nhở: sự kiện '{title}' bắt đầu sau {min} phút.");
            let task = ScheduledTask {
                id: Uuid::new_v4().to_string(),
                group_folder: group_folder.to_owned(),
                chat_jid: chat_jid.to_owned(),
                prompt,
                schedule_type: ScheduleType::Once,
                schedule_value: run_at.clone(),
                context_mode: ContextMode::Notify,
                agent_mode: AgentMode::Agent,
                script_command: None,
                watch_json: None,
                next_run: Some(run_at),
                last_run: None,
                last_result: None,
                status: TaskStatus::Active,
                created_at: Utc::now().to_rfc3339(),
            };
            if let Err(e) = self.db.insert_task(&task) {
                tracing::warn!("Space: failed to register reminder task: {e}");
            }
        }

        ToolResult::ok(
            serde_json::json!({ "success": true, "id": id, "created_at": now }).to_string(),
        )
    }

    pub fn event_list(&self, from: i64, to: i64) -> ToolResult {
        let result = self.db.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, title, description, start_at, end_at, all_day, location, color,
                        reminder_min, source, status, renotify_min, link, app_id
                 FROM space_events
                 WHERE deleted_at IS NULL AND start_at >= ?1 AND start_at <= ?2
                 ORDER BY start_at ASC",
            )?;
            let rows: Vec<serde_json::Value> = stmt
                .query_map(params![from, to], |row| {
                    Ok(serde_json::json!({
                        "id": row.get::<_,String>(0)?,
                        "title": row.get::<_,String>(1)?,
                        "description": row.get::<_,Option<String>>(2)?,
                        "start_at": row.get::<_,i64>(3)?,
                        "end_at": row.get::<_,i64>(4)?,
                        "all_day": row.get::<_,i32>(5)? != 0,
                        "location": row.get::<_,Option<String>>(6)?,
                        "color": row.get::<_,Option<String>>(7)?,
                        "reminder_min": row.get::<_,Option<i64>>(8)?,
                        "source": row.get::<_,String>(9)?,
                        "status": row.get::<_,Option<String>>(10)?.unwrap_or_else(|| "upcoming".into()),
                        "renotify_min": row.get::<_,Option<i64>>(11)?,
                        "link": row.get::<_,Option<String>>(12)?,
                        "app_id": row.get::<_,Option<String>>(13)?,
                    }))
                })?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        });
        match result {
            Ok(rows) => ToolResult::ok(serde_json::to_string_pretty(&rows).unwrap_or_default()),
            Err(e) => ToolResult::err(format!("List events failed: {e}")),
        }
    }

    pub fn event_update(
        &self,
        event_id: String,
        title: Option<String>,
        description: Option<String>,
        start_at: Option<i64>,
        end_at: Option<i64>,
        location: Option<String>,
        all_day: Option<bool>,
        color: Option<String>,
        reminder_min: Option<i64>,
        renotify_min: Option<i64>,
        link: Option<String>,
        app_id: Option<String>,
        reset_reminder: bool,
    ) -> ToolResult {
        let link = match link.as_deref().map(sanitize_event_link) {
            Some(Err(e)) => return ToolResult::err(e),
            Some(Ok(v)) => Some(v),
            None => None,
        };
        let result = self.db.with_conn(|conn| {
            let now_ms = chrono::Utc::now().timestamp_millis();
            if let Some(v) = &title {
                conn.execute("UPDATE space_events SET title=?1 WHERE id=?2 AND deleted_at IS NULL", params![v, event_id])?;
            }
            if description.is_some() {
                conn.execute("UPDATE space_events SET description=?1 WHERE id=?2 AND deleted_at IS NULL", params![description, event_id])?;
            }
            if let Some(v) = start_at {
                // Re-arm reminder + start notifications when the event moves.
                conn.execute(
                    "UPDATE space_events SET start_at=?1, reminder_sent_at=NULL, start_sent_at=NULL WHERE id=?2 AND deleted_at IS NULL",
                    params![v, event_id],
                )?;
            }
            if let Some(v) = end_at {
                conn.execute("UPDATE space_events SET end_at=?1 WHERE id=?2 AND deleted_at IS NULL", params![v, event_id])?;
            }
            if location.is_some() {
                conn.execute("UPDATE space_events SET location=?1 WHERE id=?2 AND deleted_at IS NULL", params![location, event_id])?;
            }
            if let Some(v) = all_day {
                conn.execute("UPDATE space_events SET all_day=?1 WHERE id=?2 AND deleted_at IS NULL", params![v as i32, event_id])?;
            }
            if color.is_some() {
                conn.execute("UPDATE space_events SET color=?1 WHERE id=?2 AND deleted_at IS NULL", params![color, event_id])?;
            }
            if let Some(v) = reminder_min {
                conn.execute("UPDATE space_events SET reminder_min=?1 WHERE id=?2 AND deleted_at IS NULL", params![v, event_id])?;
            }
            if let Some(v) = renotify_min {
                conn.execute("UPDATE space_events SET renotify_min=?1 WHERE id=?2 AND deleted_at IS NULL", params![v, event_id])?;
            }
            if link.is_some() {
                conn.execute("UPDATE space_events SET link=?1 WHERE id=?2 AND deleted_at IS NULL", params![link, event_id])?;
            }
            if app_id.is_some() {
                conn.execute("UPDATE space_events SET app_id=?1 WHERE id=?2 AND deleted_at IS NULL", params![app_id, event_id])?;
            }
            if reset_reminder {
                // Clear sent flags so EventNotifier fires the reminder again.
                conn.execute(
                    "UPDATE space_events SET reminder_sent_at=NULL, renotify_sent_at=NULL, start_sent_at=NULL WHERE id=?1 AND deleted_at IS NULL",
                    params![event_id],
                )?;
            }
            conn.execute(
                "UPDATE space_events SET updated_at=?1 WHERE id=?2 AND deleted_at IS NULL",
                params![now_ms, event_id],
            )?;
            Ok(())
        });
        match result {
            Ok(_) => {
                ToolResult::ok(serde_json::json!({ "success": true, "id": event_id }).to_string())
            }
            Err(e) => ToolResult::err(format!("Update event failed: {e}")),
        }
    }

    /// Search events by keyword and/or date.
    /// `date` accepts: "today", "tomorrow", "yesterday", ISO date "YYYY-MM-DD".
    /// Returns events sorted by start_at ascending.
    pub fn event_search(
        &self,
        query: Option<String>,
        date: Option<String>,
        from_ms: Option<i64>,
        to_ms: Option<i64>,
        limit: u32,
    ) -> ToolResult {
        // Resolve time window
        let (range_from, range_to) = if let (Some(f), Some(t)) = (from_ms, to_ms) {
            (f, t)
        } else if let Some(ref d) = date {
            match resolve_date(d) {
                Some((f, t)) => (f, t),
                None => {
                    return ToolResult::err(format!(
                        "Không nhận dạng được ngày: '{d}'. \
                         Dùng 'today', 'tomorrow', 'yesterday' hoặc định dạng YYYY-MM-DD."
                    ));
                }
            }
        } else {
            // Default: next 30 days
            let now = Utc::now().timestamp_millis();
            (now, now + 30 * 24 * 3600 * 1000)
        };

        let kw = query.as_deref().unwrap_or("").trim().to_lowercase();
        let result = self.db.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, title, description, start_at, end_at, all_day, location, color,
                        reminder_min, source, status, renotify_min, link, app_id
                 FROM space_events
                 WHERE deleted_at IS NULL
                   AND start_at >= ?1 AND start_at <= ?2
                 ORDER BY start_at ASC
                 LIMIT ?3",
            )?;
            let rows: Vec<serde_json::Value> = stmt
                .query_map(params![range_from, range_to, limit as i64], |row| {
                    Ok(serde_json::json!({
                        "id":           row.get::<_,String>(0)?,
                        "title":        row.get::<_,String>(1)?,
                        "description":  row.get::<_,Option<String>>(2)?,
                        "start_at":     row.get::<_,i64>(3)?,
                        "end_at":       row.get::<_,i64>(4)?,
                        "all_day":      row.get::<_,i32>(5)? != 0,
                        "location":     row.get::<_,Option<String>>(6)?,
                        "color":        row.get::<_,Option<String>>(7)?,
                        "reminder_min": row.get::<_,Option<i64>>(8)?,
                        "source":       row.get::<_,String>(9)?,
                        "status":       row.get::<_,Option<String>>(10)?.unwrap_or_else(|| "upcoming".into()),
                        "renotify_min": row.get::<_,Option<i64>>(11)?,
                        "link": row.get::<_,Option<String>>(12)?,
                        "app_id": row.get::<_,Option<String>>(13)?,
                    }))
                })?
                .filter_map(|r| r.ok())
                .filter(|ev| {
                    if kw.is_empty() {
                        return true;
                    }
                    let title = ev["title"].as_str().unwrap_or("").to_lowercase();
                    let desc = ev["description"].as_str().unwrap_or("").to_lowercase();
                    let loc = ev["location"].as_str().unwrap_or("").to_lowercase();
                    title.contains(&kw) || desc.contains(&kw) || loc.contains(&kw)
                })
                .collect();
            Ok(rows)
        });
        match result {
            Ok(rows) => ToolResult::ok(serde_json::to_string_pretty(&rows).unwrap_or_default()),
            Err(e) => ToolResult::err(format!("Search events failed: {e}")),
        }
    }

    pub fn event_delete(&self, event_id: String) -> ToolResult {
        let now = Utc::now().timestamp_millis();
        let result = self.db.with_conn(|conn| {
            conn.execute(
                "UPDATE space_events SET deleted_at=?1 WHERE id=?2",
                params![now, event_id],
            )?;
            Ok(())
        });
        match result {
            Ok(_) => {
                ToolResult::ok(serde_json::json!({ "success": true, "id": event_id }).to_string())
            }
            Err(e) => ToolResult::err(format!("Delete event failed: {e}")),
        }
    }

    pub fn set_reminder(
        &self,
        event_id: String,
        reminder_min: i64,
        group_folder: &str,
        chat_jid: &str,
    ) -> ToolResult {
        // Read event to get start_at and title
        let event = self.db.with_conn(|conn| {
            conn.query_row(
                "SELECT title, start_at FROM space_events WHERE id=?1 AND deleted_at IS NULL",
                params![event_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .map_err(|e| anyhow::anyhow!(e))
        });

        match event {
            Err(e) => return ToolResult::err(format!("Event not found: {e}")),
            Ok((title, start_at)) => {
                let _ = self.db.with_conn(|conn| {
                    conn.execute(
                        "UPDATE space_events SET reminder_min=?1 WHERE id=?2",
                        params![reminder_min, event_id],
                    )?;
                    Ok(())
                });

                let run_at_ms = start_at - reminder_min * 60 * 1000;
                let run_at = chrono::DateTime::from_timestamp_millis(run_at_ms)
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_default();
                let task = ScheduledTask {
                    id: Uuid::new_v4().to_string(),
                    group_folder: group_folder.to_owned(),
                    chat_jid: chat_jid.to_owned(),
                    prompt: format!("Nhắc nhở: '{title}' bắt đầu sau {reminder_min} phút."),
                    schedule_type: ScheduleType::Once,
                    schedule_value: run_at.clone(),
                    context_mode: ContextMode::Notify,
                    agent_mode: AgentMode::Agent,
                    script_command: None,
                    watch_json: None,
                    next_run: Some(run_at),
                    last_run: None,
                    last_result: None,
                    status: TaskStatus::Active,
                    created_at: Utc::now().to_rfc3339(),
                };
                let _ = self.db.insert_task(&task);

                ToolResult::ok(
                    serde_json::json!({ "success": true, "event_id": event_id, "reminder_min": reminder_min })
                        .to_string(),
                )
            }
        }
    }

    pub fn today_summary(&self) -> ToolResult {
        let _now_ms = Utc::now().timestamp_millis();
        // Start of today (UTC midnight)
        let today_start = {
            let t = Utc::now();
            chrono::DateTime::<Utc>::from(
                chrono::NaiveDateTime::new(t.date_naive(), chrono::NaiveTime::MIN).and_utc(),
            )
            .timestamp_millis()
        };
        let today_end = today_start + 86_400_000;

        let events_result = self.event_list(today_start, today_end);
        let recent_notes = self.note_list(None, None);

        let summary = serde_json::json!({
            "date": chrono::Utc::now().format("%Y-%m-%d").to_string(),
            "events": serde_json::from_str::<serde_json::Value>(&events_result.content).unwrap_or_default(),
            "recent_notes": serde_json::from_str::<serde_json::Value>(&recent_notes.content).unwrap_or_default(),
        });
        ToolResult::ok(serde_json::to_string_pretty(&summary).unwrap_or_default())
    }

    // ── External sync ─────────────────────────────────────────────────────
    //
    // These three took a credential, answered "Token received and stored",
    // and did nothing — which a user reads as a sync that worked. Each one
    // now performs the sync or says why it cannot. Implementations live in
    // [`crate::mcp::space_sync`].

    pub async fn sync_google_calendar(&self, token: String, days: u32) -> ToolResult {
        use crate::mcp::space_sync::google_calendar;
        let report = google_calendar::sync(
            &self.db,
            &token,
            days,
            google_calendar::default_api_base(),
        )
        .await;
        Self::finish_sync(&self.db, google_calendar::SOURCE, report)
    }

    pub async fn sync_apple_calendar(
        &self,
        username: String,
        password: String,
        base_url: Option<String>,
        days: u32,
    ) -> ToolResult {
        use crate::mcp::space_sync::caldav;
        if username.trim().is_empty() {
            return ToolResult::err(
                "CalDAV needs the account name as well as the password: pass `username` \
                 (your Apple ID on iCloud)."
                    .to_string(),
            );
        }
        let base = base_url.unwrap_or_else(|| caldav::ICLOUD_BASE.to_string());
        let report = caldav::sync(&self.db, &base, &username, &password, days).await;
        Self::finish_sync(&self.db, caldav::SOURCE, report)
    }

    pub async fn sync_apple_notes(&self, limit: usize) -> ToolResult {
        use crate::mcp::space_sync::apple_notes;
        let report = apple_notes::sync(&self.db, limit).await;
        Self::finish_sync(&self.db, apple_notes::SOURCE, report)
    }

    /// Record the run for the "last synced" line and render the report.
    fn finish_sync(
        db: &Arc<Db>,
        source: &str,
        report: anyhow::Result<crate::mcp::space_sync::SyncReport>,
    ) -> ToolResult {
        match report {
            Ok(r) => {
                crate::mcp::space_sync::store::record_run(db, source, &r);
                ToolResult::ok(r.to_json().to_string())
            }
            Err(e) => ToolResult::err(format!("{source} sync failed: {e:#}")),
        }
    }

    // ── Recurring schedule (legacy, group-bound) ──────────────────────────

    pub async fn schedule_activity(
        &self,
        prompt: String,
        cron: String,
        group_folder: String,
        chat_jid: String,
    ) -> ToolResult {
        use crate::mcp::schedule_server::ScheduleServer;
        let srv = ScheduleServer::new();
        srv.schedule_task(
            &self.db,
            &group_folder,
            &chat_jid,
            &prompt,
            "cron",
            &cron,
            Some("group"),
            None,
        )
        .await
    }

    pub fn list_schedules(&self, group_folder: String) -> ToolResult {
        use crate::mcp::schedule_server::ScheduleServer;
        ScheduleServer::new().list_tasks(&self.db, &group_folder)
    }

    // ── Recurring schedule (redesigned: each schedule owns a chat session) ─
    //
    // The new model auto-creates a dedicated `groups` row per schedule (jid
    // `schedule:<id>`, folder `schedule_<id>`). Agent output streams into that
    // chat session. Used by the Space UI and the `space_recurring_*` MCP tools.

    #[allow(clippy::too_many_arguments)]
    pub async fn recurring_create(
        &self,
        prompt: String,
        label: Option<String>,
        time_local: Option<String>,
        date_local: Option<String>,
        frequency: Option<String>,
        weekday: Option<u32>,
        day_of_month: Option<u32>,
        cron_advanced: Option<String>,
        agent_mode: Option<String>,
        model_id: Option<String>,
        agent_folder: Option<String>,
    ) -> ToolResult {
        if prompt.trim().is_empty() {
            return ToolResult::err("prompt is required".into());
        }
        let spec = match build_schedule_spec(
            cron_advanced.as_deref(),
            time_local.as_deref(),
            date_local.as_deref(),
            frequency.as_deref(),
            weekday,
            day_of_month,
        ) {
            Ok(s) => s,
            Err(e) => return ToolResult::err(e),
        };
        let (sched_type, sched_value) = match &spec {
            ScheduleSpec::Cron(c) => ("cron", c.clone()),
            ScheduleSpec::Once { at, delete_after } => (
                if *delete_after { "once_delete" } else { "once" },
                at.clone(),
            ),
        };
        let id = Uuid::new_v4().to_string();
        let chat_jid = format!("{SCHEDULE_JID_PREFIX}{id}");
        let group_folder = format!("{SCHEDULE_FOLDER_PREFIX}{id}");
        // The chat session can run under a chosen agent profile; the schedule
        // itself is still tracked by its `schedule_<id>` task folder (recurring_list).
        let binding_folder = agent_folder
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| group_folder.clone());
        let label = label
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned())
            .unwrap_or_else(|| truncate_label(&prompt, 60));

        let now = Utc::now().to_rfc3339();
        if let Err(e) = self.db.upsert_group(&crate::types::GroupBinding {
            jid: chat_jid.clone(),
            folder: binding_folder,
            name: label.clone(),
            channel: String::new(),
            group_type: "chat".into(),
            requires_trigger: false,
            allowed_tools: None,
            allowed_paths: None,
            allowed_work_dirs: None,
            bot_token: None,
            max_messages: None,
            llm_config_id: model_id
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            last_active: Some(now.clone()),
            added_at: now,
        }) {
            return ToolResult::err(format!("create chat session: {e}"));
        }

        let srv = crate::mcp::schedule_server::ScheduleServer::new();
        let result = srv
            .schedule_task(
                &self.db,
                &group_folder,
                &chat_jid,
                &prompt,
                sched_type,
                &sched_value,
                Some("group"),
                None,
            )
            .await;
        if result.is_error {
            let _ = self.db.delete_group(&chat_jid);
            return result;
        }
        let info: serde_json::Value = serde_json::from_str(&result.content).unwrap_or_default();
        let task_id = info
            .get("taskId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();

        let resolved_agent_mode =
            crate::types::AgentMode::parse(agent_mode.as_deref().unwrap_or("agent"));
        if resolved_agent_mode != crate::types::AgentMode::Agent {
            let _ = self.db.with_conn(|c| {
                c.execute(
                    "UPDATE scheduled_tasks SET agent_mode = ?1 WHERE id = ?2",
                    rusqlite::params![resolved_agent_mode.as_str(), &task_id],
                )?;
                Ok(())
            });
        }

        let tasks = match self.db.get_tasks_by_group(&group_folder) {
            Ok(t) => t,
            Err(e) => return ToolResult::err(format!("lookup task: {e}")),
        };
        let task = match tasks.into_iter().find(|t| t.id == task_id) {
            Some(t) => t,
            None => return ToolResult::err("task not found after insert".into()),
        };
        ToolResult::ok(self.serialize_schedule(&task).to_string())
    }

    pub fn recurring_list(&self) -> ToolResult {
        let tasks = match self.db.list_all_tasks() {
            Ok(t) => t,
            Err(e) => return ToolResult::err(format!("list tasks: {e}")),
        };
        let items: Vec<serde_json::Value> = tasks
            .iter()
            .filter(|t| t.group_folder.starts_with(SCHEDULE_FOLDER_PREFIX))
            .map(|t| self.serialize_schedule(t))
            .collect();
        ToolResult::ok(serde_json::Value::Array(items).to_string())
    }

    pub fn recurring_get(&self, id: &str) -> ToolResult {
        let tasks = match self.db.list_all_tasks() {
            Ok(t) => t,
            Err(e) => return ToolResult::err(format!("list tasks: {e}")),
        };
        let task = match tasks
            .into_iter()
            // Accept either the task id or the schedule's JID-derived id
            // (`schedule:<id>` → folder `schedule_<id>`); recurring_create's
            // task id differs from the id baked into the chat JID/folder.
            .find(|t| {
                t.group_folder.starts_with(SCHEDULE_FOLDER_PREFIX)
                    && (t.id == id || t.group_folder == format!("{SCHEDULE_FOLDER_PREFIX}{id}"))
            }) {
            Some(t) => t,
            None => return ToolResult::err(format!("schedule not found: {id}")),
        };
        let runs = self.db.get_task_run_logs(id, 20).unwrap_or_default();
        let mut item = self.serialize_schedule(&task);
        item["runs"] = serde_json::json!(runs
            .iter()
            .map(|l| serde_json::json!({
                "id":          l.id,
                "run_at":      l.run_at,
                "duration_ms": l.duration_ms,
                "status":      l.status.as_str(),
                "result":      l.result,
                "error":       l.error,
            }))
            .collect::<Vec<_>>());
        ToolResult::ok(item.to_string())
    }

    /// Force a recurring schedule to fire on the next scheduler tick by
    /// rewinding `next_run` to "now". The scheduler poll loop (1-10 s
    /// cadence) picks it up like any other due task — same code path, same
    /// run-log entry. The original `next_run` is then recomputed normally
    /// after the executor finishes.
    pub fn recurring_run_now(&self, id: &str) -> ToolResult {
        let tasks = match self.db.list_all_tasks() {
            Ok(t) => t,
            Err(e) => return ToolResult::err(format!("list tasks: {e}")),
        };
        let task = match tasks
            .into_iter()
            // Accept either the task id or the schedule's JID-derived id
            // (`schedule:<id>` → folder `schedule_<id>`); recurring_create's
            // task id differs from the id baked into the chat JID/folder.
            .find(|t| {
                t.group_folder.starts_with(SCHEDULE_FOLDER_PREFIX)
                    && (t.id == id || t.group_folder == format!("{SCHEDULE_FOLDER_PREFIX}{id}"))
            }) {
            Some(t) => t,
            None => return ToolResult::err(format!("schedule not found: {id}")),
        };

        // Subtract 1 second so the comparison in the poller (next_run <= now)
        // is strictly true even on the same wall-clock tick.
        let now = chrono::Utc::now()
            .checked_sub_signed(chrono::Duration::seconds(1))
            .unwrap_or_else(chrono::Utc::now)
            .to_rfc3339();

        if let Err(e) =
            self.db
                .advance_task_next_run(&task.id, Some(&now), crate::types::TaskStatus::Active)
        {
            return ToolResult::err(format!("set next_run: {e}"));
        }
        ToolResult::ok(serde_json::json!({ "id": task.id, "queued_at": now }).to_string())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn recurring_update(
        &self,
        id: &str,
        prompt: Option<String>,
        label: Option<String>,
        status: Option<String>,
        time_local: Option<String>,
        date_local: Option<String>,
        frequency: Option<String>,
        weekday: Option<u32>,
        day_of_month: Option<u32>,
        cron_advanced: Option<String>,
        agent_mode: Option<String>,
        agent_folder: Option<String>,
        model_id: Option<String>,
    ) -> ToolResult {
        let tasks = match self.db.list_all_tasks() {
            Ok(t) => t,
            Err(e) => return ToolResult::err(format!("list tasks: {e}")),
        };
        let task = match tasks
            .into_iter()
            // Accept either the task id or the schedule's JID-derived id
            // (`schedule:<id>` → folder `schedule_<id>`); recurring_create's
            // task id differs from the id baked into the chat JID/folder.
            .find(|t| {
                t.group_folder.starts_with(SCHEDULE_FOLDER_PREFIX)
                    && (t.id == id || t.group_folder == format!("{SCHEDULE_FOLDER_PREFIX}{id}"))
            }) {
            Some(t) => t,
            None => return ToolResult::err(format!("schedule not found: {id}")),
        };
        // The lookup above accepts either the task id or the JID-derived folder
        // id. Every write below must use the id we actually resolved, not the
        // raw path param — otherwise the folder form matches no row and the
        // update silently does nothing.
        let task_id = task.id.clone();

        // Self-heal: config reconciliation used to wipe a schedule's chat
        // session when its folder collided with a config-managed profile
        // folder. Without the `groups` row every UPDATE below matches zero
        // rows and the edit silently does nothing — recreate it first.
        match self.db.get_group(&task.chat_jid) {
            Ok(Some(_)) => {}
            Ok(None) => {
                let now = Utc::now().to_rfc3339();
                if let Err(e) = self.db.upsert_group(&crate::types::GroupBinding {
                    jid: task.chat_jid.clone(),
                    folder: task.group_folder.clone(),
                    name: truncate_label(&task.prompt, 60),
                    channel: String::new(),
                    group_type: "chat".into(),
                    requires_trigger: false,
                    allowed_tools: None,
                    allowed_paths: None,
                    allowed_work_dirs: None,
                    bot_token: None,
                    max_messages: None,
                    llm_config_id: None,
                    last_active: Some(now.clone()),
                    added_at: now,
                }) {
                    return ToolResult::err(format!("recreate chat session: {e}"));
                }
            }
            Err(e) => return ToolResult::err(format!("lookup chat session: {e}")),
        }

        let touches_schedule = cron_advanced.is_some()
            || time_local.is_some()
            || date_local.is_some()
            || frequency.is_some()
            || weekday.is_some()
            || day_of_month.is_some();
        let new_spec = if touches_schedule {
            match build_schedule_spec(
                cron_advanced.as_deref(),
                time_local.as_deref(),
                date_local.as_deref(),
                frequency.as_deref(),
                weekday,
                day_of_month,
            ) {
                Ok(s) => Some(s),
                Err(e) => return ToolResult::err(e),
            }
        } else {
            None
        };

        if let Some(p) = prompt
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            if let Err(e) = self.db.with_conn(|c| {
                c.execute(
                    "UPDATE scheduled_tasks SET prompt = ?1 WHERE id = ?2",
                    rusqlite::params![p, task_id],
                )?;
                Ok(())
            }) {
                return ToolResult::err(format!("update prompt: {e}"));
            }
        }

        if let Some(spec) = &new_spec {
            use crate::types::ScheduleType;
            let (sched_type, sched_value, next_run): (ScheduleType, String, Option<String>) =
                match spec {
                    ScheduleSpec::Cron(c) => {
                        let mut tmp = task.clone();
                        tmp.schedule_type = ScheduleType::Cron;
                        tmp.schedule_value = c.clone();
                        tmp.next_run = None;
                        let next = crate::scheduler::task_scheduler::compute_next_run(&tmp);
                        (ScheduleType::Cron, c.clone(), next)
                    }
                    // One-shot fires exactly at `at`; the scheduler's
                    // compute_next_run returns None for one-shot types, so set
                    // next_run to the target instant directly.
                    ScheduleSpec::Once { at, delete_after } => {
                        let st = if *delete_after {
                            ScheduleType::OnceDelete
                        } else {
                            ScheduleType::Once
                        };
                        (st, at.clone(), Some(at.clone()))
                    }
                };
            if let Err(e) = self.db.with_conn(|c| {
                c.execute(
                    "UPDATE scheduled_tasks SET schedule_type=?1, schedule_value=?2, next_run=?3 WHERE id=?4",
                    rusqlite::params![sched_type.as_str(), sched_value, next_run, task_id],
                )?;
                Ok(())
            }) {
                return ToolResult::err(format!("update schedule: {e}"));
            }
        }

        if let Some(st) = status.as_deref() {
            let parsed = match st {
                "active" => Some(crate::types::TaskStatus::Active),
                "paused" => Some(crate::types::TaskStatus::Paused),
                "completed" => Some(crate::types::TaskStatus::Completed),
                other => return ToolResult::err(format!("unknown status: {other}")),
            };
            if let Some(st) = parsed {
                if let Err(e) = self.db.update_task_status(&task_id, st) {
                    return ToolResult::err(format!("update status: {e}"));
                }
            }
        }

        if let Some(am) = agent_mode
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            let parsed = crate::types::AgentMode::parse(am);
            if let Err(e) = self.db.with_conn(|c| {
                c.execute(
                    "UPDATE scheduled_tasks SET agent_mode = ?1 WHERE id = ?2",
                    rusqlite::params![parsed.as_str(), task_id],
                )?;
                Ok(())
            }) {
                return ToolResult::err(format!("update agent_mode: {e}"));
            }
        }

        if let Some(new_label) = label.as_deref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            if let Err(e) = self.db.with_conn(|c| {
                c.execute(
                    "UPDATE groups SET name = ?1 WHERE jid = ?2",
                    rusqlite::params![new_label, task.chat_jid],
                )?;
                Ok(())
            }) {
                return ToolResult::err(format!("update label: {e}"));
            }
        }

        // The agent profile a schedule runs under IS its chat session's folder
        // (recurring_create's `binding_folder`) — that folder is what the
        // executor resolves persona/skills/MCP from. Without this, editing a
        // schedule could never move it off the bare `schedule_<id>` folder,
        // which has no persona and no MCP servers, so the agent ran with only
        // built-in tools.
        //
        // `None` = the caller didn't mention the profile (e.g. a pause toggle),
        // `Some("")` = "back to Default". They must stay distinct: treating ""
        // as "no change" is what made a profile permanent once set — the
        // dropdown had no way to say "none".
        if let Some(raw) = agent_folder.as_deref() {
            let trimmed = raw.trim();
            // Default = the schedule's own bare `schedule_<id>` folder, which
            // serialize_schedule filters back out to null.
            let new_folder = if trimmed.is_empty() {
                task.group_folder.clone()
            } else {
                trimmed.to_owned()
            };
            if let Err(e) = self.db.with_conn(|c| {
                c.execute(
                    "UPDATE groups SET folder = ?1 WHERE jid = ?2",
                    rusqlite::params![new_folder, task.chat_jid],
                )?;
                Ok(())
            }) {
                return ToolResult::err(format!("update agent profile: {e}"));
            }
        }

        // Same shape for the model: `Some("")` clears back to the active
        // default. Until now update dropped model_id entirely, so the editor's
        // Model dropdown was dead UI — it changed nothing and said nothing.
        if let Some(raw) = model_id.as_deref() {
            let trimmed = raw.trim();
            let value: Option<&str> = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            };
            if let Err(e) = self.db.with_conn(|c| {
                c.execute(
                    "UPDATE groups SET llm_config_id = ?1 WHERE jid = ?2",
                    rusqlite::params![value, task.chat_jid],
                )?;
                Ok(())
            }) {
                return ToolResult::err(format!("update model: {e}"));
            }
        }

        // Re-fetch and return.
        let tasks = match self.db.list_all_tasks() {
            Ok(t) => t,
            Err(e) => return ToolResult::err(format!("list tasks: {e}")),
        };
        let task = match tasks.into_iter().find(|t| t.id == task_id) {
            Some(t) => t,
            None => return ToolResult::err("schedule disappeared after update".into()),
        };
        ToolResult::ok(self.serialize_schedule(&task).to_string())
    }

    pub fn recurring_delete(&self, id: &str) -> ToolResult {
        let tasks = match self.db.list_all_tasks() {
            Ok(t) => t,
            Err(e) => return ToolResult::err(format!("list tasks: {e}")),
        };
        let task = match tasks
            .into_iter()
            // Accept either the task id or the schedule's JID-derived id
            // (`schedule:<id>` → folder `schedule_<id>`); recurring_create's
            // task id differs from the id baked into the chat JID/folder.
            .find(|t| {
                t.group_folder.starts_with(SCHEDULE_FOLDER_PREFIX)
                    && (t.id == id || t.group_folder == format!("{SCHEDULE_FOLDER_PREFIX}{id}"))
            }) {
            Some(t) => t,
            None => return ToolResult::err(format!("schedule not found: {id}")),
        };
        // Delete by the resolved task id — the raw param may be the
        // JID-derived id, which matches no scheduled_tasks row.
        if let Err(e) = self.db.delete_task(&task.id) {
            return ToolResult::err(format!("delete task: {e}"));
        }
        let _ = self.db.delete_group(&task.chat_jid);
        let _ = self.db.delete_group_by_folder(&task.group_folder);
        ToolResult::ok(serde_json::json!({ "success": true, "id": id }).to_string())
    }

    fn serialize_schedule(&self, task: &crate::types::ScheduledTask) -> serde_json::Value {
        let group = self.db.get_group(&task.chat_jid).ok().flatten();
        let label = group
            .as_ref()
            .map(|g| g.name.clone())
            .unwrap_or_else(|| truncate_label(&task.prompt, 40));
        // The chat session's folder doubles as the agent profile. A folder still
        // equal to the schedule's own `schedule_<id>` means "no profile chosen",
        // so report null rather than a folder the profile dropdown can't match.
        let agent_folder = group
            .as_ref()
            .map(|g| g.folder.clone())
            .filter(|f| !f.starts_with(SCHEDULE_FOLDER_PREFIX));
        let logs = self.db.get_task_run_logs(&task.id, 1).unwrap_or_default();
        let last_status = logs.first().map(|l| l.status.as_str().to_owned());
        serde_json::json!({
            "id":              task.id,
            "label":           label,
            "prompt":          task.prompt,
            "chat_jid":        task.chat_jid,
            "group_folder":    task.group_folder,
            "schedule_type":   task.schedule_type.as_str(),
            "schedule_value":  task.schedule_value,
            "agent_mode":      task.agent_mode.as_str(),
            "agent_folder":    agent_folder,
            // The model the schedule runs under, so the editor can round-trip
            // it. Without this the Model dropdown had nothing to restore from
            // and silently reset to "Active default" on every edit.
            "model_id":        group.as_ref().and_then(|g| g.llm_config_id.clone()),
            "status":          task.status.as_str(),
            "next_run":        task.next_run,
            "last_run":        task.last_run,
            "last_status":     last_status,
            "created_at":      task.created_at,
        })
    }
}

// ─── Recurring schedule helpers ──────────────────────────────────────────────

pub(crate) const SCHEDULE_FOLDER_PREFIX: &str = "schedule_";
pub(crate) const SCHEDULE_JID_PREFIX: &str = "schedule:";

/// Interpretation of the schedule form fields: either a recurring cron
/// expression or a one-shot instant.
pub(crate) enum ScheduleSpec {
    /// Recurring — `schedule_value` is a 5-field cron expression.
    Cron(String),
    /// One-shot — `schedule_value` is an RFC3339 instant. `delete_after` picks
    /// `once_delete` (row removed after firing) over `once` (kept, completed).
    Once { at: String, delete_after: bool },
}

/// Interpret the schedule form fields. A `frequency` of `once` / `once_delete`
/// resolves to a single instant: `date_local` ("YYYY-MM-DD") + `time_local`
/// when a date is given, otherwise the next occurrence of `time_local` (today
/// if still ahead, else tomorrow). Every other frequency delegates to
/// [`build_schedule_cron`]. An explicit `cron_advanced` always wins.
pub(crate) fn build_schedule_spec(
    advanced: Option<&str>,
    time_local: Option<&str>,
    date_local: Option<&str>,
    frequency: Option<&str>,
    weekday: Option<u32>,
    day_of_month: Option<u32>,
) -> std::result::Result<ScheduleSpec, String> {
    let has_advanced = advanced.map(|s| !s.trim().is_empty()).unwrap_or(false);
    let freq = frequency.unwrap_or("daily");
    if !has_advanced && (freq == "once" || freq == "once_delete") {
        let time = time_local.unwrap_or("").trim();
        let (h, m) = parse_hhmm(time).ok_or_else(|| "time_local must be HH:MM (24h)".to_owned())?;
        let at = resolve_once_instant(date_local, h, m)?;
        return Ok(ScheduleSpec::Once {
            at,
            delete_after: freq == "once_delete",
        });
    }
    build_schedule_cron(advanced, time_local, frequency, weekday, day_of_month)
        .map(ScheduleSpec::Cron)
}

/// Combine a local date + time into an RFC3339 UTC instant. With no date, falls
/// back to the next occurrence of the time-of-day (today or tomorrow).
fn resolve_once_instant(
    date_local: Option<&str>,
    h: u32,
    m: u32,
) -> std::result::Result<String, String> {
    use chrono::NaiveDate;
    match date_local.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some(d) => {
            let date = NaiveDate::parse_from_str(d, "%Y-%m-%d")
                .map_err(|_| "date_local must be YYYY-MM-DD".to_owned())?;
            local_naive_to_utc_rfc3339(date, h, m)
                .ok_or_else(|| "could not resolve one-shot date/time".to_owned())
        }
        None => {
            next_local_occurrence(h, m).ok_or_else(|| "could not resolve one-shot time".to_owned())
        }
    }
}

/// Attach a local time to a `NaiveDate` and convert to a UTC RFC3339 string,
/// tolerating DST gaps/overlaps.
fn local_naive_to_utc_rfc3339(date: chrono::NaiveDate, h: u32, m: u32) -> Option<String> {
    use chrono::{Duration, Local, LocalResult, TimeZone};
    let naive = date.and_hms_opt(h, m, 0)?;
    let dt = match Local.from_local_datetime(&naive) {
        LocalResult::Single(dt) => dt,
        LocalResult::Ambiguous(dt, _) => dt,
        // Skipped by a DST forward jump — nudge past the gap.
        LocalResult::None => Local
            .from_local_datetime(&(naive + Duration::hours(1)))
            .single()?,
    };
    Some(dt.with_timezone(&Utc).to_rfc3339())
}

/// Resolve an "HH:MM" local wall-clock time to the next RFC3339 UTC instant at
/// or after now — today if the time is still ahead, otherwise tomorrow.
fn next_local_occurrence(h: u32, m: u32) -> Option<String> {
    use chrono::{Duration, Local, LocalResult, TimeZone};
    let now = Local::now();
    let naive = now.date_naive().and_hms_opt(h, m, 0)?;
    let mut candidate = match Local.from_local_datetime(&naive) {
        LocalResult::Single(dt) => dt,
        LocalResult::Ambiguous(dt, _) => dt,
        // Skipped by a DST forward jump — nudge past the gap.
        LocalResult::None => Local
            .from_local_datetime(&(naive + Duration::hours(1)))
            .single()?,
    };
    if candidate <= now {
        candidate += Duration::days(1);
    }
    Some(candidate.with_timezone(&Utc).to_rfc3339())
}

pub(crate) fn build_schedule_cron(
    advanced: Option<&str>,
    time_local: Option<&str>,
    frequency: Option<&str>,
    weekday: Option<u32>,
    day_of_month: Option<u32>,
) -> std::result::Result<String, String> {
    if let Some(raw) = advanced.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if raw.split_whitespace().count() != 5 {
            return Err("cron_advanced must be a 5-field expression".into());
        }
        return Ok(raw.to_owned());
    }
    let time = time_local.unwrap_or("").trim();
    let (h, m) = parse_hhmm(time).ok_or_else(|| "time_local must be HH:MM (24h)".to_owned())?;
    let freq = frequency.unwrap_or("daily");
    Ok(match freq {
        "daily" => format!("{m} {h} * * *"),
        "weekdays" => format!("{m} {h} * * 1-5"),
        "weekly" => {
            let dow = weekday.unwrap_or(1).min(6);
            format!("{m} {h} * * {dow}")
        }
        "monthly" => {
            let dom = day_of_month.unwrap_or(1).clamp(1, 28);
            format!("{m} {h} {dom} * *")
        }
        other => return Err(format!("Unknown frequency: {other}")),
    })
}

fn parse_hhmm(s: &str) -> Option<(u32, u32)> {
    let (h, m) = s.split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some((h, m))
}

pub(crate) fn truncate_label(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max {
        trimmed.to_owned()
    } else {
        let head: String = trimmed.chars().take(max - 1).collect();
        format!("{head}…")
    }
}

// ─── Date resolution helper ───────────────────────────────────────────────────

/// Parse a natural-language or ISO date string into a (start_ms, end_ms) day range.
/// Returns None when the string is not recognized.
fn resolve_date(s: &str) -> Option<(i64, i64)> {
    use chrono::{Datelike, Duration, Local, NaiveDate, TimeZone};

    let s = s.trim().to_lowercase();
    let today = Local::now().date_naive();

    let date: NaiveDate = match s.as_str() {
        "today" | "hôm nay" | "hom nay" => today,
        "tomorrow" | "ngày mai" | "ngay mai" => today + Duration::days(1),
        "yesterday" | "hôm qua" | "hom qua" => today - Duration::days(1),
        "next monday" | "thứ 2 tuần sau" => {
            let days = (7 - today.weekday().num_days_from_monday() as i64 + 7) % 7;
            today + Duration::days(if days == 0 { 7 } else { days })
        }
        "this week" | "tuần này" => today, // treat as "from today through end of week" below
        _ => {
            // Try ISO date YYYY-MM-DD or DD/MM/YYYY
            if let Ok(d) = NaiveDate::parse_from_str(&s, "%Y-%m-%d") {
                d
            } else if let Ok(d) = NaiveDate::parse_from_str(&s, "%d/%m/%Y") {
                d
            } else if let Ok(d) = NaiveDate::parse_from_str(&s, "%d-%m-%Y") {
                d
            } else {
                return None;
            }
        }
    };

    let start = Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0)?)
        .single()?
        .timestamp_millis();
    let end = Local
        .from_local_datetime(&date.and_hms_opt(23, 59, 59)?)
        .single()?
        .timestamp_millis();
    Some((start, end))
}

// ─── stdio server entry point ─────────────────────────────────────────────────

pub async fn run_stdio_server() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let server = McpSpaceServer::from_env()?
        .context("SENCLAW_DB_PATH / SENCLAW_GROUP_FOLDER / SENCLAW_CHAT_JID not set")?;

    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod local_time_tests {
    use super::parse_local_datetime_ms;
    use chrono::{Local, TimeZone};

    #[test]
    fn parses_local_wall_clock() {
        let ms = parse_local_datetime_ms("2026-07-03 22:00").expect("parse");
        let expected = Local
            .with_ymd_and_hms(2026, 7, 3, 22, 0, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(ms, expected);
        // T separator + seconds also accepted.
        assert_eq!(parse_local_datetime_ms("2026-07-03T22:00:00"), Some(ms));
    }

    #[test]
    fn parses_bare_date_as_local_midnight() {
        let ms = parse_local_datetime_ms("2026-07-04").expect("parse");
        let expected = Local
            .with_ymd_and_hms(2026, 7, 4, 0, 0, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(ms, expected);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_local_datetime_ms("tomorrow 10pm"), None);
        assert_eq!(parse_local_datetime_ms(""), None);
    }
}

#[cfg(test)]
mod schedule_spec_tests {
    use super::{build_schedule_spec, sanitize_event_link, ScheduleSpec};
    use chrono::{DateTime, Datelike, Local, Timelike, Utc};

    #[test]
    fn recurring_frequency_builds_cron() {
        match build_schedule_spec(None, Some("09:00"), None, Some("daily"), None, None).unwrap() {
            ScheduleSpec::Cron(c) => assert_eq!(c, "0 9 * * *"),
            _ => panic!("daily should be a cron spec"),
        }
    }

    #[test]
    fn once_resolves_to_future_instant_without_delete() {
        let spec =
            build_schedule_spec(None, Some("16:00"), None, Some("once"), None, None).unwrap();
        match spec {
            ScheduleSpec::Once { at, delete_after } => {
                assert!(!delete_after, "plain 'once' must keep the row");
                let parsed = DateTime::parse_from_rfc3339(&at).unwrap();
                assert!(
                    parsed > Utc::now(),
                    "one-shot instant must be in the future"
                );
            }
            _ => panic!("'once' should be a one-shot spec"),
        }
    }

    #[test]
    fn once_delete_sets_delete_after() {
        let spec = build_schedule_spec(None, Some("16:00"), None, Some("once_delete"), None, None)
            .unwrap();
        match spec {
            ScheduleSpec::Once { delete_after, .. } => assert!(delete_after),
            _ => panic!("'once_delete' should be a one-shot spec"),
        }
    }

    #[test]
    fn once_with_explicit_date_targets_that_day() {
        // A specific future date (e.g. a week out) is honoured verbatim.
        let spec = build_schedule_spec(
            None,
            Some("08:30"),
            Some("2030-01-07"),
            Some("once"),
            None,
            None,
        )
        .unwrap();
        match spec {
            ScheduleSpec::Once { at, .. } => {
                let local = DateTime::parse_from_rfc3339(&at)
                    .unwrap()
                    .with_timezone(&Local);
                assert_eq!(local.year(), 2030);
                assert_eq!(local.month(), 1);
                assert_eq!(local.day(), 7);
                assert_eq!(local.hour(), 8);
                assert_eq!(local.minute(), 30);
            }
            _ => panic!("'once' with a date should be a one-shot spec"),
        }
    }

    #[test]
    fn once_rejects_bad_date() {
        assert!(build_schedule_spec(
            None,
            Some("08:30"),
            Some("07/01/2030"),
            Some("once"),
            None,
            None
        )
        .is_err());
    }

    #[test]
    fn once_requires_valid_time() {
        assert!(build_schedule_spec(None, Some("nope"), None, Some("once"), None, None).is_err());
        assert!(build_schedule_spec(None, None, None, Some("once"), None, None).is_err());
    }

    #[test]
    fn advanced_cron_wins_over_once_frequency() {
        // An explicit cron expression takes precedence even if frequency=once.
        match build_schedule_spec(
            Some("*/5 * * * *"),
            Some("16:00"),
            Some("2030-01-07"),
            Some("once"),
            None,
            None,
        )
        .unwrap()
        {
            ScheduleSpec::Cron(c) => assert_eq!(c, "*/5 * * * *"),
            _ => panic!("advanced cron should win"),
        }
    }

    // ── event link safety ───────────────────────────────────────────────────

    #[test]
    fn an_internal_space_app_route_is_accepted() {
        assert_eq!(
            sanitize_event_link("/space/app/study?session=abc").unwrap(),
            "/space/app/study?session=abc"
        );
        assert!(sanitize_event_link("/space/app/luna-calendar").is_ok());
    }

    #[test]
    fn external_urls_are_refused_so_an_event_cannot_be_a_phishing_button() {
        for bad in [
            "https://evil.example/login",
            "http://127.0.0.1:1/",
            "javascript:alert(1)",
            "//evil.example/x",
            "/space/app/../../etc/passwd",
            "\\\\evil.example",
            "/other/route",
            "",
        ] {
            assert!(
                sanitize_event_link(bad).is_err(),
                "must reject event link: {bad:?}"
            );
        }
    }

    #[test]
    fn a_rejected_link_is_an_error_not_a_silently_dropped_field() {
        let err = sanitize_event_link("https://evil.example").unwrap_err();
        assert!(!err.is_empty(), "the caller must be told why");
    }
}

#[cfg(test)]
mod event_link_tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use std::sync::Arc;

    fn server() -> SpaceServer {
        let db = Db::open_in_memory(&Config::from_env()).expect("open db");
        SpaceServer::new(Arc::new(db))
    }

    fn only_event(srv: &SpaceServer) -> serde_json::Value {
        let out = srv.event_list(0, i64::MAX);
        assert!(!out.is_error, "{}", out.content);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&out.content).expect("json");
        assert_eq!(rows.len(), 1, "expected exactly one event");
        rows.into_iter().next().unwrap()
    }

    #[test]
    fn an_event_stores_and_returns_its_space_app_link() {
        let srv = server();
        let out = srv.event_create(
            "Buổi 1/30 · Chương 1".into(),
            1_800_000_000_000,
            1_800_003_600_000,
            None,
            None,
            false,
            None,
            None,
            None,
            Some("/space/app/study?session=abc-123".into()),
            Some("study".into()),
            "default",
            "",
        );
        assert!(!out.is_error, "{}", out.content);

        let ev = only_event(&srv);
        assert_eq!(ev["link"], "/space/app/study?session=abc-123");
        assert_eq!(ev["app_id"], "study");
    }

    #[test]
    fn an_event_without_a_link_still_works_and_reports_null() {
        let srv = server();
        let out = srv.event_create(
            "Họp".into(),
            1_800_000_000_000,
            1_800_003_600_000,
            None,
            None,
            false,
            None,
            None,
            None,
            None,
            None,
            "default",
            "",
        );
        assert!(!out.is_error, "{}", out.content);
        assert!(only_event(&srv)["link"].is_null());
    }

    #[test]
    fn an_external_link_is_refused_and_no_event_is_created() {
        let srv = server();
        let out = srv.event_create(
            "Bẫy".into(),
            1_800_000_000_000,
            1_800_003_600_000,
            None,
            None,
            false,
            None,
            None,
            None,
            Some("https://evil.example/login".into()),
            None,
            "default",
            "",
        );
        assert!(out.is_error, "an external link must not be stored");

        let list = srv.event_list(0, i64::MAX);
        let rows: Vec<serde_json::Value> = serde_json::from_str(&list.content).unwrap();
        assert!(
            rows.is_empty(),
            "a rejected link must not leave a half-made event"
        );
    }

    #[test]
    fn updating_the_link_replaces_it_and_still_validates() {
        let srv = server();
        srv.event_create(
            "Buổi 1".into(),
            1_800_000_000_000,
            1_800_003_600_000,
            None,
            None,
            false,
            None,
            None,
            None,
            Some("/space/app/study?session=one".into()),
            Some("study".into()),
            "default",
            "",
        );
        let id = only_event(&srv)["id"].as_str().unwrap().to_string();

        let ok = srv.event_update(
            id.clone(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("/space/app/study?session=two".into()),
            None,
            false,
        );
        assert!(!ok.is_error, "{}", ok.content);
        assert_eq!(only_event(&srv)["link"], "/space/app/study?session=two");

        let bad = srv.event_update(
            id,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("javascript:alert(1)".into()),
            None,
            false,
        );
        assert!(bad.is_error);
        assert_eq!(
            only_event(&srv)["link"],
            "/space/app/study?session=two",
            "a rejected update must not clear the good link"
        );
    }
}
