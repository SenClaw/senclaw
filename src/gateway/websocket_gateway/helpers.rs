// ===== Helpers =====

use std::sync::Arc;

use axum::extract::ws::Message;
use tokio::sync::Mutex;

use super::state::WsClient;

pub(crate) async fn require_auth(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
) -> bool {
    let guard = clients.lock().await;
    let Some(client) = guard.get(client_idx) else {
        return false;
    };
    if !client.authenticated {
        drop(guard);
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "Not authenticated"}),
        );
        return false;
    }
    true
}

pub(crate) async fn require_admin(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
) -> bool {
    let guard = clients.lock().await;
    let Some(client) = guard.get(client_idx) else {
        return false;
    };
    if !client.is_admin {
        drop(guard);
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "Admin subscription required"}),
        );
        return false;
    }
    true
}

pub(crate) fn send_json(
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    msg: &serde_json::Value,
) {
    let _ = sender.send(Message::Text(msg.to_string().into()));
}

pub(crate) async fn send_error(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    message: &str,
) {
    let guard = clients.lock().await;
    if let Some(client) = guard.get(client_idx) {
        let _ = client.sender.send(Message::Text(
            serde_json::json!({"type": "error", "message": message})
                .to_string()
                .into(),
        ));
    }
}

pub(crate) async fn broadcast_to_all_inner(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    msg: &serde_json::Value,
) {
    let raw = msg.to_string();
    let guard = clients.lock().await;
    for client in guard.iter() {
        if client.authenticated {
            let _ = client.sender.send(Message::Text(raw.clone().into()));
        }
    }
}

pub(crate) fn now_iso() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    // Simple ISO timestamp matching the project style.
    let days_since_epoch = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;
    let (year, month, day) = days_to_ymd(days_since_epoch as i64);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}.000Z")
}

fn days_to_ymd(mut days: i64) -> (i64, u32, u32) {
    days += 719468;
    let era = if days >= 0 { days } else { days - 146096 } / 146097;
    let doe = (days - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
