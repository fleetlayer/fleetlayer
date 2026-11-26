//! The control plane's read API, for the CLI and later the web and phone apps: `GET /v1/fleet`, `/v1/events`,
//! `/v1/subscriptions`, `/v1/endpoints`, behind the admin token. A self-hosted control plane has one tenant; the
//! hosted one will sign people in instead.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use crate::App;
use crate::db::{self, DEFAULT_TENANT};

fn authorized(app: &App, headers: &HeaderMap) -> bool {
    let Some(expected) = &app.admin_token_hash else { return false };
    let Some(token) =
        headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    bool::from(db::hash(token).as_bytes().ct_eq(expected.as_bytes()))
}

type ApiResult = Result<Json<Value>, (StatusCode, String)>;

fn guard(app: &App, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    if authorized(app, headers) { Ok(()) } else { Err((StatusCode::UNAUTHORIZED, "an admin token is needed".into())) }
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
    guard(&app, &headers)?;
    Ok(Json(fleet(&app, DEFAULT_TENANT).await.map_err(internal)?))
}

pub async fn get_events(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    guard(&app, &headers)?;
    Ok(Json(json!({"events": db::recent_events(&app.db, DEFAULT_TENANT, 100).await.map_err(internal)?})))
}

pub async fn get_subscriptions(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    guard(&app, &headers)?;
    Ok(Json(json!({"subscriptions": db::subscriptions(&app.db, DEFAULT_TENANT).await.map_err(internal)?})))
}

pub async fn get_endpoints(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    guard(&app, &headers)?;
    Ok(Json(json!({"endpoints": db::endpoints(&app.db, DEFAULT_TENANT).await.map_err(internal)?})))
}

#[derive(serde::Deserialize)]
pub struct SummaryQuery {
    q: Option<String>,
    session: Option<String>,
    limit: Option<i64>,
}

/// `GET /v1/summaries?q=words` searches; `?session=<id>` is one session's timeline.
pub async fn get_summaries(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<SummaryQuery>,
) -> ApiResult {
    guard(&app, &headers)?;
    let limit = q.limit.unwrap_or(20).clamp(1, 200);
    let hits = match &q.session {
        Some(s) => crate::summaries::timeline(&app.db, DEFAULT_TENANT, s, limit).await,
        None => crate::summaries::search(&app.db, DEFAULT_TENANT, q.q.as_deref().unwrap_or_default(), limit).await,
    }
    .map_err(internal)?;
    Ok(Json(json!({"summaries": hits})))
}

/// `/healthz`: ok when the database answers, with the number of linked nodes.
pub async fn healthz(State(app): State<Arc<App>>) -> Result<Json<Value>, StatusCode> {
    sqlx::query("SELECT 1").execute(&app.db).await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let linked = app.links.nodes.lock().await.len();
    Ok(Json(json!({"ok": true, "linkedNodes": linked})))
}
