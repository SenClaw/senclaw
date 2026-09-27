//! The pre-turn skill router: which skill, if any, a request should run with
//! — and whether to load it outright or only hint at it.
//!
//! Measured on 224 installed skills and 32 real requests
//! (`plans/reports/research-260926-1032-pre-skill-jev-laya.md`):
//! - the legacy keyword matcher is right 20/32 and, with `preTriggerSkill`
//!   on, force-loads a **wrong** skill 10 times;
//! - reading each skill's description too (its quoted example requests) is
//!   right 23/32 ([`Reading::Full`]);
//! - Laya on its own is worse (15/32, 16 wrong loads) and confidently wrong on
//!   small talk ("chào bạn" → `x-browse` at 1.00) — so it never picks a skill
//!   by itself here;
//! - as a **co-signer** it helps: load only when the keyword top pick and the
//!   decision engine agree, or the top pick matched a whole trigger/example and
//!   the engine's pick is in the top three. That is 14 right loads and 2 wrong,
//!   the rest become hints. Without an answer from the engine the rule falls
//!   back to "load only on a whole-phrase match" (14 right, 3 wrong).
//!
//! The engine is asked one `choice` over the keyword top-8 plus `none`, each
//! option the skill's triggers and the first sentence of its description.

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::decision::client;
use crate::decision::json::Json;
use crate::decision::types::AskRequest;
use crate::runtime::manager::RuntimeManager;
use crate::skills::matching::{self, Reading, Scored, SkillCard, SkillRoute};

/// A turn waits at most this long for the engine (~75 ms warm on Laya); a cold
/// load or a slow provider falls back to the keyword rule for that turn while
/// the load carries on for the next.
pub const ROUTE_TIMEOUT: Duration = Duration::from_millis(1500);
/// How many keyword candidates the engine chooses among. Top-8 held the right
/// skill for 28–29 of the 32 requests.
pub const CANDIDATES: usize = 8;
const MAX_STATE_CHARS: usize = 2_000;
const NONE: &str = "none";

/// Everything one routing decision rests on — what the log and the Settings
/// page show.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteReport {
    /// What the legacy matcher does (the behaviour with the router off).
    pub legacy: Option<SkillRoute>,
    /// What the router decides (applied when it is on).
    pub route: Option<SkillRoute>,
    pub reason: String,
    /// The keyword candidates (full reading), best first.
    pub candidates: Vec<Scored>,
    /// The engine's choice; `None` when it chose "none" or was not asked.
    pub engine_pick: Option<String>,
    pub engine_p: Option<f64>,
    pub model: Option<String>,
    pub latency_ms: Option<f64>,
    /// Why the engine's opinion is missing, when it is.
    pub fallback: Option<String>,
}

/// What the legacy matcher does: the best keyword match, loaded when
/// `preTriggerSkill` is on and hinted otherwise.
pub fn legacy_route(cards: &[SkillCard], prompt: &str, pre_trigger: bool) -> Option<SkillRoute> {
    matching::best(&matching::score(cards, prompt, Reading::Legacy)).map(|s| SkillRoute {
        name: s.name.clone(),
        force: pre_trigger,
    })
}

/// The rule from the research report. `engine` is `None` when the engine did
/// not answer, `Some(None)` when it answered "none".
pub fn decide(candidates: &[Scored], engine: Option<Option<&str>>, pre_trigger: bool) -> (Option<SkillRoute>, String) {
    let Some(top) = matching::best(candidates) else {
        return (None, "no skill matches the request".into());
    };
    let (co_signed, why) = match engine {
        None => (
            top.phrase_hit,
            if top.phrase_hit {
                "no answer from the decision engine; a whole trigger matched"
            } else {
                "no answer from the decision engine; only words overlap"
            },
        ),
        Some(pick) => {
            let agree = pick == Some(top.name.as_str());
            let in_top3 = pick.is_some_and(|p| candidates.iter().take(3).any(|c| c.name == p));
            if agree {
                (true, "the keyword match and the decision engine agree")
            } else if top.phrase_hit && in_top3 {
                (true, "a whole trigger matched and the decision engine's pick is in the top three")
            } else if pick.is_none() {
                (false, "the decision engine sees no skill for this")
            } else {
                (false, "the decision engine picked another skill")
            }
        }
    };
    let force = pre_trigger && co_signed;
    let reason = format!("{why} → {}", if force { "load" } else { "hint" });
    (
        Some(SkillRoute {
            name: top.name.clone(),
            force,
        }),
        reason,
    )
}

fn first_sentence(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let end = flat
        .char_indices()
        .find(|&(i, c)| matches!(c, '.' | '!' | '?') && flat[i + c.len_utf8()..].starts_with(' '))
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(flat.len());
    flat[..end].chars().take(max_chars).collect()
}

/// An option as the engine reads it: the triggers, then what the skill does.
fn option_text(card: &SkillCard) -> String {
    let triggers: Vec<&str> = card.triggers.iter().map(|t| t.trim()).filter(|t| !t.is_empty()).take(6).collect();
    let what = first_sentence(&card.description, 140);
    if triggers.is_empty() {
        what
    } else {
        format!("{} — {what}", triggers.join("; "))
    }
}

fn question(cards: &[SkillCard], candidates: &[Scored]) -> Json {
    let mut criteria: Vec<(String, Json)> = candidates
        .iter()
        .filter_map(|c| cards.iter().find(|k| k.name == c.name))
        .map(|k| (k.name.clone(), Json::String(option_text(k))))
        .collect();
    criteria.push((
        NONE.into(),
        Json::String("a greeting, small talk or a general question — no tool needed".into()),
    ));
    let route = Json::Object(vec![
        ("type".into(), Json::String("choice".into())),
        ("instructions".into(), Json::String("Which skill should handle this request?".into())),
        ("criteria".into(), Json::Object(criteria)),
    ]);
    Json::Object(vec![("route".into(), route)])
}

/// Route one request. Never fails: without the engine it decides by keywords.
///
/// Takes no `DecisionSettings` — unlike the gate, the router asks no question
/// the backend choice would change, so nothing here reads settings the
/// `sen-sysone` runtime does not already own itself.
pub async fn route(
    runtime: &RuntimeManager,
    prompt: &str,
    cards: &[SkillCard],
    pre_trigger: bool,
) -> RouteReport {
    let legacy = legacy_route(cards, prompt, pre_trigger);
    let candidates: Vec<Scored> = matching::score(cards, prompt, Reading::Full)
        .into_iter()
        .filter(|s| s.score > 0)
        .take(CANDIDATES)
        .collect();
    let mut report = RouteReport {
        legacy,
        route: None,
        reason: String::new(),
        candidates,
        engine_pick: None,
        engine_p: None,
        model: None,
        latency_ms: None,
        fallback: None,
    };
    if report.candidates.is_empty() {
        report.reason = "no skill matches the request".into();
        return report;
    }

    let request = AskRequest {
        backend: None,
        model: None,
        state: Json::String(prompt.chars().take(MAX_STATE_CHARS).collect()),
        questions: question(cards, &report.candidates),
    };
    let started = Instant::now();
    let answered = tokio::time::timeout(ROUTE_TIMEOUT, client::ask(runtime, &request)).await;
    report.latency_ms = Some((started.elapsed().as_secs_f64() * 10_000.0).round() / 10.0);
    let engine: Option<Option<String>> = match answered {
        Err(_) => {
            report.fallback = Some(format!(
                "the decision engine did not answer within {} ms",
                ROUTE_TIMEOUT.as_millis()
            ));
            None
        }
        Ok(Err(e)) => {
            report.fallback = Some(format!("the decision engine failed: {e}"));
            None
        }
        Ok(Ok(resp)) => {
            report.model = Some(resp.model.clone());
            let answers = serde_json::to_value(&resp.answers).unwrap_or_default();
            let a = &answers["route"];
            match a["choice"].as_str() {
                Some(choice) => {
                    report.engine_p = a["probabilities"][choice].as_f64();
                    Some((choice != NONE).then(|| choice.to_string()))
                }
                None => {
                    report.fallback = Some("the decision engine answered without a choice".into());
                    None
                }
            }
        }
    };
    report.engine_pick = engine.clone().flatten();
    let (route, reason) = decide(
        &report.candidates,
        engine.as_ref().map(|p| p.as_deref()),
        pre_trigger,
    );
    report.route = route;
    report.reason = reason;
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scored(items: &[(&str, u32, bool)]) -> Vec<Scored> {
        items
            .iter()
            .map(|(n, s, h)| Scored {
                name: n.to_string(),
                score: *s,
                phrase_hit: *h,
            })
            .collect()
    }

    #[test]
    fn a_load_needs_the_engine_to_co_sign() {
        let c = scored(&[("clock-timer", 60, true), ("calendar", 20, false), ("schedule", 15, false)]);
        let load = |engine, pre| decide(&c, engine, pre).0.unwrap();
        assert!(load(Some(Some("clock-timer")), true).force, "agreement loads");
        assert!(load(Some(Some("schedule")), true).force, "a whole trigger + a pick in the top three loads");
        assert!(!load(Some(Some("x-browse")), true).force, "a pick outside the top three only hints");
        assert!(!load(Some(None), true).force, "\"none\" only hints");
        assert!(!load(Some(Some("clock-timer")), false).force, "never loads with preTriggerSkill off");
        assert_eq!(load(Some(Some("x-browse")), true).name, "clock-timer", "the hint is still the keyword pick");
    }

    #[test]
    fn word_overlap_alone_needs_agreement_and_no_match_means_nothing() {
        let weak = scored(&[("weather-xem", 15, false), ("agent-browser", 10, false)]);
        assert!(!decide(&weak, Some(Some("agent-browser")), true).0.unwrap().force);
        assert!(decide(&weak, Some(Some("weather-xem")), true).0.unwrap().force);
        assert_eq!(decide(&scored(&[("a", 10, false)]), Some(Some("a")), true).0, None, "below the threshold");
    }

    #[test]
    fn without_the_engine_only_a_whole_phrase_loads() {
        assert!(decide(&scored(&[("a", 50, true)]), None, true).0.unwrap().force);
        assert!(!decide(&scored(&[("a", 30, false)]), None, true).0.unwrap().force);
    }

    #[test]
    fn options_lead_with_the_triggers_and_keep_the_first_sentence() {
        let card = SkillCard {
            name: "clock-timer".into(),
            description: "Hẹn giờ và đếm ngược qua app Clock. Dùng khi người dùng muốn hẹn giờ.".into(),
            when_to_use: None,
            triggers: vec!["hẹn giờ".into(), " đếm ngược ".into()],
        };
        assert_eq!(option_text(&card), "hẹn giờ; đếm ngược — Hẹn giờ và đếm ngược qua app Clock.");
        let q = serde_json::to_string(&question(&[card], &scored(&[("clock-timer", 50, true)]))).unwrap();
        assert!(q.contains(r#""criteria":{"clock-timer":"#) && q.ends_with(r#""none":"a greeting, small talk or a general question — no tool needed"}}}"#), "{q}");
    }

    #[tokio::test]
    async fn an_engine_failure_falls_back_to_the_keyword_rule() {
        let cards = vec![SkillCard {
            name: "clock-timer".into(),
            description: "Hẹn giờ. Dùng khi người dùng muốn \"hẹn giờ 10 phút\".".into(),
            when_to_use: None,
            triggers: vec!["hẹn giờ".into()],
        }];
        let tmp = tempfile::tempdir().unwrap(); // no decision runtime installed here
        let runtime = crate::runtime::manager::RuntimeManager::new(crate::runtime::manager::RuntimeManagerConfig {
            runtimes_dir: tmp.path().join("runtimes"),
            runtime_data_dir: tmp.path().join("runtime-data"),
            runtime_logs_dir: tmp.path().join("logs"),
            bundled_dir: None,
            local_models_dir: tmp.path().join("local-models"),
            config_path: tmp.path().join("config.json"),
            home: tmp.path().to_path_buf(),
            index_url: "file:///dev/null".to_string(),
        });
        let r = route(&runtime, "đặt hẹn giờ 10 phút giúp tôi", &cards, true).await;
        assert!(r.fallback.is_some());
        assert_eq!(r.route, Some(SkillRoute { name: "clock-timer".into(), force: true }));
        assert_eq!(r.legacy, Some(SkillRoute { name: "clock-timer".into(), force: true }));
    }
}
