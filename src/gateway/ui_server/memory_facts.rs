//! `/api/memory/facts` — see and forget what the agent remembers.
//!
//! Facts are extracted from conversations automatically, so the person has to
//! be able to look at them and take one away: a wrong fact, or one they did not
//! want kept. Both calls name the `bank` (the agent folder); a fact of another
//! bank is never listed or deleted.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use std::sync::Arc;

use super::core::{AppError, UiState};
use crate::memory::cognitive::retrievers::build_fts_match;
use crate::memory::engine::facts::{self, StoredFact};

const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 200;

fn engine() -> Result<Arc<facts::FactEngine>, AppError> {
    facts::instance().ok_or_else(|| {
        AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            "fact_store_not_started".into(),
        )
    })
}

fn internal(e: anyhow::Error) -> AppError {
    AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn wire(f: &StoredFact) -> serde_json::Value {
    serde_json::json!({
        "id": f.id,
        "kind": f.kind.as_str(),
        "text": f.text,
        "occurredStart": f.occurred_start,
        "occurredEnd": f.occurred_end,
        "mentionedAt": f.mentioned_at,
        "entities": f.entities,
    })
}

#[derive(serde::Deserialize)]
pub(crate) struct ListQuery {
    /// The agent folder.
    bank: String,
    /// Keyword search; omitted → newest first.
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

/// GET /api/memory/facts?bank=&q=&limit= — facts of one agent, newest first or
/// best keyword matches.
pub(crate) async fn facts_list(
    State(_): State<Arc<UiState>>,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let engine = engine()?;
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize;
    let found = match q.q.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(text) => match build_fts_match(text) {
            Some(m) => engine.store.search_keyword(&q.bank, &m, None, limit),
            None => Ok(Vec::new()),
        },
        None => engine.store.recent(&q.bank, limit),
    }
    .map_err(internal)?;
    Ok(Json(serde_json::json!({
        "bank": q.bank,
        "total": engine.store.count(&q.bank).map_err(internal)?,
        "count": found.len(),
        "facts": found.iter().map(wire).collect::<Vec<_>>(),
    })))
}

#[derive(serde::Deserialize)]
pub(crate) struct ForgetQuery {
    bank: String,
}

/// DELETE /api/memory/facts/:id?bank= — forget one fact.
pub(crate) async fn fact_forget(
    State(_): State<Arc<UiState>>,
    Path(id): Path<String>,
    Query(q): Query<ForgetQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let deleted = engine()?
        .store
        .delete_in_bank(&q.bank, &id)
        .map_err(internal)?;
    if !deleted {
        return Err(AppError(StatusCode::NOT_FOUND, "no such fact".into()));
    }
    Ok(Json(serde_json::json!({ "deleted": id })))
}
