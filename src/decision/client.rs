//! HTTP client for the `decision` runtime slot, replacing the old in-process
//! Laya call. This is server-side code — the gate and the skill router call
//! it directly against whatever process the runtime manager has running for
//! `Slot::Decision`, never by looping back through the daemon's own HTTP API.

use crate::runtime::manager::{RuntimeClientError, RuntimeManager};

use super::types::{AskRequest, AskResponse};

/// Ask the decision runtime one request. Builds the wire text through
/// `AskRequest`'s own `Serialize` impl (its `state`/`questions` fields are
/// [`super::json::Json`], which preserves key order end to end — never
/// `serde_json::Value`, whose sorted keys would change what the model reads).
pub async fn ask(manager: &RuntimeManager, request: &AskRequest) -> Result<AskResponse, RuntimeClientError> {
    let text = serde_json::to_string(request).map_err(|e| RuntimeClientError::Internal(e.to_string()))?;
    let body = crate::runtime::clients::decision_ask(manager, text).await?;
    serde_json::from_slice(&body)
        .map_err(|e| RuntimeClientError::Internal(format!("the decision runtime sent an unreadable answer: {e}")))
}
