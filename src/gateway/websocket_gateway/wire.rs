// ===== GroupInfo (wire format, camelCase) =====

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct GroupInfo {
    pub(crate) jid: String,
    pub(crate) folder: String,
    pub(crate) name: String,
    pub(crate) channel: String,
    #[serde(rename = "groupType")]
    pub(crate) group_type: String,
    #[serde(rename = "requiresTrigger")]
    pub(crate) requires_trigger: bool,
    #[serde(rename = "allowedTools")]
    pub(crate) allowed_tools: Option<Vec<String>>,
    #[serde(rename = "allowedPaths")]
    pub(crate) allowed_paths: Option<Vec<String>>,
    #[serde(rename = "allowedWorkDirs")]
    pub(crate) allowed_work_dirs: Option<Vec<String>>,
    #[serde(rename = "maxMessages")]
    pub(crate) max_messages: Option<u32>,
    #[serde(rename = "modelId", skip_serializing_if = "Option::is_none")]
    pub(crate) llm_config_id: Option<String>,
    #[serde(rename = "agentId", skip_serializing_if = "Option::is_none")]
    pub(crate) agent_id: Option<i64>,
    #[serde(rename = "channelId", skip_serializing_if = "Option::is_none")]
    pub(crate) channel_id: Option<i64>,
    /// Last chat message/agent response time (ms since epoch), from
    /// group_messages. Powers the sidebar "recent activity" sort.
    #[serde(rename = "lastActivity", skip_serializing_if = "Option::is_none")]
    pub(crate) last_activity: Option<i64>,
}

pub(crate) fn to_group_info(g: &crate::types::GroupBinding) -> GroupInfo {
    GroupInfo {
        jid: g.jid.clone(),
        folder: g.folder.clone(),
        name: g.name.clone(),
        channel: g.channel.clone(),
        group_type: g.group_type.clone(),
        requires_trigger: g.requires_trigger,
        allowed_tools: g.allowed_tools.clone(),
        allowed_paths: g.allowed_paths.clone(),
        allowed_work_dirs: g.allowed_work_dirs.clone(),
        max_messages: g.max_messages,
        llm_config_id: g.llm_config_id.clone(),
        agent_id: None,
        channel_id: None,
        last_activity: None,
    }
}

// ===== Entity wire format (camelCase) =====

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ChannelInfoWire {
    pub(crate) id: i64,
    #[serde(rename = "platformType")]
    pub(crate) platform_type: String,
    pub(crate) name: String,
    #[serde(rename = "credentialsJson")]
    pub(crate) credentials_json: String,
    #[serde(rename = "connectionState")]
    pub(crate) connection_state: String,
    #[serde(rename = "createdAt")]
    pub(crate) created_at: String,
    #[serde(rename = "updatedAt")]
    pub(crate) updated_at: String,
}

pub(crate) fn to_channel_info(ch: &crate::types::Channel) -> ChannelInfoWire {
    ChannelInfoWire {
        id: ch.id,
        platform_type: ch.platform_type.clone(),
        name: ch.name.clone(),
        credentials_json: ch.credentials_json.clone(),
        connection_state: ch.connection_state.clone(),
        created_at: ch.created_at.clone(),
        updated_at: ch.updated_at.clone(),
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AgentInfoWire {
    pub(crate) id: i64,
    pub(crate) folder: String,
    pub(crate) name: String,
    #[serde(rename = "requiresTrigger")]
    pub(crate) requires_trigger: bool,
    #[serde(rename = "allowedTools")]
    pub(crate) allowed_tools: Option<Vec<String>>,
    #[serde(rename = "allowedWorkDirs")]
    pub(crate) allowed_work_dirs: Option<Vec<String>>,
    #[serde(rename = "corePrompt")]
    pub(crate) core_prompt: String,
    #[serde(rename = "modelId")]
    pub(crate) model_id: Option<String>,
    #[serde(rename = "createdAt")]
    pub(crate) created_at: String,
    #[serde(rename = "updatedAt")]
    pub(crate) updated_at: String,
}

pub(crate) fn to_agent_info(a: &crate::types::Agent) -> AgentInfoWire {
    AgentInfoWire {
        id: a.id,
        folder: a.folder.clone(),
        name: a.name.clone(),
        requires_trigger: a.requires_trigger,
        allowed_tools: a.allowed_tools.clone(),
        allowed_work_dirs: a.allowed_work_dirs.clone(),
        core_prompt: a.core_prompt.clone(),
        model_id: a.model_id.clone(),
        created_at: a.created_at.clone(),
        updated_at: a.updated_at.clone(),
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct BindingWithRelationsWire {
    pub(crate) id: i64,
    pub(crate) jid: Option<String>,
    #[serde(rename = "agentId")]
    pub(crate) agent_id: i64,
    #[serde(rename = "channelId")]
    pub(crate) channel_id: i64,
    #[serde(rename = "botTokenOverride")]
    pub(crate) bot_token_override: Option<String>,
    #[serde(rename = "maxMessages")]
    pub(crate) max_messages: Option<u32>,
    #[serde(rename = "lastActive")]
    pub(crate) last_active: Option<String>,
    #[serde(rename = "createdAt")]
    pub(crate) created_at: String,
    pub(crate) agent: AgentInfoWire,
    pub(crate) channel: ChannelInfoWire,
}

pub(crate) fn to_binding_with_relations(
    br: &crate::types::BindingWithRelations,
) -> BindingWithRelationsWire {
    BindingWithRelationsWire {
        id: br.binding.id,
        jid: br.binding.jid.clone(),
        agent_id: br.binding.agent_id,
        channel_id: br.binding.channel_id,
        bot_token_override: br.binding.bot_token_override.clone(),
        max_messages: br.binding.max_messages,
        last_active: br.binding.last_active.clone(),
        created_at: br.binding.created_at.clone(),
        agent: to_agent_info(&br.agent),
        channel: to_channel_info(&br.channel),
    }
}
