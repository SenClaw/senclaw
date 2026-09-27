//! The daemon's machine-readable API contract, derived from the routers.
//!
//! Four SDKs, three clients and a browser extension each hand-write their own
//! models against these routes, with nothing to check them against. A spec
//! maintained by hand beside 350 routes would drift on its first commit, so
//! this one is **parsed out of the router source itself** (`include_str!`), at
//! the same paths and methods axum registers. A route that exists is in the
//! spec; a route that is deleted leaves it. There is nothing to keep in step.
//!
//! What that buys and what it does not: paths, methods, path parameters,
//! tags and per-route summaries are exact. Request and response **bodies**
//! are not — most handlers return `Json<serde_json::Value>`, so the spec says
//! `object` and marks the operation `x-untyped: true`. Those counts are in
//! the spec's own `x-untyped-operations`, which is the list to work down.
//!
//! `GET /api/openapi.json` serves it, and `docs/openapi-daemon.yaml` is the
//! committed copy a test keeps current.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde_json::{json, Value};

/// The router sources, with the prefix each is mounted under.
///
/// `core.rs` carries the daemon's own routes (including the auth sub-router
/// it merges); the other two are nested, so their paths need their mount
/// prefix put back.
const ROUTER_SOURCES: &[(&str, &str)] = &[
    ("", include_str!("core.rs")),
    ("/api/kanban", include_str!("../../kanban/api.rs")),
    ("/api/sandbox", include_str!("../../sandbox/api.rs")),
];

/// One route as the router declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteDecl {
    pub path: String,
    /// Lower-case HTTP methods, in declaration order.
    pub methods: Vec<String>,
    /// Handler names, parallel to `methods`.
    pub handlers: Vec<String>,
}

/// Every HTTP method axum's routing module exposes as a top-level builder.
const METHODS: &[&str] = &[
    "get", "post", "put", "patch", "delete", "head", "options", "trace",
];

/// Pull the `.route("<path>", <method>(<handler>)…)` declarations out of one
/// router source.
///
/// Deliberately a small scanner rather than a regex: the second argument
/// chains method builders (`get(a).post(b)`) and handlers are path-qualified
/// (`super::auth::auth_login`), which a single pattern reads badly.
pub fn parse_routes(src: &str) -> Vec<RouteDecl> {
    let mut out = Vec::new();
    let bytes = src.as_bytes();
    let mut i = 0usize;
    while let Some(found) = src[i..].find(".route(") {
        let start = i + found + ".route(".len();
        i = start;
        // The path literal is the next string literal.
        let Some(q1) = src[start..].find('"') else {
            break;
        };
        let lit_start = start + q1 + 1;
        let Some(q2) = src[lit_start..].find('"') else {
            break;
        };
        let path = &src[lit_start..lit_start + q2];
        // Only real API paths; the static-file fallbacks and the test routers
        // inside `auth.rs` are not part of the contract.
        if !path.starts_with('/') {
            continue;
        }
        // The rest of the call, up to the matching close paren.
        let mut depth = 1usize;
        let mut j = lit_start + q2 + 1;
        while j < bytes.len() && depth > 0 {
            match bytes[j] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            j += 1;
        }
        let body = &src[lit_start + q2 + 1..j.saturating_sub(1)];
        let (methods, handlers) = parse_method_chain(body);
        if !methods.is_empty() {
            out.push(RouteDecl {
                path: path.to_string(),
                methods,
                handlers,
            });
        }
        i = j;
    }
    out
}

/// Read `, get(a).post(b)` into `(["get","post"], ["a","b"])`.
fn parse_method_chain(body: &str) -> (Vec<String>, Vec<String>) {
    let mut methods = Vec::new();
    let mut handlers = Vec::new();
    let mut rest = body;
    while let Some(pos) = rest.find('(') {
        let head = rest[..pos]
            .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        // Take the argument up to the paren that closes this call.
        let after = &rest[pos + 1..];
        let mut depth = 1usize;
        let mut end = 0usize;
        for (k, c) in after.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = k;
                        break;
                    }
                }
                _ => {}
            }
        }
        let arg = &after[..end];
        if METHODS.contains(&head.as_str()) {
            methods.push(head);
            handlers.push(handler_name(arg));
        }
        rest = &after[end.min(after.len())..];
        if rest.is_empty() {
            break;
        }
        rest = &rest[1.min(rest.len())..];
    }
    (methods, handlers)
}

/// The bare name of a handler expression: `super::auth::auth_login` →
/// `auth_login`, a closure → `inline`.
fn handler_name(arg: &str) -> String {
    let arg = arg.trim();
    if arg.starts_with('|') || arg.starts_with("async") || arg.is_empty() {
        return "inline".to_string();
    }
    arg.split(&[',', ' ', '<'][..])
        .next()
        .unwrap_or(arg)
        .rsplit("::")
        .next()
        .unwrap_or(arg)
        .trim()
        .to_string()
}

/// Every route the daemon serves, mount prefixes applied, sorted by path.
pub fn all_routes() -> Vec<RouteDecl> {
    let mut out: Vec<RouteDecl> = Vec::new();
    for (prefix, src) in ROUTER_SOURCES {
        for mut r in parse_routes(src) {
            if !prefix.is_empty() {
                // A nested router's "/" is the prefix itself.
                r.path = if r.path == "/" {
                    prefix.to_string()
                } else {
                    format!("{prefix}{}", r.path)
                };
            }
            out.push(r);
        }
    }
    // A router source may declare the same path twice across sub-routers
    // (the auth sub-router is merged into the same file); keep the first.
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup_by(|a, b| a.path == b.path && a.methods == b.methods);
    out
}

/// Tag an operation by the first meaningful path segment, so a generated
/// client groups the way the UI does.
fn tag_for(path: &str) -> String {
    let mut segs = path.trim_start_matches('/').split('/');
    let first = segs.next().unwrap_or("");
    if first != "api" {
        return "static".to_string();
    }
    match segs.next().unwrap_or("") {
        "" => "meta".to_string(),
        s => s.to_string(),
    }
}

/// Rewrite axum's `:name` and `*rest` captures into OpenAPI `{name}`, and
/// report the parameter names in order.
fn openapi_path(path: &str) -> (String, Vec<String>) {
    let mut params = Vec::new();
    let converted: Vec<String> = path
        .split('/')
        .map(|seg| {
            if let Some(name) = seg.strip_prefix(':') {
                params.push(name.to_string());
                format!("{{{name}}}")
            } else if let Some(name) = seg.strip_prefix('*') {
                params.push(name.to_string());
                format!("{{{name}}}")
            } else {
                seg.to_string()
            }
        })
        .collect();
    (converted.join("/"), params)
}

/// A document-unique operation id: `get_api_kanban_board_rename`.
fn operation_id(method: &str, path: &str) -> String {
    let slug: String = path
        .trim_start_matches('/')
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let slug = slug.trim_matches('_').replace("__", "_");
    format!("{method}_{slug}")
}

/// A readable summary from the handler name: `llm_config_set_active` →
/// "Llm config set active". It is the only description the source offers,
/// and it beats an empty `summary` in a generated client.
fn summary_for(handler: &str) -> String {
    if handler == "inline" {
        return "Inline handler".to_string();
    }
    let mut s = handler.replace('_', " ");
    if let Some(first) = s.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    s
}

/// Build the OpenAPI document.
pub fn spec() -> Value {
    static CACHE: OnceLock<Value> = OnceLock::new();
    CACHE.get_or_init(build_spec).clone()
}

fn build_spec() -> Value {
    let routes = all_routes();
    let mut paths: BTreeMap<String, Value> = BTreeMap::new();
    let mut untyped = 0usize;
    let mut tags: BTreeMap<String, ()> = BTreeMap::new();

    for r in &routes {
        let (path, param_names) = openapi_path(&r.path);
        let tag = tag_for(&r.path);
        tags.insert(tag.clone(), ());

        let parameters: Vec<Value> = param_names
            .iter()
            .map(|name| {
                json!({
                    "name": name,
                    "in": "path",
                    "required": true,
                    "schema": { "type": "string" },
                })
            })
            .collect();

        let entry = paths.entry(path).or_insert_with(|| json!({}));
        let obj = entry.as_object_mut().expect("path item is an object");
        for (method, handler) in r.methods.iter().zip(r.handlers.iter()) {
            untyped += 1;
            // The id must be unique across the document, and two routes can
            // legitimately share a handler name (`status` exists under more
            // than one subtree), so the path is what disambiguates.
            let mut op = json!({
                "tags": [tag],
                "summary": summary_for(handler),
                "operationId": operation_id(method, &r.path),
                "responses": {
                    "200": {
                        "description": "Success",
                        "content": { "application/json": { "schema": { "type": "object" } } }
                    },
                    "401": { "description": "Token required or invalid (see SENCLAW_AUTH_MODE)" }
                },
                // The handler's own types are not read, so nothing here is a
                // promise about the body's shape.
                "x-untyped": true,
                "x-handler": handler,
            });
            if !parameters.is_empty() {
                op["parameters"] = json!(parameters);
            }
            // The two paths the auth middleware lets through unauthenticated.
            // Without this a client is told to authenticate before it can ask
            // whether authentication is required.
            if crate::gateway::ui_server::auth::OPEN_API_PATHS.contains(&r.path.as_str()) {
                op["security"] = json!([]);
                op["responses"]
                    .as_object_mut()
                    .expect("responses object")
                    .remove("401");
            }
            if matches!(method.as_str(), "post" | "put" | "patch") {
                op["requestBody"] = json!({
                    "required": false,
                    "content": { "application/json": { "schema": { "type": "object" } } }
                });
            }
            obj.insert(method.clone(), op);
        }
    }

    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "SenClaw daemon API",
            "version": env!("CARGO_PKG_VERSION"),
            "description":
                "Generated from the daemon's own routers. Paths, methods and path \
                 parameters are exact. Bodies are not described: handlers that return \
                 `Json<serde_json::Value>` are marked `x-untyped: true`, and \
                 `x-untyped-operations` counts how many remain.\n\n\
                 A few templates overlap (`/api/oauth/{provider}/start` and \
                 `/api/oauth/accounts/{id}`, for example). That is not a conflict at \
                 runtime: axum matches a literal segment before a capture, so \
                 `/api/oauth/accounts/1` reaches the accounts handler. A client \
                 generated from this document should prefer the more literal template \
                 for the same reason.",
        },
        "servers": [{ "url": "http://127.0.0.1:18788", "description": "Local daemon" }],
        // Whether the token is actually demanded is `SENCLAW_AUTH_MODE`, so a
        // loopback client on the default `auto` sends none. Declaring the
        // schemes anyway is what lets a generated client offer them at all.
        "components": {
            "securitySchemes": {
                "bearerToken": {
                    "type": "http",
                    "scheme": "bearer",
                    "description":
                        "The daemon API token: SENCLAW_API_TOKEN, else the generated \
                         ~/.senclaw/api_token.",
                },
                "tokenHeader": {
                    "type": "apiKey",
                    "in": "header",
                    "name": "X-SenClaw-Token",
                    "description": "The same token, for clients that cannot set Authorization.",
                },
                "sessionCookie": {
                    "type": "apiKey",
                    "in": "cookie",
                    "name": "senclaw_token",
                    "description": "Minted by POST /api/auth/login.",
                },
            }
        },
        "security": [
            { "bearerToken": [] },
            { "tokenHeader": [] },
            { "sessionCookie": [] },
        ],
        "tags": tags.keys().map(|t| json!({ "name": t })).collect::<Vec<_>>(),
        "x-route-count": routes.len(),
        "x-untyped-operations": untyped,
        "paths": paths,
    })
}

/// `GET /api/openapi.json`
pub(crate) async fn openapi_json() -> axum::Json<Value> {
    axum::Json(spec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_and_chained_method_declarations() {
        let src = r#"
            .route("/api/one", get(handler_one))
            .route(
                "/api/two/:id",
                get(super::mod_a::get_two).put(mod_b::put_two),
            )
            .route("/api/three", post(|| async { "x" }))
        "#;
        let routes = parse_routes(src);
        assert_eq!(routes.len(), 3);
        assert_eq!(routes[0].path, "/api/one");
        assert_eq!(routes[0].methods, vec!["get"]);
        assert_eq!(routes[0].handlers, vec!["handler_one"]);
        assert_eq!(routes[1].methods, vec!["get", "put"]);
        assert_eq!(
            routes[1].handlers,
            vec!["get_two".to_string(), "put_two".to_string()],
            "a path-qualified handler keeps only its own name"
        );
        assert_eq!(routes[2].handlers, vec!["inline"]);
    }

    #[test]
    fn axum_captures_become_openapi_parameters() {
        // Route params in this crate are axum 0.7 `:name`; braces there are a
        // literal segment, which is exactly the bug this conversion must not
        // reintroduce in the other direction.
        let (path, params) = openapi_path("/api/chats/:jid/messages/:id");
        assert_eq!(path, "/api/chats/{jid}/messages/{id}");
        assert_eq!(params, vec!["jid".to_string(), "id".to_string()]);
        let (path, params) = openapi_path("/api/plugins/:name/widget-static/*path");
        assert_eq!(path, "/api/plugins/{name}/widget-static/{path}");
        assert_eq!(params, vec!["name".to_string(), "path".to_string()]);
    }

    #[test]
    fn the_real_routers_are_covered() {
        let routes = all_routes();
        // The daemon has hundreds of routes; a parser that silently matched a
        // handful would pass every other assertion here.
        assert!(
            routes.len() > 250,
            "only parsed {} routes — the scanner missed most of the router",
            routes.len()
        );
        let paths: Vec<&str> = routes.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.contains(&"/api/auth/status"));
        assert!(paths.contains(&"/api/config"));
        // Nested routers get their mount prefix back.
        assert!(
            paths.iter().any(|p| p.starts_with("/api/kanban/")),
            "kanban routes are nested at /api/kanban"
        );
        assert!(
            paths.iter().any(|p| p.starts_with("/api/sandbox/")),
            "sandbox routes are nested at /api/sandbox"
        );
        // Every path must be routable as declared.
        for r in &routes {
            assert!(r.path.starts_with('/'), "bad path {}", r.path);
            assert!(!r.methods.is_empty(), "{} has no method", r.path);
        }
    }

    /// The committed `docs/openapi-daemon.yaml` is the copy clients read, so
    /// it must be what the routers currently say. Run with
    /// `SENCLAW_WRITE_OPENAPI=1` to refresh it after adding routes.
    #[test]
    fn committed_spec_matches_the_routers() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/openapi-daemon.yaml");
        let current = serde_yaml::to_string(&spec()).expect("serialize spec");
        if std::env::var("SENCLAW_WRITE_OPENAPI").is_ok() {
            std::fs::write(path, &current).expect("write spec");
            return;
        }
        let committed = std::fs::read_to_string(path).unwrap_or_default();
        assert_eq!(
            committed.trim(),
            current.trim(),
            "docs/openapi-daemon.yaml is stale — rerun with SENCLAW_WRITE_OPENAPI=1"
        );
    }

    #[test]
    fn spec_is_a_valid_document_with_converted_paths() {
        let s = spec();
        assert_eq!(s["openapi"], "3.1.0");
        let paths = s["paths"].as_object().unwrap();
        assert!(paths.len() > 200);
        for key in paths.keys() {
            assert!(
                !key.contains(':') && !key.contains('*'),
                "{key} still carries an axum capture"
            );
        }
        // Operation ids must be unique across the document or a generated
        // client has two methods with one name; handler names alone are not
        // (several subtrees have a `status`).
        let mut ids = std::collections::HashSet::new();
        for (path, item) in paths {
            for (method, op) in item.as_object().unwrap() {
                let id = op["operationId"].as_str().unwrap();
                assert!(ids.insert(id.to_string()), "duplicate operationId {id} at {method} {path}");
            }
        }
        // The open paths must not demand a token: a client has to be able to
        // ask whether one is needed.
        assert_eq!(paths["/api/auth/status"]["get"]["security"], json!([]));
        assert!(paths["/api/config"]["get"].get("security").is_none());

        // The honesty markers are the point: a consumer must be able to see
        // which operations describe no body.
        assert!(s["x-untyped-operations"].as_u64().unwrap() > 0);
        let op = &paths["/api/config"]["get"];
        assert_eq!(op["x-untyped"], true);
        assert_eq!(op["tags"][0], "config");
    }
}
