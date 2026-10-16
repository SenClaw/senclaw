//! `/api/memory/facts` — see and forget what the agent remembers.
//!
//! Facts are extracted from conversations automatically, so the person has to
//! be able to look at them and take one away: a wrong fact, or one they did not
//! want kept. Both calls name the `bank` (the agent folder); a fact of another
//! bank is never listed or deleted.
//!
//! The handlers hold no daemon state, only a way to find the fact store, so the
//! routes can be built — and tested through a real router — without a running
//! daemon. That matters here: a mistyped path parameter or query name compiles
//! and then silently never matches.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{delete, get};
use axum::Router;

use super::core::AppError;
use crate::memory::cognitive::retrievers::build_fts_match;
use crate::memory::engine::facts::{self, FactEngine, StoredFact};

const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 200;

/// How a request finds the fact store: the process-wide one in the daemon, a
/// private one in a test.
type Resolver = Arc<dyn Fn() -> Option<Arc<FactEngine>> + Send + Sync>;

/// The routes, for any router state (the handlers use none of it).
pub(crate) fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    router(Arc::new(facts::instance))
}

fn router<S: Clone + Send + Sync + 'static>(resolver: Resolver) -> Router<S> {
    Router::new()
        .route("/api/memory/facts", get(facts_list))
        .route("/api/memory/facts/:id", delete(fact_forget))
        .with_state(resolver)
}

fn engine(resolve: &Resolver) -> Result<Arc<FactEngine>, AppError> {
    resolve().ok_or_else(|| {
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
        "proofCount": f.proof_count,
        "supersededBy": f.superseded_by,
    })
}

#[derive(serde::Deserialize)]
struct ListQuery {
    /// The agent folder.
    bank: String,
    /// Keyword search; omitted → newest first.
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
    /// Also list facts that were replaced (they carry `supersededBy`). Only
    /// the newest-first listing has a history; a keyword search never returns
    /// replaced facts.
    #[serde(default)]
    history: Option<bool>,
}

/// GET /api/memory/facts?bank=&q=&limit=&history= — facts of one agent, newest
/// first or best keyword matches.
async fn facts_list(
    State(resolve): State<Resolver>,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let engine = engine(&resolve)?;
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize;
    let found = match q.q.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(text) => match build_fts_match(text) {
            Some(m) => engine.store.search_keyword(&q.bank, &m, None, limit),
            None => Ok(Vec::new()),
        },
        None => engine
            .store
            .recent(&q.bank, limit, q.history.unwrap_or(false)),
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
struct ForgetQuery {
    bank: String,
}

/// DELETE /api/memory/facts/:id?bank= — forget one fact.
async fn fact_forget(
    State(resolve): State<Resolver>,
    Path(id): Path<String>,
    Query(q): Query<ForgetQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let deleted = engine(&resolve)?
        .store
        .delete_in_bank(&q.bank, &id)
        .map_err(internal)?;
    if !deleted {
        return Err(AppError(StatusCode::NOT_FOUND, "no such fact".into()));
    }
    Ok(Json(serde_json::json!({ "deleted": id })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use crate::memory::engine::facts::store::{FactKind, Inserted, NewFact};
    use axum::body::Body;
    use axum::http::{Method, Request};
    use tower::ServiceExt;

    fn seeded() -> (Arc<FactEngine>, Vec<String>) {
        let db = Arc::new(Db::open_in_memory(&Config::from_env()).unwrap());
        let engine = Arc::new(FactEngine::new(db, None));
        let mut ids = Vec::new();
        for (bank, text, at) in [
            ("main", "Emily moved to Hue in March", 10),
            ("main", "Sen prefers dark mode", 30),
            ("main", "The deploy is on Friday", 20),
            ("other", "Another agent's private fact", 99),
        ] {
            let id = match engine
                .store
                .insert(
                    bank,
                    &NewFact {
                        text: text.into(),
                        kind: FactKind::World,
                        occurred: None,
                        mentioned_at: at,
                        entities: vec![],
                        embedding: None,
                        model: None,
                        source: "test".into(),
                    },
                )
                .unwrap()
            {
                Inserted::New(id) => id,
                other => panic!("{other:?}"),
            };
            ids.push(id);
        }
        (engine, ids)
    }

    async fn call(
        engine: Option<Arc<FactEngine>>,
        method: Method,
        uri: &str,
    ) -> (StatusCode, serde_json::Value) {
        let resolver: Resolver = Arc::new(move || engine.clone());
        let app: Router<()> = router(resolver);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    fn texts(v: &serde_json::Value) -> Vec<String> {
        v["facts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["text"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn listing_is_newest_first_and_only_the_banks_own() {
        let (e, _) = seeded();
        let (status, body) = call(Some(e), Method::GET, "/api/memory/facts?bank=main").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["bank"], "main");
        assert_eq!(body["total"], 3);
        assert_eq!(body["count"], 3);
        assert_eq!(
            texts(&body),
            [
                "Sen prefers dark mode",
                "The deploy is on Friday",
                "Emily moved to Hue in March"
            ]
        );
        // The wire names are camelCase, the shape a client codes against.
        let first = &body["facts"][0];
        for key in [
            "id",
            "kind",
            "text",
            "occurredStart",
            "occurredEnd",
            "mentionedAt",
            "entities",
            "proofCount",
            "supersededBy",
        ] {
            assert!(first.get(key).is_some(), "missing {key}: {first}");
        }
        assert_eq!(first["kind"], "world");
    }

    #[tokio::test]
    async fn a_query_searches_by_keyword_and_a_query_with_no_words_finds_nothing() {
        let (e, _) = seeded();
        let (_, body) = call(
            Some(e.clone()),
            Method::GET,
            "/api/memory/facts?bank=main&q=emily",
        )
        .await;
        assert_eq!(texts(&body), ["Emily moved to Hue in March"]);
        assert_eq!(body["total"], 3, "total counts the bank, not the matches");

        let (status, body) = call(
            Some(e.clone()),
            Method::GET,
            "/api/memory/facts?bank=main&q=%3F%21",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["count"], 0);

        // A blank q is no query: newest first, not an empty match.
        let (_, body) = call(Some(e), Method::GET, "/api/memory/facts?bank=main&q=%20").await;
        assert_eq!(body["count"], 3);
    }

    #[tokio::test]
    async fn a_replaced_fact_is_hidden_unless_history_is_asked_for() {
        let (e, ids) = seeded();
        assert!(e.store.supersede("main", &ids[0], &ids[1], 99).unwrap());

        let (_, body) = call(Some(e.clone()), Method::GET, "/api/memory/facts?bank=main").await;
        assert_eq!(body["count"], 2);
        assert!(!texts(&body).contains(&"Emily moved to Hue in March".to_string()));

        let (_, body) = call(
            Some(e.clone()),
            Method::GET,
            "/api/memory/facts?bank=main&history=true",
        )
        .await;
        assert_eq!(body["count"], 3);
        let replaced = body["facts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["text"] == "Emily moved to Hue in March")
            .unwrap();
        assert_eq!(replaced["supersededBy"], ids[1]);

        // A keyword search never returns it, history or not.
        let (_, body) = call(
            Some(e),
            Method::GET,
            "/api/memory/facts?bank=main&q=emily&history=true",
        )
        .await;
        assert_eq!(body["count"], 0);
    }

    #[tokio::test]
    async fn the_limit_is_clamped_not_rejected() {
        let (e, _) = seeded();
        let (status, body) = call(
            Some(e.clone()),
            Method::GET,
            "/api/memory/facts?bank=main&limit=0",
        )
        .await;
        assert_eq!((status, body["count"].as_u64()), (StatusCode::OK, Some(1)));
        let (_, body) = call(
            Some(e.clone()),
            Method::GET,
            "/api/memory/facts?bank=main&limit=2",
        )
        .await;
        assert_eq!(body["count"], 2);
        let (_, body) = call(
            Some(e),
            Method::GET,
            "/api/memory/facts?bank=main&limit=99999",
        )
        .await;
        assert_eq!(body["count"], 3);
    }

    #[tokio::test]
    async fn naming_no_bank_is_a_client_error_for_both_calls() {
        let (e, ids) = seeded();
        let (status, _) = call(Some(e.clone()), Method::GET, "/api/memory/facts").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = call(
            Some(e),
            Method::DELETE,
            &format!("/api/memory/facts/{}", ids[0]),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn forgetting_removes_the_fact_once_and_then_it_is_gone() {
        let (e, ids) = seeded();
        let uri = format!("/api/memory/facts/{}?bank=main", ids[1]);
        let (status, body) = call(Some(e.clone()), Method::DELETE, &uri).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["deleted"], ids[1]);

        let (status, body) = call(Some(e.clone()), Method::DELETE, &uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "no such fact");

        let (_, body) = call(Some(e), Method::GET, "/api/memory/facts?bank=main").await;
        assert_eq!(body["count"], 2);
        assert!(!texts(&body).contains(&"Sen prefers dark mode".to_string()));
    }

    #[tokio::test]
    async fn one_agent_cannot_forget_another_agents_fact_by_id() {
        let (e, ids) = seeded();
        let other_fact = &ids[3];
        let (status, _) = call(
            Some(e.clone()),
            Method::DELETE,
            &format!("/api/memory/facts/{other_fact}?bank=main"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            e.store.count("other").unwrap(),
            1,
            "the other agent's fact is untouched"
        );
    }

    #[tokio::test]
    async fn before_the_fact_store_starts_both_calls_say_so() {
        let (status, body) = call(None, Method::GET, "/api/memory/facts?bank=main").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"], "fact_store_not_started");
        let (status, _) = call(None, Method::DELETE, "/api/memory/facts/x?bank=main").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn an_unknown_path_or_verb_does_not_match() {
        let (e, _) = seeded();
        let (status, _) = call(Some(e.clone()), Method::POST, "/api/memory/facts?bank=main").await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        let (status, _) = call(Some(e), Method::GET, "/api/memory/fact?bank=main").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
