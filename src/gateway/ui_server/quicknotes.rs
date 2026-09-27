use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::Json};
use serde::Deserialize;

use super::core::{AppError, UiState};

// ===== /api/quicknotes =====

#[derive(Deserialize)]
pub(crate) struct QuicknoteBody {
    text: String,
}

pub(crate) async fn quicknotes_save(
    State(_s): State<Arc<UiState>>,
    Json(body): Json<QuicknoteBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    // Derive filename from H1 → H2 → timestamp
    let raw_title = body
        .text
        .lines()
        .find(|l| l.starts_with("# ") && !l.starts_with("## "))
        .or_else(|| body.text.lines().find(|l| l.starts_with("## ")))
        .map(|l| l.trim_start_matches('#').trim().to_string())
        .unwrap_or_else(|| {
            let now = chrono::Local::now();
            now.format("%Y-%m-%d-%H-%M-%S").to_string()
        });

    let safe = raw_title
        .chars()
        .filter(|c| {
            !matches!(
                c,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '\x00'..='\x1f'
            )
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .take(60)
        .collect::<String>();

    let safe = if safe.is_empty() {
        "quicknote".to_string()
    } else {
        safe
    };

    let dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("senclaw")
        .join("quicknotes");
    fs::create_dir_all(&dir)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Resolve filename conflicts
    let mut filename = format!("{safe}.md");
    let mut filepath = dir.join(&filename);
    let mut counter = 1u32;
    while filepath.exists() {
        filename = format!("{safe}-{counter}.md");
        filepath = dir.join(&filename);
        counter += 1;
    }

    fs::write(&filepath, &body.text)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(serde_json::json!({ "filename": filename })))
}
