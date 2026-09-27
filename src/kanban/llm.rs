use app_space_sdk::SpaceClient;
use serde::Deserialize;
use serde_json::{json, Value};

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

/// Ask the LLM to design a whole board (columns + cards) for `goal`.
pub async fn generate_board(
    goal: &str,
    instruction: Option<&str>,
) -> Result<GeneratedBoard, String> {
    let mut prompt = format!("Project goal: {goal}\n");
    if let Some(i) = instruction {
        if !i.trim().is_empty() {
            prompt.push_str(&format!("Extra instruction: {i}\n"));
        }
    }
    prompt.push_str("\nReturn the JSON now.");

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
) -> Result<GeneratedCards, String> {
    let mut prompt = format!("Project goal: {goal}\n");
    if let Some(i) = instruction {
        if !i.trim().is_empty() {
            prompt.push_str(&format!("Extra instruction: {i}\n"));
        }
    }
    prompt.push_str("\nReturn the JSON now.");
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

/// Ask the LLM to break `card_title` into subtask cards.
pub async fn breakdown_card(
    card_title: &str,
    card_desc: &str,
    board_outline: Option<&str>,
    instruction: Option<&str>,
) -> Result<GeneratedCards, String> {
    let mut prompt = format!("Task card: {card_title}\n");
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

/// The app's single gateway to SenClaw services. Every LLM call goes through the
/// app-space-sdk (the daemon's Space-App open API) — the app never contacts an
/// LLM provider directly.
fn client() -> SpaceClient {
    if std::env::var("SENCLAW_SPACE_APP_ID").is_err() {
        std::env::set_var("SENCLAW_SPACE_APP_ID", "kanban");
    }
    SpaceClient::from_env()
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
