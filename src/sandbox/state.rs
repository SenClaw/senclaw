//! Shared state for the sandbox REST handlers (nested under `/api/sandbox`).

use crate::sandbox::db::Db;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
}
