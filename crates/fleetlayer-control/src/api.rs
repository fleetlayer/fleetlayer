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
    let sessions: Vec<Value> = db::sessions(&app.db, tenant)
        .await?
        .into_iter()
        .map(|(node, mut s)| {
            s["node"] = json!(node);
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

/// `/healthz`: ok when the database answers, with the number of linked nodes.
pub async fn healthz(State(app): State<Arc<App>>) -> Result<Json<Value>, StatusCode> {
    sqlx::query("SELECT 1").execute(&app.db).await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let linked = app.links.nodes.lock().await.len();
    Ok(Json(json!({"ok": true, "linkedNodes": linked})))
}
