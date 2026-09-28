//! Fit the candidate list into what the local checkpoint can read.
//!
//! laya-browser reads every option of a question inside one `head_max_len`
//! budget (768 tokens at training, shared with the instructions), so a busy
//! page — Google Flights offers 56+ clickable elements — does not fit. Each
//! operation's candidates are ranked and cut to a character budget, then put
//! back in page order so indices still read top to bottom. Controls (scroll,
//! wait) are never cut.

use std::collections::HashSet;

use serde_json::Value;

/// Characters of option lines per question that fit beside the instructions.
/// Conservative: the runtime answers "do not fit" rather than truncating, and
/// the loop then halves this and asks again.
pub const DEFAULT_OPTION_CHARS: usize = 1400;

/// A cheap stand-in for one format-v3 option line's length.
fn line_len(action: &Value) -> usize {
    let label = action.get("label").and_then(Value::as_str).unwrap_or_default().chars().count().min(50);
    let role = action.get("role").and_then(Value::as_str).unwrap_or_default().len();
    let value = action
        .get("current_value")
        .or_else(|| action.get("value"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .chars()
        .count()
        .min(30);
    let flags = ["checked", "selected", "expanded"].iter().filter(|f| action.get(**f).is_some()).count() * 14;
    8 + label + role + 3 + if value > 0 { value + 5 } else { 0 } + flags
}

/// Lower-case, Vietnamese diacritics folded, common Latin accents folded.
pub fn fold(text: &str) -> String {
    crate::security::replication::fold(text)
        .chars()
        .map(|c| match c {
            'ä' | 'å' | 'â' => 'a',
            'ö' | 'ø' | 'ô' => 'o',
            'ü' | 'û' => 'u',
            'ë' | 'ê' => 'e',
            'ï' | 'î' => 'i',
            'ñ' => 'n',
            'ç' => 'c',
            other => other,
        })
        .collect()
}

pub fn tokens(text: &str) -> HashSet<String> {
    fold(text)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 3 || t.chars().all(|c| c.is_ascii_digit()) && !t.is_empty())
        .map(str::to_string)
        .collect()
}

fn score(action: &Value, goal: &HashSet<String>, position: usize, total: usize) -> f64 {
    let role = action.get("role").and_then(Value::as_str).unwrap_or_default();
    let kind = action.get("kind").and_then(Value::as_str).unwrap_or_default();
    let mut s = match role {
        "option" => 60.0,
        "textbox" | "searchbox" | "combobox" | "spinbutton" => 50.0,
        "button" => 30.0,
        "checkbox" | "radio" | "switch" => 25.0,
        "gridcell" => 20.0,
        "tab" | "menuitem" | "menuitemradio" | "menuitemcheckbox" => 15.0,
        "link" => 10.0,
        _ => 5.0,
    };
    if kind == "fill" || kind == "select" {
        s += 10.0;
    }
    let text = format!(
        "{} {}",
        action.get("label").and_then(Value::as_str).unwrap_or_default(),
        action.get("value").and_then(Value::as_str).unwrap_or_default()
    );
    let overlap = tokens(&text).intersection(goal).count().min(3);
    s += 25.0 * overlap as f64;
    // Reading order breaks ties: what is higher on the page first.
    s += 10.0 * (1.0 - position as f64 / total.max(1) as f64);
    s
}

/// Cut `observation.actions` so every operation's options fit `max_chars`.
/// Returns the pruned observation and how many candidates were left out.
pub fn prune(observation: &Value, goal: &str, max_chars: usize) -> (Value, usize) {
    let actions = observation.get("actions").and_then(Value::as_array).cloned().unwrap_or_default();
    let goal_tokens = tokens(goal);
    let total = actions.len();
    let mut keep = vec![true; total];
    let mut omitted = 0;
    for kind in ["click", "fill", "select"] {
        let mut group: Vec<(usize, f64, usize)> = actions
            .iter()
            .enumerate()
            .filter(|(_, a)| a.get("kind").and_then(Value::as_str) == Some(kind))
            .map(|(i, a)| (i, score(a, &goal_tokens, i, total), line_len(a)))
            .collect();
        group.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
        let mut used = 0;
        for (i, _, len) in group {
            if used + len <= max_chars {
                used += len;
            } else {
                keep[i] = false;
                omitted += 1;
            }
        }
    }
    let kept: Vec<Value> = actions.into_iter().zip(keep).filter(|(_, k)| *k).map(|(a, _)| a).collect();
    let mut pruned = observation.clone();
    pruned["actions"] = Value::Array(kept);
    (pruned, omitted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn budget_keeps_priority_candidates_within_limit() {
        let mut actions = vec![json!({"id": "e0", "node": 1, "kind": "fill", "role": "combobox", "label": "Where to?", "value": ""})];
        for i in 0..120 {
            actions.push(json!({"id": format!("f{i}"), "node": 100 + i, "kind": "click", "role": "link", "label": format!("Footer link number {i}"), "value": ""}));
        }
        actions.push(json!({"id": "e1", "node": 2, "kind": "click", "role": "option", "label": "Zürich, Switzerland", "value": ""}));
        actions.push(json!({"id": "e2", "node": 3, "kind": "click", "role": "link", "label": "Cheap flights to London", "value": ""}));
        actions.push(json!({"id": "e3", "node": 4, "kind": "click", "role": "button", "label": "Search", "value": ""}));
        actions.push(json!({"id": "scroll_down", "kind": "scroll", "label": "Scroll down", "delta": 560}));
        actions.push(json!({"id": "wait", "kind": "wait", "label": "Wait for the page to update"}));
        let obs = json!({"url": "https://a.test/", "title": "A", "text": "", "actions": actions});

        let (pruned, omitted) = prune(&obs, "Flights from Zurich to London", 400);
        let kept = pruned["actions"].as_array().unwrap();
        let ids: Vec<&str> = kept.iter().map(|a| a["id"].as_str().unwrap()).collect();
        assert!(omitted > 100, "most footer links are cut ({omitted})");
        for must in ["e0", "e1", "e2", "e3", "scroll_down", "wait"] {
            assert!(ids.contains(&must), "{must} kept: {ids:?}");
        }
        let clicks: usize = kept.iter().filter(|a| a["kind"] == "click").map(line_len).sum();
        assert!(clicks <= 400, "click options fit the budget ({clicks})");
        // Page order is preserved among what is kept.
        let pos = |id: &str| ids.iter().position(|x| *x == id).unwrap();
        assert!(pos("e1") < pos("e2") && pos("e2") < pos("e3"));
        // Nothing to cut, nothing cut.
        let (same, none) = prune(&obs, "anything", 1_000_000);
        assert_eq!(none, 0);
        assert_eq!(same["actions"].as_array().unwrap().len(), obs["actions"].as_array().unwrap().len());
    }

    #[test]
    fn folding_matches_across_accents() {
        assert!(tokens("Zürich").contains("zurich"));
        assert!(tokens("Hà Nội").contains("noi"));
        assert!(tokens("Đặt vé máy bay").contains("dat"));
    }
}
