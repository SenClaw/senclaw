use app_space_sdk::SpaceClient;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::kanban::db::{GenCard, GenColumn};

/// One turn in the chat panel.
#[derive(Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Deserialize)]
pub struct ChatBody {
    pub messages: Vec<ChatMessage>,
    /// Optional: a text outline of the current board, so answers are grounded in it.
    #[serde(default)]
    pub board_outline: Option<String>,
}

const CHAT_SYSTEM: &str = "You are SenClaw Kanban, an AI project assistant embedded in a \
Kanban board app. Help the user plan work, break goals into tasks, prioritize, and reason \
about the board they are managing. When the current board's outline is provided, ground your \
answer in it and suggest concrete columns, cards, priorities, or next actions. Be concise and \
use markdown (bullet lists, short headers). If the user asks you to build or expand the board, \
tell them to use the ✨ AI buttons so the cards are created directly. Reply in the same \
language as the user's message.";

/// Conversational chat about the current board. Returns (answer, model).
pub async fn chat(body: &ChatBody) -> Result<(String, String), String> {
    let mut prompt = String::new();
    if let Some(outline) = &body.board_outline {
        if !outline.trim().is_empty() {
            prompt.push_str("Current board outline:\n");
            prompt.push_str(outline);
            prompt.push_str("\n\n");
        }
    }
    prompt.push_str("Conversation:\n");
    for m in &body.messages {
        let who = match m.role.as_str() {
            "assistant" => "Assistant",
            "system" => "System",
            _ => "User",
        };
        prompt.push_str(&format!("{who}: {}\n", m.content));
    }
    prompt.push_str("Assistant:");
    bridge_llm(CHAT_SYSTEM, &prompt, 1200).await
}

const BOARD_SYSTEM: &str = "You are a Kanban board planner. Given a project goal (and optional \
extra instructions), design a practical board: a set of workflow columns and the task cards that \
populate them. Return ONLY valid JSON, no prose and no markdown fences, in exactly this shape:\n\
{\"columns\":[{\"title\":\"Todo\",\"role\":\"todo\",\"cards\":[{\"title\":\"Task\",\"description\":\"\",\"priority\":\"medium\",\"labels\":[\"tag\"]}]}]}\n\
Rules: model the real workflow as columns. Prefer the standard stages Triage → Todo → Ready → \
In Progress → Blocked → Done, tagging each column's 'role' with one of \
triage|todo|ready|in_progress|blocked|done|custom (use 'custom' for a domain-specific extra stage). \
Each card 'title' is a short, actionable task (imperative, 3-8 words); 'description' is one short \
sentence of detail (may be empty). 'priority' is one of low|medium|high|urgent. 'labels' is an \
optional short array of tags. Put most cards in the Todo/Triage columns; leave In Progress / Blocked / \
Done empty. Aim for 8-16 cards total. Write in the same language as the goal/instruction.";

const BREAKDOWN_SYSTEM: &str = "You break a single task card down into smaller, concrete subtask \
cards. Given the card title (and its description / board context), produce the actionable steps \
needed to complete it. Return ONLY valid JSON, no prose and no markdown fences, in exactly this \
shape:\n\
{\"cards\":[{\"title\":\"Subtask\",\"description\":\"\",\"priority\":\"medium\"}]}\n\
Rules: 4-8 subtasks, each 'title' a short imperative step (3-8 words), 'description' optional. \
'priority' is one of low|medium|high|urgent. Do NOT repeat the parent task verbatim. Write in the \
same language as the card.";

/// A generated board parsed from the LLM.
pub struct GeneratedBoard {
    pub columns: Vec<GenColumn>,
    pub model: String,
}

#[derive(Deserialize)]
struct BoardRoot {
    #[serde(default)]
    columns: Vec<GenColumn>,
}

#[derive(Deserialize)]
struct CardsRoot {
    #[serde(default)]
    cards: Vec<GenCard>,
}

/// How much of a project's context a prompt carries.
const PROJECT_CONTEXT_MAX_CHARS: usize = 2_000;

/// The project's context for a prompt: the person's own description of the
/// work, cut to [`PROJECT_CONTEXT_MAX_CHARS`]. Empty when there is none, which
/// leaves the prompt exactly as it was.
fn project_section(context: Option<&str>) -> String {
    match context.map(str::trim).filter(|c| !c.is_empty()) {
        Some(c) => format!(
            "About the project this board belongs to (written by the user):\n{}\n\n",
            truncate(c, PROJECT_CONTEXT_MAX_CHARS)
        ),
        None => String::new(),
    }
}

/// The user prompt for a goal: the project (if any), the goal, any extra
/// instruction. Shared by the board and the cards-only generators.
fn goal_prompt(goal: &str, instruction: Option<&str>, project_context: Option<&str>) -> String {
    let mut prompt = format!("{}Project goal: {goal}\n", project_section(project_context));
    if let Some(i) = instruction {
        if !i.trim().is_empty() {
            prompt.push_str(&format!("Extra instruction: {i}\n"));
        }
    }
    prompt.push_str("\nReturn the JSON now.");
    prompt
}

/// Ask the LLM to design a whole board (columns + cards) for `goal`.
pub async fn generate_board(
    goal: &str,
    instruction: Option<&str>,
    project_context: Option<&str>,
) -> Result<GeneratedBoard, String> {
    let prompt = goal_prompt(goal, instruction, project_context);
    let (text, model) = bridge_llm(BOARD_SYSTEM, &prompt, 3000).await?;
    let root: BoardRoot = parse_json(&text).ok_or_else(|| {
        format!(
            "could not parse board JSON from model output:\n{}",
            truncate(&text, 400)
        )
    })?;
    if root.columns.is_empty() {
        return Err("model returned an empty board".into());
    }
    Ok(GeneratedBoard {
        columns: root.columns,
        model,
    })
}

/// A generated card list parsed from the LLM.
pub struct GeneratedCards {
    pub cards: Vec<GenCard>,
    pub model: String,
}

const CARDS_SYSTEM: &str = "You are a Kanban task planner. Given a project goal (and optional \
extra instructions), produce the task cards to reach it — the board's columns already exist, so \
generate ONLY the tasks. Return ONLY valid JSON, no prose and no markdown fences, in exactly this \
shape:\n\
{\"cards\":[{\"title\":\"Task\",\"description\":\"\",\"priority\":\"medium\",\"labels\":[\"tag\"]}]}\n\
Rules: each 'title' is a short, actionable task (imperative, 3-8 words); 'description' is one \
short sentence (may be empty); 'priority' is one of low|medium|high|urgent; 'labels' is an \
optional short array of tags. Aim for 8-16 cards. Write in the same language as the goal.";

/// Ask the LLM for the task cards only (used when the board's columns come from
/// a template instead of being AI-generated).
pub async fn generate_cards(
    goal: &str,
    instruction: Option<&str>,
    project_context: Option<&str>,
) -> Result<GeneratedCards, String> {
    let prompt = goal_prompt(goal, instruction, project_context);
    let (text, model) = bridge_llm(CARDS_SYSTEM, &prompt, 2600).await?;
    let root: CardsRoot = parse_json(&text).ok_or_else(|| {
        format!(
            "could not parse cards JSON from model output:\n{}",
            truncate(&text, 400)
        )
    })?;
    if root.cards.is_empty() {
        return Err("model returned no cards".into());
    }
    Ok(GeneratedCards {
        cards: root.cards,
        model,
    })
}

/// The user prompt for breaking a card down.
fn breakdown_prompt(
    card_title: &str,
    card_desc: &str,
    board_outline: Option<&str>,
    instruction: Option<&str>,
    project_context: Option<&str>,
) -> String {
    let mut prompt = format!(
        "{}Task card: {card_title}\n",
        project_section(project_context)
    );
    if !card_desc.trim().is_empty() {
        prompt.push_str(&format!("Description: {card_desc}\n"));
    }
    if let Some(o) = board_outline {
        if !o.trim().is_empty() {
            prompt.push_str(&format!("\nBoard context:\n{}\n", truncate(o, 1500)));
        }
    }
    if let Some(i) = instruction {
        if !i.trim().is_empty() {
            prompt.push_str(&format!("Extra instruction: {i}\n"));
        }
    }
    prompt.push_str("\nReturn the JSON now.");
    prompt
}

/// Ask the LLM to break `card_title` into subtask cards.
pub async fn breakdown_card(
    card_title: &str,
    card_desc: &str,
    board_outline: Option<&str>,
    instruction: Option<&str>,
    project_context: Option<&str>,
) -> Result<GeneratedCards, String> {
    let prompt = breakdown_prompt(
        card_title,
        card_desc,
        board_outline,
        instruction,
        project_context,
    );

    let (text, model) = bridge_llm(BREAKDOWN_SYSTEM, &prompt, 1600).await?;
    let root: CardsRoot = parse_json(&text).ok_or_else(|| {
        format!(
            "could not parse subtask JSON from model output:\n{}",
            truncate(&text, 400)
        )
    })?;
    if root.cards.is_empty() {
        return Err("model returned no subtasks".into());
    }
    Ok(GeneratedCards {
        cards: root.cards,
        model,
    })
}

/// Parse a `T` out of possibly-fenced / chatty / truncated model output.
fn parse_json<T: for<'de> Deserialize<'de>>(text: &str) -> Option<T> {
    if let Ok(r) = serde_json::from_str::<T>(text.trim()) {
        return Some(r);
    }
    let cleaned = strip_fences(text);
    if let Ok(r) = serde_json::from_str::<T>(cleaned.trim()) {
        return Some(r);
    }
    if let Some(block) = first_json_object(&cleaned) {
        if let Ok(r) = serde_json::from_str::<T>(&block) {
            return Some(r);
        }
    }
    let repaired = repair_truncated_json(&cleaned)?;
    serde_json::from_str::<T>(&repaired).ok()
}

/// Salvage a truncated JSON object: cut back to the last complete `}` outside a
/// string and append closers for any still-open brackets.
fn repair_truncated_json(text: &str) -> Option<String> {
    let start = text.find(|c| c == '{' || c == '[')?;
    let s = &text[start..];
    let bytes = s.as_bytes();
    let mut in_str = false;
    let mut esc = false;
    let mut last_close: Option<usize> = None;
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'}' => last_close = Some(i),
            _ => {}
        }
    }
    let end = last_close?;
    let head = &s[..=end];
    let mut stack: Vec<u8> = Vec::new();
    let mut in_str = false;
    let mut esc = false;
    for &b in head.as_bytes() {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => stack.push(b'}'),
            b'[' => stack.push(b']'),
            b'}' | b']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    let mut out = head.to_string();
    while let Some(closer) = stack.pop() {
        out.push(closer as char);
    }
    Some(out)
}

fn strip_fences(text: &str) -> String {
    let t = text.trim();
    if let Some(rest) = t.strip_prefix("```") {
        let rest = rest.splitn(2, '\n').nth(1).unwrap_or(rest);
        return rest.trim_end_matches("```").to_string();
    }
    t.to_string()
}

/// Return the first balanced `{...}` (or `[...]`) substring.
fn first_json_object(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{' || b == b'[')?;
    let open = bytes[start];
    let close = if open == b'{' { b'}' } else { b']' };
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            x if x == open => depth += 1,
            x if x == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

/// The daemon's configured LLMs via the SDK → { activeId, configs:[…] }.
pub async fn list_models() -> Result<Value, String> {
    let (active, configs) = client().list_models().await.map_err(|e| e.to_string())?;
    let configs: Vec<Value> = configs
        .into_iter()
        .map(|m| json!({ "id": m.id, "modelName": m.model_name, "provider": m.provider }))
        .collect();
    Ok(json!({ "activeId": active, "configs": configs }))
}

pub async fn set_active_model(id: &str) -> Result<(), String> {
    client()
        .set_active_model(id)
        .await
        .map_err(|e| e.to_string())
}

/// The Space-App id the board's model calls are made under
/// (`/api/space/apps/kanban/bridge`).
pub const APP_ID: &str = "kanban";

/// The board's Space-App access token, published once by `run_daemon`.
///
/// A process-global for the reason `util::internal_auth` keeps the daemon token
/// in one: the callers are free functions several layers below anything that
/// holds the daemon's database. Under `SENCLAW_APP_TOKEN_MODE=strict` the
/// bridge refuses a tokenless call from anything that is not the daemon's web
/// UI, and the board is not the web UI.
static APP_TOKEN: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The board's token from the daemon's own token table, minted on first use.
/// The board is not an installed Space App, so no launcher mints it.
pub fn provision_app_token(db: &crate::db::Db) -> anyhow::Result<String> {
    crate::apps::token::ensure(db, APP_ID)
}

/// Publish the token for this process's own model calls. Idempotent.
pub fn set_app_token(token: &str) {
    let t = token.trim();
    if !t.is_empty() {
        let _ = APP_TOKEN.set(t.to_string());
    }
}

/// The token to present: the one the daemon published in this process, else
/// the one a parent put in `SENCLAW_TOKEN_ACCESS_APP` (a `kanban-server`
/// child — see [`child_env`]).
fn app_token() -> Option<String> {
    APP_TOKEN
        .get()
        .cloned()
        .or_else(app_space_sdk::bridge::app_token_from_env)
}

/// Environment for a `kanban-server` child, so its own model calls (the
/// `kanban_generate_board` / `kanban_breakdown_card` tools) reach this daemon
/// and present the board's token. Without it the child inherits nothing but
/// the daemon's environment, which names no port and no token.
pub fn child_env(ui_port: u16) -> HashMap<String, String> {
    let mut env = HashMap::from([(
        "SENCLAW_BASE_URL".to_string(),
        crate::util::internal_auth::loopback_base_url(ui_port),
    )]);
    if let Some(token) = APP_TOKEN.get() {
        env.insert(crate::apps::token::ENV_APP_TOKEN.to_string(), token.clone());
    }
    env
}

/// The app's single gateway to SenClaw services. Every LLM call goes through the
/// app-space-sdk (the daemon's Space-App open API) — the app never contacts an
/// LLM provider directly.
///
/// The target is the daemon this process belongs to, never a fixed port: a
/// daemon on another port (an isolated test run, or a user who moved it) would
/// otherwise send the board's calls to whichever daemon holds 18788.
fn client() -> SpaceClient {
    client_for(
        &crate::util::internal_auth::daemon_base_url(),
        app_token().as_deref(),
    )
}

fn client_for(base_url: &str, token: Option<&str>) -> SpaceClient {
    SpaceClient::with_token(base_url, APP_ID, token)
}

/// One-shot completion on SenClaw's active LLM via the SDK open API.
pub async fn bridge_llm(
    system: &str,
    user: &str,
    max_tokens: u32,
) -> Result<(String, String), String> {
    client()
        .llm_request(system, user, max_tokens)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::token::{self, TokenMode};
    use crate::gateway::ui_server::app_auth::{
        decide, looks_like_daemon_ui, presented_token, Decision,
    };
    use axum::{
        extract::{Path, State},
        http::{HeaderMap, StatusCode, Uri},
        routing::post,
        Json, Router,
    };
    use std::sync::{Arc, Mutex};

    /// A stand-in for the daemon's `/api/space/apps/<id>/bridge`: the real
    /// strict-mode decision over a real token table, with the real SDK client
    /// on the other end of a real socket. Only the HTTP glue is test code.
    struct Gate {
        db: crate::db::Db,
        calls: Mutex<Vec<(String, Option<&'static str>)>>,
    }

    async fn bridge(
        State(g): State<Arc<Gate>>,
        Path(id): Path<String>,
        uri: Uri,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        let presented = presented_token(&headers, uri.query());
        let verdict = decide(
            TokenMode::Strict,
            &id,
            "/bridge",
            presented.as_deref(),
            |t| token::resolve(&g.db, t),
            looks_like_daemon_ui(&headers),
        );
        match verdict {
            Decision::Allow(_) => {
                g.calls.lock().unwrap().push((id, None));
                (
                    StatusCode::OK,
                    Json(json!({ "status": "ok", "text": "served", "model": "mock" })),
                )
            }
            Decision::Deny(status, code, message) => {
                g.calls.lock().unwrap().push((id, Some(code)));
                (status, Json(json!({ "error": message, "code": code })))
            }
        }
    }

    /// Serve the stand-in on an ephemeral loopback port; returns its base URL.
    async fn serve(gate: Arc<Gate>) -> String {
        let app = Router::new()
            .route("/api/space/apps/:id/bridge", post(bridge))
            .with_state(gate);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        base
    }

    fn gate() -> Arc<Gate> {
        let db = crate::db::Db::open_in_memory(&crate::config::Config::from_env()).unwrap();
        Arc::new(Gate {
            db,
            calls: Mutex::new(Vec::new()),
        })
    }

    #[tokio::test]
    async fn strict_mode_serves_the_board_with_its_token_and_refuses_it_without() {
        let gate = gate();
        let token = provision_app_token(&gate.db).unwrap();
        let base = serve(Arc::clone(&gate)).await;

        let served = client_for(&base, Some(&token))
            .llm_request("sys", "prompt", 8)
            .await;
        assert_eq!(served.unwrap(), ("served".to_string(), "mock".to_string()));

        // What every kanban model call did before the token was sent.
        assert!(client_for(&base, None)
            .llm_request("sys", "prompt", 8)
            .await
            .is_err());

        // The board's token is the board's alone.
        let other = token::ensure(&gate.db, "someone-else").unwrap();
        assert!(client_for(&base, Some(&other))
            .llm_request("sys", "prompt", 8)
            .await
            .is_err());

        assert_eq!(
            *gate.calls.lock().unwrap(),
            vec![
                ("kanban".to_string(), None),
                ("kanban".to_string(), Some("app_token_required")),
                ("kanban".to_string(), Some("app_token_scope")),
            ]
        );
    }

    #[test]
    fn the_board_token_is_stable_across_daemon_restarts() {
        let gate = gate();
        let first = provision_app_token(&gate.db).unwrap();
        assert_eq!(provision_app_token(&gate.db).unwrap(), first);
    }

    #[test]
    fn a_kanban_server_child_is_told_where_the_daemon_is_and_who_it_is() {
        // The only test that publishes the process-global token.
        let gate = gate();
        let token = provision_app_token(&gate.db).unwrap();
        set_app_token(&token);

        assert_eq!(app_token().as_deref(), Some(token.as_str()));
        let env = child_env(38788);
        assert_eq!(env["SENCLAW_BASE_URL"], "http://127.0.0.1:38788");
        assert_eq!(env[crate::apps::token::ENV_APP_TOKEN], token);
    }

    #[test]
    fn prompts_without_a_project_are_what_they_were() {
        assert_eq!(
            goal_prompt("Ship v2", Some("keep it small"), None),
            "Project goal: Ship v2\nExtra instruction: keep it small\n\nReturn the JSON now."
        );
        assert_eq!(
            goal_prompt("Ship v2", None, Some("  \n ")),
            "Project goal: Ship v2\n\nReturn the JSON now."
        );
        assert_eq!(
            breakdown_prompt("Write docs", "", None, None, None),
            "Task card: Write docs\n\nReturn the JSON now."
        );
    }

    #[test]
    fn the_project_context_leads_each_prompt_and_is_cut_at_two_thousand_characters() {
        let long = format!("<project name=\"Sen\">{}</project>", "ấ".repeat(5_000));
        for prompt in [
            goal_prompt("Ship v2", None, Some(&long)),
            breakdown_prompt("Write docs", "", None, None, Some(&long)),
        ] {
            assert!(prompt.starts_with(
                "About the project this board belongs to (written by the user):\n<project name=\"Sen\">"
            ));
            let carried = prompt.matches('ấ').count();
            assert!(
                carried < PROJECT_CONTEXT_MAX_CHARS,
                "{carried} characters of context"
            );
            assert!(carried > PROJECT_CONTEXT_MAX_CHARS - 100);
            assert!(
                !prompt.contains("</project>"),
                "the cut drops the end of the block"
            );
        }
        let short = goal_prompt("Ship v2", None, Some("<project>tiny</project>"));
        assert!(short.contains("<project>tiny</project>\n\nProject goal: Ship v2"));
    }
}
