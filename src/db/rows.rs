use anyhow::Result;
use rusqlite::Row;

use crate::types::{
    Agent, Binding, BindingWithRelations, Channel, ContextMode, GroupBinding, ScheduleType,
    ScheduledTask, StoredMessage, TaskStatus,
};

use super::helpers::parse_json_array;

pub(crate) fn row_to_channel(row: &Row<'_>) -> Result<Channel> {
    Ok(Channel {
        id: row.get("id")?,
        platform_type: row.get("platform_type")?,
        name: row.get("name")?,
        credentials_json: row.get("credentials_json")?,
        connection_state: row.get("connection_state")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

pub(crate) fn row_to_agent(row: &Row<'_>) -> Result<Agent> {
    Ok(Agent {
        id: row.get("id")?,
        folder: row.get("folder")?,
        name: row.get("name")?,
        requires_trigger: row.get::<_, i64>("requires_trigger")? != 0,
        allowed_tools: parse_json_array(row.get("allowed_tools")?),
        allowed_paths: parse_json_array(row.get("allowed_paths")?),
        allowed_work_dirs: parse_json_array(row.get("allowed_work_dirs")?),
        core_prompt: row.get::<_, String>("core_prompt").unwrap_or_default(),
        model_id: row.get("model_id")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

pub(crate) fn row_to_binding(row: &Row<'_>) -> Result<Binding> {
    Ok(Binding {
        id: row.get("id")?,
        jid: row.get("jid")?,
        agent_id: row.get("agent_id")?,
        channel_id: row.get("channel_id")?,
        bot_token_override: row.get("bot_token_override")?,
        max_messages: row.get::<_, Option<i64>>("max_messages")?.map(|n| n as u32),
        last_active: row.get("last_active")?,
        created_at: row.get("created_at")?,
    })
}

pub(crate) fn row_to_pairing(row: &Row<'_>) -> Result<crate::types::ChannelPairing> {
    Ok(crate::types::ChannelPairing {
        id: row.get("id")?,
        channel_id: row.get("channel_id")?,
        chat_jid: row.get("chat_jid")?,
        chat_type: row.get("chat_type")?,
        sender_jid: row.get("sender_jid")?,
        sender_name: row.get("sender_name")?,
        chat_title: row.get("chat_title")?,
        code: row.get("code")?,
        status: row.get("status")?,
        bot_token: row.get("bot_token")?,
        created_at: row.get("created_at")?,
        expires_at: row.get("expires_at")?,
        resolved_at: row.get("resolved_at")?,
    })
}

pub(crate) fn row_to_binding_with_relations(row: &Row<'_>) -> Result<BindingWithRelations> {
    Ok(BindingWithRelations {
        binding: Binding {
            id: row.get(0)?,
            jid: row.get(1)?,
            agent_id: row.get(2)?,
            channel_id: row.get(3)?,
            // column 4 (`b.is_admin`) is a dead/defaulted DB column — kept in the
            // SELECT for positional stability but no longer mapped to a field.
            bot_token_override: row.get(5)?,
            max_messages: row.get::<_, Option<i64>>(6)?.map(|n| n as u32),
            last_active: row.get(7)?,
            created_at: row.get(8)?,
        },
        agent: Agent {
            id: row.get(9)?,
            folder: row.get(10)?,
            name: row.get(11)?,
            requires_trigger: row.get::<_, i64>(12)? != 0,
            allowed_tools: parse_json_array(row.get(13)?),
            allowed_paths: parse_json_array(row.get(14)?),
            allowed_work_dirs: parse_json_array(row.get(15)?),
            core_prompt: row.get::<_, String>(16).unwrap_or_default(),
            model_id: row.get(17)?,
            created_at: row.get(18)?,
            updated_at: row.get(19)?,
        },
        channel: Channel {
            id: row.get(20)?,
            platform_type: row.get(21)?,
            name: row.get(22)?,
            credentials_json: row.get(23)?,
            connection_state: row.get(24)?,
            created_at: row.get(25)?,
            updated_at: row.get(26)?,
        },
    })
}

pub(crate) fn row_to_group(row: &Row<'_>) -> Result<GroupBinding> {
    Ok(GroupBinding {
        jid: row.get("jid")?,
        folder: row.get("folder")?,
        name: row.get("name")?,
        channel: row.get::<_, Option<String>>("channel")?.unwrap_or_default(),
        group_type: row
            .get::<_, Option<String>>("group_type")?
            .unwrap_or_else(|| "chat".to_string()),
        requires_trigger: row.get::<_, i64>("requires_trigger")? != 0,
        allowed_tools: parse_json_array(row.get("allowed_tools")?),
        allowed_paths: parse_json_array(row.get("allowed_paths")?),
        allowed_work_dirs: parse_json_array(row.get("allowed_work_dirs")?),
        bot_token: row.get("bot_token")?,
        max_messages: row.get::<_, Option<i64>>("max_messages")?.map(|n| n as u32),
        llm_config_id: row.get("llm_config_id")?,
        last_active: row.get("last_active")?,
        added_at: row.get("added_at")?,
    })
}

pub(crate) fn row_to_message(row: &Row<'_>) -> Result<StoredMessage> {
    Ok(StoredMessage {
        message_id: row.get("message_id")?,
        chat_jid: row.get("chat_jid")?,
        sender_jid: row.get("sender_jid")?,
        sender_name: row.get("sender_name")?,
        content: row.get("content")?,
        timestamp: row.get("timestamp")?,
        is_from_me: row.get::<_, i64>("is_from_me")? != 0,
        is_bot_reply: row.get::<_, i64>("is_bot_reply")? != 0,
        reply_to_id: row.get("reply_to_id")?,
        media_type: row.get("media_type")?,
        attachments: row.get("attachments")?,
    })
}

pub(crate) fn row_to_task(row: &Row<'_>) -> Result<ScheduledTask> {
    Ok(ScheduledTask {
        id: row.get("id")?,
        group_folder: row.get("group_folder")?,
        chat_jid: row.get("chat_jid")?,
        prompt: row.get("prompt")?,
        schedule_type: ScheduleType::parse(&row.get::<_, String>("schedule_type")?),
        schedule_value: row.get("schedule_value")?,
        context_mode: ContextMode::parse(&row.get::<_, String>("context_mode")?),
        agent_mode: crate::types::AgentMode::parse(
            &row.get::<_, Option<String>>("agent_mode")?
                .unwrap_or_default(),
        ),
        script_command: row.get("script_path")?,
        watch_json: row.get("watch_json")?,
        next_run: row.get("next_run")?,
        last_run: row.get("last_run")?,
        last_result: row.get("last_result")?,
        status: TaskStatus::parse(&row.get::<_, String>("status")?),
        created_at: row.get("created_at")?,
    })
}

pub(crate) fn row_to_checkpoint(row: &Row<'_>) -> Result<crate::types::ChatCheckpoint> {
    Ok(crate::types::ChatCheckpoint {
        id: row.get("id")?,
        chat_jid: row.get("chat_jid")?,
        sha: row.get("sha")?,
        parent_sha: row.get("parent_sha")?,
        tool_name: row.get("tool_name")?,
        summary: row.get("summary")?,
        workspace: row.get("workspace")?,
        files_changed: row.get("files_changed")?,
        created_at: row.get("created_at")?,
    })
}
