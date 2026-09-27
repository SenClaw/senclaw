//! AgentApi trait implementation for AgentPool.

use anyhow::Result;
use async_trait::async_trait;

use crate::agent::agent_pool::pool::AgentPool;
use crate::types::AgentApi;
use crate::types::GroupBinding;
use crate::types::MessageAttachment;

#[async_trait]
impl AgentApi for AgentPool {
    async fn broadcast_reply(&self, chat_jid: &str, text: &str, bot_token: Option<&str>) {
        AgentPool::broadcast_reply(self, chat_jid, text, bot_token).await
    }

    async fn process_and_wait(&self, jid: &str, group: &GroupBinding, prompt: &str) -> Result<()> {
        self.process_and_wait_inner(jid, group, prompt, 5).await
    }

    async fn process_and_wait_with_attachments(
        &self,
        jid: &str,
        group: &GroupBinding,
        prompt: &str,
        attachments: &[MessageAttachment],
    ) -> Result<()> {
        self.process_and_wait_inner_with_attachments(jid, group, prompt, attachments, 5)
            .await
    }

    async fn destroy(&self, jid: &str) {
        self.destroy_inner(jid).await;
    }

    fn get_last_reply_text(&self, jid: &str) -> Option<String> {
        self.state
            .lock()
            .unwrap()
            .last_dispatch_replies
            .get(jid)
            .cloned()
    }
}
