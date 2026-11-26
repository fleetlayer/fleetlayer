//! The control plane's API, for programs that read the fleet (the web and phone apps, Quietdeck's deck): `GET
//! /v1/fleet`, `/v1/events`, `/v1/subscriptions`, `/v1/endpoints`, `/v1/summaries`, and `POST
//! /v1/sessions/<id>/catchup`. Each needs a bearer token: the admin token file's, or an API token
//! (`fleetlayer-control token add <name> --scope read`). A self-hosted control plane has one tenant; the
//! hosted one will sign people in instead.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use crate::App;
use crate::db::{self, DEFAULT_TENANT};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Read,
    Admin,
}

/// The scope the request's bearer token has: the admin token file's, or an API token's.
async fn scope_of(app: &App, headers: &HeaderMap) -> Option<Scope> {
    let token = headers.get("authorization")?.to_str().ok()?.strip_prefix("Bearer ")?;
    if let Some(expected) = &app.admin_token_hash
        && bool::from(db::hash(token).as_bytes().ct_eq(expected.as_bytes()))
    {
        return Some(Scope::Admin);
    }
    match db::token_scope(&app.db, token).await.ok().flatten() {
        Some((tenant, scope)) if tenant == DEFAULT_TENANT => {
            Some(if scope == "admin" { Scope::Admin } else { Scope::Read })
        }
        _ => None,
    }
}

type ApiResult = Result<Json<Value>, (StatusCode, String)>;

/// Every route here needs at least the read scope.
async fn guard(app: &App, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    match scope_of(app, headers).await {
        Some(Scope::Read | Scope::Admin) => Ok(()),
        None => Err((StatusCode::UNAUTHORIZED, "a read or admin token is needed".into())),
    }
}

fn internal(e: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// Every node (with whether it's linked now) and every session, as last pushed.
pub async fn fleet(app: &App, tenant: &str) -> anyhow::Result<Value> {
    let online = app.links.online(tenant).await;
    let nodes: Vec<Value> = db::nodes(&app.db, tenant)
        .await?
        .into_iter()
        .map(|n| {
            let linked = online.contains(&n.name);
            let mut v = serde_json::to_value(n).unwrap_or_default();
            v["online"] = json!(linked);
            v
        })
        .collect();
    let latest = crate::summaries::latest(&app.db, tenant).await?;
    let sessions: Vec<Value> = db::sessions(&app.db, tenant)
        .await?
        .into_iter()
        .map(|(node, mut s)| {
            s["node"] = json!(node);
            if let Some(l) = s["sessionId"].as_str().and_then(|id| latest.get(id)) {
                s["summary"] = l.clone();
            }
            s
        })
        .collect();
    Ok(json!({"nodes": nodes, "sessions": sessions}))
}

pub async fn get_fleet(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    guard(&app, &headers).await?;
    Ok(Json(fleet(&app, DEFAULT_TENANT).await.map_err(internal)?))
}

pub async fn get_events(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    guard(&app, &headers).await?;
    Ok(Json(json!({"events": db::recent_events(&app.db, DEFAULT_TENANT, 100).await.map_err(internal)?})))
}

pub async fn get_subscriptions(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    guard(&app, &headers).await?;
    Ok(Json(json!({"subscriptions": db::subscriptions(&app.db, DEFAULT_TENANT).await.map_err(internal)?})))
}

pub async fn get_endpoints(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    guard(&app, &headers).await?;
    Ok(Json(json!({"endpoints": db::endpoints(&app.db, DEFAULT_TENANT).await.map_err(internal)?})))
}

#[derive(serde::Deserialize)]
pub struct SummaryQuery {
    q: Option<String>,
    mentions: Option<String>,
    session: Option<String>,
    limit: Option<i64>,
}

/// `GET /v1/summaries?q=words` searches (best match first); `?mentions=sc-1234` finds the sessions whose summaries
/// mention that exact text (newest mention each); `?session=<id>` is one session's whole timeline, oldest first.
pub async fn get_summaries(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<SummaryQuery>,
) -> ApiResult {
    guard(&app, &headers).await?;
    let limit = q.limit.unwrap_or(20).clamp(1, 200);
    let hits = if let Some(s) = &q.session {
        crate::summaries::timeline(&app.db, DEFAULT_TENANT, s, q.limit.unwrap_or(10_000).clamp(1, 100_000)).await
    } else if let Some(m) = &q.mentions {
        crate::summaries::mentions(&app.db, DEFAULT_TENANT, m, limit).await
    } else {
        crate::summaries::search(&app.db, DEFAULT_TENANT, q.q.as_deref().unwrap_or_default(), limit).await
    }
    .map_err(internal)?;
    Ok(Json(json!({"summaries": hits})))
}

/// `POST /v1/sessions/<id>/catchup`: ask the session's node for its catch-up ("since your last message"). Answers at
/// once from the node's cache when it has one, else waits while it's written (up to 8 minutes).
pub async fn post_catchup(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult {
    guard(&app, &headers).await?;
    let node = db::session_node(&app.db, DEFAULT_TENANT, &id)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, format!("no session {id}")))?;
    let peer = app
        .links
        .peer(DEFAULT_TENANT, &node)
        .await
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, format!("node {node} is offline")))?;
    let c: Value = peer
        .request_within(
            fleetlayer_protocol::link::method::CATCHUP,
            &json!({"sessionId": id}),
            std::time::Duration::from_secs(490),
        )
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, e.message))?;
    Ok(Json(c))
}

/// `/healthz`: ok when the database answers, with the number of linked nodes.
pub async fn healthz(State(app): State<Arc<App>>) -> Result<Json<Value>, StatusCode> {
    sqlx::query("SELECT 1").execute(&app.db).await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let linked = app.links.nodes.lock().await.len();
    Ok(Json(json!({"ok": true, "linkedNodes": linked})))
}
