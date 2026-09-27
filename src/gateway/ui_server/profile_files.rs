//! Profile file editor — read/write SOUL.md and MEMORY.md per agent folder.
//!
//! Pairs with the Settings → Profile UI. Edits trigger a cognitive
//! re-ingest for SOUL.md so the persona embeddings stay in sync.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::gateway::group_manager::{read_memory_md, read_soul_md, write_memory_md, write_soul_md};

use super::core::{AppError, UiState};

#[derive(Debug, Serialize)]
pub(crate) struct ProfileFiles {
    pub folder: String,
    pub soul: String,
    pub memory: String,
}

/// GET /api/agents/:folder/files
pub(crate) async fn get_files(
    State(s): State<Arc<UiState>>,
    Path(folder): Path<String>,
) -> Json<ProfileFiles> {
    Json(ProfileFiles {
        folder: folder.clone(),
        soul: read_soul_md(&s.config, &folder),
        memory: read_memory_md(&s.config, &folder),
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct PutFiles {
    pub soul: Option<String>,
    pub memory: Option<String>,
}

/// PUT /api/agents/:folder/files
///
/// Body: `{ soul?: string, memory?: string }`. Fields that are absent are
/// left untouched on disk. SOUL.md edits enqueue a cognitive re-ingest so
/// the persona embeddings refresh; MEMORY.md is a free-form scratchpad and
/// re-ingest happens via the existing watcher on next agent boot.
pub(crate) async fn put_files(
    State(s): State<Arc<UiState>>,
    Path(folder): Path<String>,
    Json(body): Json<PutFiles>,
) -> Result<Json<ProfileFiles>, AppError> {
    // Validate the agent exists so we don't write to arbitrary folders.
    let db = s
        .db
        .as_ref()
        .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "db not initialized".into()))?;
    let agent = db
        .get_agent_by_folder(&folder)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, format!("unknown agent: {folder}")))?;

    if let Some(soul) = body.soul.as_deref() {
        write_soul_md(&s.config, &folder, &agent.name, soul);
        // Re-ingest persona into cognitive graph (fire-and-forget).
        crate::gateway::agent_manager::spawn_soul_ingest(
            s.config.paths.agents_dir.clone(),
            folder.clone(),
        );
    }
    if let Some(memory) = body.memory.as_deref() {
        write_memory_md(&s.config, &folder, memory);
    }

    Ok(Json(ProfileFiles {
        folder: folder.clone(),
        soul: read_soul_md(&s.config, &folder),
        memory: read_memory_md(&s.config, &folder),
    }))
}
