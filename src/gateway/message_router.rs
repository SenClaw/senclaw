//! Message routing core — bridges channels, agent pool, and group management.
//! Mirrors `src-old/gateway/MessageRouter.ts`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::agent::group_queue::GroupQueue;
use crate::config::Config;
use crate::db::Db;
use crate::gateway::binding_manager::BindingManager;
use crate::gateway::command_dispatcher::dispatch_command;
use crate::gateway::group_manager::{ensure_app_group, ensure_wechat_admin_group, GroupManager};
use crate::gateway::plugin_command::dispatch_plugin_command;
use crate::gateway::trigger_checker::should_trigger;
use crate::gateway::websocket_gateway::WebSocketGateway;
use crate::marketplace::manager::MarketplaceManager;
use crate::types::{AgentApi, BindingWithRelations, GroupBinding, IncomingMessage, StoredMessage};

// ===== JID migration callback =====

pub type OnJidMigrated = Arc<dyn Fn(&str, &GroupBinding) + Send + Sync + 'static>;

// ===== MessageRouter =====

pub struct MessageRouter {
    group_manager: Arc<GroupManager>,
    binding_manager: Arc<BindingManager>,
    agent_api: Arc<dyn AgentApi>,
    group_queue: Arc<GroupQueue>,
    db: Arc<Db>,
    config: Arc<Config>,
    wechat_agent_folder: String,
    notified_jids: Mutex<HashSet<String>>,
    /// chat_jid → when its pairing code was last spoken aloud.
    ///
    /// Not "have we ever challenged this chat": a challenge that failed to send
    /// is indistinguishable from one that arrived, and the row stays live for an
    /// hour, so a once-only rule leaves the user in permanent silence holding no
    /// code at all. Re-announcing on a cooldown is the only version that
    /// survives a delivery failure.
    challenge_sent_at: Mutex<HashMap<String, std::time::Instant>>,
    on_jid_migrated: Mutex<Option<OnJidMigrated>>,
    ws_gateway: Mutex<Option<Arc<WebSocketGateway>>>,
    /// Shared with the UI server so `/plugin` chat commands and the marketplace
    /// panel mutate the same state. Wired post-construction via
    /// [`Self::set_marketplace_manager`]; plugin commands are inert until then.
    marketplace_manager: Mutex<Option<Arc<std::sync::Mutex<MarketplaceManager>>>>,
}

impl MessageRouter {
    pub fn new(
        group_manager: Arc<GroupManager>,
        binding_manager: Arc<BindingManager>,
        agent_api: Arc<dyn AgentApi>,
        group_queue: Arc<GroupQueue>,
        db: Arc<Db>,
        config: Arc<Config>,
    ) -> Self {
        Self {
            group_manager,
            binding_manager,
            agent_api,
            group_queue,
            db,
            config,
            wechat_agent_folder: "main".to_string(),
            notified_jids: Mutex::new(HashSet::new()),
            challenge_sent_at: Mutex::new(HashMap::new()),
            on_jid_migrated: Mutex::new(None),
            ws_gateway: Mutex::new(None),
            marketplace_manager: Mutex::new(None),
        }
    }

    pub async fn set_ws_gateway(&self, gw: Arc<WebSocketGateway>) {
        *self.ws_gateway.lock().await = Some(gw);
    }

    /// Wire the shared marketplace manager so `/plugin` chat commands work.
    pub async fn set_marketplace_manager(
        &self,
        manager: Arc<std::sync::Mutex<MarketplaceManager>>,
    ) {
        *self.marketplace_manager.lock().await = Some(manager);
    }

    pub async fn set_on_jid_migrated(&self, cb: OnJidMigrated) {
        let mut guard = self.on_jid_migrated.lock().await;
        *guard = Some(cb);
    }

    /// Resolve a [`GroupBinding`] for the incoming message JID.
    /// Tries the new entity model first (bindings table), then falls back to
    /// the legacy groups table.
    async fn resolve_binding(&self, msg: &IncomingMessage) -> Option<GroupBinding> {
        // 1. Try new entity model via BindingManager.
        if let Ok(Some(br)) = self
            .binding_manager
            .get_with_relations(&self.db, &msg.chat_jid)
        {
            tracing::info!(
                "[MessageRouter] Resolved via entity model: agent={} channel={}",
                br.agent.folder,
                br.channel.name
            );
            return Some(to_group_binding(&br));
        }

        // 2. Fall back to legacy GroupManager.
        self.group_manager.get(&self.db, &msg.chat_jid)
    }

    /// Main entry point — called by channels when a message arrives.
    pub async fn handle_incoming(&self, msg: IncomingMessage) {
        tracing::info!(
            "[MessageRouter] Incoming from {}: \"{}\"",
            msg.chat_jid,
            &msg.content.chars().take(60).collect::<String>(),
        );

        // Ghi vào sổ inbound của egress guard TRƯỚC mọi bước routing: nội dung này không
        // tin cậy kể từ giây nó đến, và nếu nó quay trở ra ở một reply nào đó thì
        // `security::egress` phải có bản gốc để đối chiếu. Ghi cả tin không thuộc group
        // nào — worm không cần binding hợp lệ mới lây được.
        crate::security::record_inbound(&msg.chat_jid, &msg.content);

        // 1. Find registered group binding (entity model first, then legacy)
        let mut group = self.resolve_binding(&msg).await;

        if group.is_none() {
            if msg.chat_jid.starts_with("wx:") {
                if msg.bot_token.is_some() {
                    group = self.complete_pending_wechat_binding(&msg).await;
                }
                if group.is_none() {
                    ensure_wechat_admin_group(
                        &self.db,
                        &self.group_manager,
                        &self.config,
                        &msg.chat_jid,
                        &self.wechat_agent_folder,
                    );
                    group = self.group_manager.get(&self.db, &msg.chat_jid);
                }
            }
            if group.is_none() && msg.chat_jid.starts_with("tg:") {
                group = self.challenge_unbound_telegram(&msg).await;
            }
            if group.is_none() && msg.chat_jid.starts_with("feishu:") {
                group = self.complete_pending_feishu_binding(&msg).await;
            }
            if group.is_none() && msg.chat_jid.starts_with("app:") {
                ensure_app_group(&self.db, &self.group_manager, &self.config, &msg.chat_jid);
                group = self.group_manager.get(&self.db, &msg.chat_jid);
            }
            if group.is_none() {
                tracing::info!(
                    "[MessageRouter] No registered group for {}, ignoring",
                    msg.chat_jid
                );
                self.notify_unregistered_feishu(&msg).await;
                return;
            }
        }

        let group = group.unwrap();

        // 2. Persist message
        self.store_message(&msg);

        // 2b. Notify WebSocket clients of the incoming message (real-time update).
        if let Some(gw) = self.ws_gateway.lock().await.clone() {
            gw.notify_incoming(&msg).await;
        }

        // 3. Trigger check
        if !should_trigger(&msg, &group) {
            tracing::info!("[MessageRouter] Trigger check failed for {}", msg.chat_jid);
            return;
        }

        // 4. Command interception — every chat has full (admin) privileges now,
        // so slash-commands are honored in all groups, not just a "main" one.

        // 4a. `/plugin ...` marketplace commands (async: git/HTTP under the hood).
        if let Some(manager) = self.marketplace_manager.lock().await.clone() {
            if let Some(result) = dispatch_plugin_command(manager, &msg.content).await {
                tracing::info!(
                    "[MessageRouter] Plugin command handled for {}",
                    msg.chat_jid
                );
                self.agent_api
                    .broadcast_reply(&msg.chat_jid, &result, group.bot_token.as_deref())
                    .await;
                return;
            }
        }

        // 4a-bis. `/app ...` Space App update commands (self-HTTP to the daemon).
        if let Some(result) =
            crate::gateway::plugin_command::dispatch_app_command(&msg.content).await
        {
            tracing::info!("[MessageRouter] App command handled for {}", msg.chat_jid);
            self.agent_api
                .broadcast_reply(&msg.chat_jid, &result, group.bot_token.as_deref())
                .await;
            return;
        }

        if let Some(result) = dispatch_command(&self.db, &msg.content, Some(&msg.chat_jid)) {
            tracing::info!("[MessageRouter] Command handled for {}", msg.chat_jid);
            self.agent_api
                .broadcast_reply(&msg.chat_jid, &result, group.bot_token.as_deref())
                .await;
            return;
        }

        tracing::info!("[MessageRouter] Triggering agent for {}", msg.chat_jid);

        // 5. Update last-active
        self.group_manager
            .touch_active(&self.db, &msg.chat_jid, &chrono_now());

        // 6. Build prompt and enqueue
        let agent_api = Arc::clone(&self.agent_api);
        let db = Arc::clone(&self.db);
        let jid = msg.chat_jid.clone();
        let g = group.clone();

        let jid_key = jid.clone();
        self.group_queue
            .enqueue(
                &jid_key,
                Box::pin(async move {
                    run_agent(agent_api, db, jid, g).await;
                }),
            )
            .await;
    }

    /// Dispatch a task directly (bypasses trigger/command checks).
    pub async fn dispatch_task(
        &self,
        jid: &str,
        prompt: &str,
        callbacks: Option<DispatchTaskCallbacks>,
    ) {
        let Some(group) = self.group_manager.get(&self.db, jid) else {
            tracing::warn!("[MessageRouter] dispatchTask: no group for {jid}");
            return;
        };
        self.group_manager
            .touch_active(&self.db, jid, &chrono_now());

        let agent_api = Arc::clone(&self.agent_api);
        let jid_owned = jid.to_string();
        let g = group.clone();
        let p = prompt.to_string();

        let jid_key = jid_owned.clone();
        self.group_queue
            .enqueue(
                &jid_key,
                Box::pin(async move {
                    if let Some(ref cb) = callbacks {
                        (cb.on_started)();
                    }
                    if let Err(e) = agent_api.process_and_wait(&jid_owned, &g, &p).await {
                        tracing::error!(
                            "[MessageRouter] dispatchTask agent error for {jid_owned}: {e:#}"
                        );
                    }
                    if let Some(ref cb) = callbacks {
                        (cb.on_completed)();
                    }
                }),
            )
            .await;
    }

    // ===== Internal =====

    async fn complete_pending_binding(
        &self,
        msg: &IncomingMessage,
        pending: Option<GroupBinding>,
    ) -> Option<GroupBinding> {
        let pending = pending?;
        let old_jid = pending.jid.clone();
        let new_binding = self.group_manager.migrate_jid(
            &self.db,
            &self.config.paths.global_config_path,
            &old_jid,
            &msg.chat_jid,
        )?;
        tracing::info!(
            "[MessageRouter] Pending binding completed: {old_jid} → {}",
            msg.chat_jid
        );
        self.agent_api.destroy(&old_jid).await;
        let guard = self.on_jid_migrated.lock().await;
        if let Some(ref cb) = *guard {
            cb(&old_jid, &new_binding);
        }
        Some(new_binding)
    }

    /// For Telegram: an unknown chat gets a pairing code, not a binding.
    ///
    /// This used to complete the channel's pending binding on sight, which made
    /// the bot answer to whoever messaged it first — with all tools, since a
    /// UI-created agent has no `allowed_tools` whitelist. Now the chat is
    /// recorded as a request, told its code, and left unbound until a human
    /// approves it in Settings → Channels → Pairing.
    ///
    /// Always returns `None`: nothing this function sees can grant access.
    async fn challenge_unbound_telegram(&self, msg: &IncomingMessage) -> Option<GroupBinding> {
        let bot_token = msg.bot_token.as_deref().unwrap_or("");
        let Some(channel) = crate::gateway::pairing::resolve_telegram_channel(
            &self.db,
            bot_token,
            &self.config.telegram.bot_token,
        ) else {
            tracing::info!(
                "[MessageRouter] Telegram message for {} arrived on a bot no channel owns, ignoring",
                msg.chat_jid
            );
            return None;
        };

        match crate::gateway::pairing::request(&self.db, &channel, msg) {
            // Announce a fresh code immediately; otherwise re-announce the
            // live one on a cooldown.
            //
            // Announcing only on `is_new` was wrong in the one case that
            // matters: if the challenge fails to send, the row still sits there
            // live for an hour, so every later message re-serves it with
            // `is_new == false` and the bot stays mute — the user is left with
            // no code and no way to ask for one. The send is fire-and-forget, so
            // the router cannot know it failed; re-announcing on a cooldown
            // recovers without needing to know.
            Ok(req) if req.is_new || self.challenge_cooled_down(&msg.chat_jid).await => {
                self.stamp_challenge(&msg.chat_jid).await;
                tracing::info!(
                    "[MessageRouter] Pairing requested: {} on channel '{}' (code {})",
                    msg.chat_jid,
                    channel.name,
                    req.pairing.code
                );
                // A channel nobody attached an agent to can hand out codes that
                // can never be approved — `pairing::approve` refuses with
                // "channel chưa gắn agent" only once someone clicks. Say it now,
                // while the operator is still looking at why nothing happened.
                if self
                    .db
                    .list_bindings_for_channel(channel.id)
                    .map(|b| b.is_empty())
                    .unwrap_or(false)
                {
                    tracing::warn!(
                        "[MessageRouter] Channel '{}' chưa gắn agent nào — mã {} sẽ không duyệt được. \
                         Tạo agent cho channel này ở Settings → Agents → Add Agent.",
                        channel.name,
                        req.pairing.code
                    );
                }
                self.agent_api
                    .broadcast_reply(
                        &msg.chat_jid,
                        &crate::gateway::pairing::render_challenge(&req.pairing),
                        msg.bot_token.as_deref(),
                    )
                    .await;
                if let Some(gw) = self.ws_gateway.lock().await.clone() {
                    gw.notify_pairing_requested(&req.pairing).await;
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("[MessageRouter] Could not record pairing request: {e}"),
        }
        None
    }

    /// Has this chat's code been quiet long enough to say again?
    ///
    /// Read-only, and paired with [`Self::stamp_challenge`] rather than
    /// stamping itself: the announce condition is `is_new || cooled_down`, and
    /// `||` short-circuits — a self-stamping check would never run on a freshly
    /// minted code, leaving no timestamp and letting the very next message
    /// repeat it. The window bounds a chat that keeps typing to one reply a
    /// minute while still letting somebody who never received the first one ask
    /// again.
    async fn challenge_cooled_down(&self, chat_jid: &str) -> bool {
        let seen = self.challenge_sent_at.lock().await;
        challenge_cooled_down_at(
            seen.get(chat_jid).copied(),
            std::time::Instant::now(),
            CHALLENGE_COOLDOWN,
        )
    }

    /// Record that the code was just spoken, whichever branch decided to.
    async fn stamp_challenge(&self, chat_jid: &str) {
        self.challenge_sent_at
            .lock()
            .await
            .insert(chat_jid.to_string(), std::time::Instant::now());
    }

    async fn complete_pending_feishu_binding(&self, msg: &IncomingMessage) -> Option<GroupBinding> {
        let app_id = msg.bot_token.as_deref().unwrap_or("");
        if app_id.is_empty() {
            return None;
        }
        let pending = self
            .group_manager
            .find_pending_feishu_binding(&self.db, app_id);
        self.complete_pending_binding(msg, pending).await
    }

    async fn complete_pending_wechat_binding(&self, msg: &IncomingMessage) -> Option<GroupBinding> {
        let folder = msg.bot_token.as_deref().unwrap_or("");
        if folder.is_empty() {
            return None;
        }
        let pending = self
            .group_manager
            .find_pending_wechat_binding(&self.db, folder);
        self.complete_pending_binding(msg, pending).await
    }

    async fn notify_unregistered_feishu(&self, msg: &IncomingMessage) {
        if !msg.chat_jid.starts_with("feishu:") {
            return;
        }
        {
            let mut jids = self.notified_jids.lock().await;
            if !jids.insert(msg.chat_jid.clone()) {
                return;
            }
        }
        let text = format!(
            "👋 Hello!\n\nThis conversation is not bound to SenClaw yet.\n\n\
             Your JID is: `{}`\n\n\
             Please add an Agent in the Web admin UI and paste the JID above into the Chat JID field.",
            msg.chat_jid
        );
        self.agent_api
            .broadcast_reply(&msg.chat_jid, &text, msg.bot_token.as_deref())
            .await;
    }

    fn store_message(&self, msg: &IncomingMessage) {
        // Attachments ride the stored row: `run_agent` rebuilds the turn from
        // history, so media dropped here can never reach the agent.
        let attachments = if msg.attachments.is_empty() {
            None
        } else {
            serde_json::to_string(&msg.attachments).ok()
        };
        let stored = StoredMessage {
            message_id: msg.id.clone(),
            chat_jid: msg.chat_jid.clone(),
            sender_jid: msg.sender_jid.clone(),
            sender_name: msg.sender_name.clone(),
            content: msg.content.clone(),
            timestamp: msg.timestamp.clone(),
            is_from_me: msg.is_from_me,
            is_bot_reply: false,
            reply_to_id: None,
            media_type: msg.attachments.first().map(|a| a.mime_type.clone()),
            attachments,
        };
        let limit = self.config.agent.max_messages_per_group;
        // Raw platform message log
        if let Err(e) = self.db.insert_message(&stored, limit) {
            tracing::error!(
                "[MessageRouter] Failed to store channel message {}: {e:#}",
                msg.id
            );
        }
        // Conversation history
        if let Err(e) = self.db.insert_group_message(&stored, limit) {
            tracing::error!(
                "[MessageRouter] Failed to store group message {}: {e:#}",
                msg.id
            );
        }
    }
}

// ===== Dispatch task callbacks =====

pub struct DispatchTaskCallbacks {
    pub on_started: Box<dyn Fn() + Send + 'static>,
    pub on_completed: Box<dyn Fn() + Send + 'static>,
}

// ===== Standalone agent runner =====

async fn run_agent(agent_api: Arc<dyn AgentApi>, db: Arc<Db>, jid: String, group: GroupBinding) {
    let prompt_built_at = chrono_now();
    let built = crate::agent::session_bridge::build_group_prompt(&db, &jid);

    if built.prompt.is_empty() {
        tracing::warn!("[MessageRouter] Empty prompt for {jid}, skipping");
        return;
    }

    let cursor = match built.last_timestamp {
        Some(ref last_ts) if last_ts.as_str() > prompt_built_at.as_str() => Some(last_ts.clone()),
        _ => Some(prompt_built_at),
    };

    if !built.attachments.is_empty() {
        tracing::info!(
            "[MessageRouter] {jid} turn carries {} attachment(s) from channel messages",
            built.attachments.len()
        );
    }
    if let Err(e) = agent_api
        .process_and_wait_with_attachments(&jid, &group, &built.prompt, &built.attachments)
        .await
    {
        tracing::error!("[MessageRouter] Agent error for {jid}: {e:#}");
    }

    if let Some(ts) = cursor {
        let _ = db.set_last_agent_timestamp(&jid, &ts);
    }
}

/// Synthesize a legacy [`GroupBinding`] from the new entity model so the
/// existing AgentPool / trigger-checker / dispatch paths work without changes.
fn to_group_binding(br: &BindingWithRelations) -> GroupBinding {
    GroupBinding {
        jid: br.binding.jid.clone().unwrap_or_default(),
        folder: br.agent.folder.clone(),
        name: br.agent.name.clone(),
        channel: br.channel.platform_type.clone(),
        group_type: "chat".into(),
        requires_trigger: br.agent.requires_trigger,
        allowed_tools: br.agent.allowed_tools.clone(),
        allowed_paths: br.agent.allowed_paths.clone(),
        allowed_work_dirs: br.agent.allowed_work_dirs.clone(),
        bot_token: br.binding.bot_token_override.clone(),
        max_messages: br.binding.max_messages,
        llm_config_id: None,
        last_active: br.binding.last_active.clone(),
        added_at: br.binding.created_at.clone(),
    }
}

// ===== Helpers =====

/// How long a chat waits before its pairing code is repeated.
const CHALLENGE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);

/// The cooldown decision, free of the router's state so it can be tested.
///
/// `None` (never announced) must answer `true`: that is both the first message
/// and — after a restart — a chat whose only challenge failed to send.
fn challenge_cooled_down_at(
    last_sent: Option<std::time::Instant>,
    now: std::time::Instant,
    cooldown: std::time::Duration,
) -> bool {
    match last_sent {
        Some(prev) => now.duration_since(prev) >= cooldown,
        None => true,
    }
}

fn chrono_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format_iso(now.as_secs())
}

fn format_iso(secs: u64) -> String {
    let days = secs / 86400;
    let tod = secs % 86400;
    let h = tod / 3600;
    let m = (tod % 3600) / 60;
    let s = tod % 60;
    let (y, mo, d) = days_to_ymd(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.000Z")
}

fn days_to_ymd(mut days: i64) -> (i64, u32, u32) {
    days += 719468;
    let era = if days >= 0 { days } else { days - 146096 } / 146097;
    let doe = (days - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn a_chat_never_challenged_is_told_its_code() {
        assert!(challenge_cooled_down_at(None, Instant::now(), CHALLENGE_COOLDOWN));
    }

    #[test]
    fn a_chat_that_keeps_typing_is_not_answered_every_time() {
        let now = Instant::now();
        let just_now = now - Duration::from_secs(5);
        assert!(!challenge_cooled_down_at(
            Some(just_now),
            now,
            CHALLENGE_COOLDOWN
        ));
    }

    #[test]
    fn a_code_that_never_arrived_can_be_asked_for_again() {
        // The case this whole mechanism exists for: the challenge failed to
        // send, the pairing row stays live for an hour, so every later message
        // re-serves it with `is_new == false`. Without the cooldown branch the
        // bot would stay mute and the user would hold no code at all.
        let now = Instant::now();
        let long_ago = now - Duration::from_secs(90);
        assert!(challenge_cooled_down_at(
            Some(long_ago),
            now,
            CHALLENGE_COOLDOWN
        ));
    }

    #[test]
    fn the_boundary_counts_as_cooled_down() {
        let now = Instant::now();
        let exactly = now - CHALLENGE_COOLDOWN;
        assert!(challenge_cooled_down_at(
            Some(exactly),
            now,
            CHALLENGE_COOLDOWN
        ));
    }
}
