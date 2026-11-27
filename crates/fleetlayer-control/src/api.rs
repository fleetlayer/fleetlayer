//! The control plane's API, for programs that read the fleet (the web and phone apps, Quietdeck's deck): `GET
//! /v1/fleet`, `/v1/events`, `/v1/subscriptions`, `/v1/endpoints`, `/v1/summaries`, and `POST
//! /v1/sessions/<id>/catchup`. Each needs a bearer token: the admin token file's, or an API token
//! (`fleetlayer-control token add <name> --scope read`). A self-hosted control plane has one tenant; the
//! hosted one will sign people in instead.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

use crate::App;
use crate::db::{self, DEFAULT_TENANT};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Read,
    Admin,
    /// A service that only sends messages, as `service:<token name>`.
    Send,
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
        Some((tenant, scope)) if tenant == DEFAULT_TENANT => Some(match scope.as_str() {
            "admin" => Scope::Admin,
            "send" => Scope::Send,
            _ => Scope::Read,
        }),
        _ => None,
    }
}

type ApiResult = Result<Json<Value>, (StatusCode, String)>;

/// Every route here needs at least the read scope.
async fn guard(app: &App, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    match scope_of(app, headers).await {
        Some(Scope::Read | Scope::Admin) => Ok(()),
        Some(Scope::Send) => {
            Err((StatusCode::FORBIDDEN, "a send token only sends messages (POST /v1/messages)".into()))
        }
        None => Err((StatusCode::UNAUTHORIZED, "a read or admin token is needed".into())),
    }
}

fn internal(e: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// Every node (with whether it's linked now) and every session, as last pushed.
pub async fn fleet(app: &App, tenant: &str) -> anyhow::Result<Value> {
    fleet_with(app, tenant, true).await
}

/// The fleet, with each session's latest summary when `summaries` is set.
pub async fn fleet_with(app: &App, tenant: &str, summaries: bool) -> anyhow::Result<Value> {
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
    let latest = if summaries { crate::summaries::latest(&app.db, tenant).await? } else { Default::default() };
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
    since: Option<String>,
    mentions: Option<String>,
    session: Option<String>,
    limit: Option<i64>,
}

/// `GET /v1/summaries?q=words` searches (best match first); `?mentions=sc-1234` finds the sessions whose summaries
/// mention that exact text (newest mention each); `?session=<id>` is one session's whole timeline, oldest first;
/// `?since=<cursor>` is the feed of every record written after it, oldest first, with `nextCursor`.
pub async fn get_summaries(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<SummaryQuery>,
) -> ApiResult {
    guard(&app, &headers).await?;
    let limit = q.limit.unwrap_or(20).clamp(1, 200);
    if let Some(after) = &q.since {
        let hits = crate::summaries::feed(&app.db, DEFAULT_TENANT, after, q.limit.unwrap_or(500).clamp(1, 5000))
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("since: {e}")))?;
        let next = hits.last().map(|h| h["cursor"].clone()).unwrap_or(json!(after));
        return Ok(Json(json!({"summaries": hits, "nextCursor": next})));
    }
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

pub async fn get_approvals(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    guard(&app, &headers).await?;
    Ok(Json(json!({"approvals": crate::approvals::list(&app.db, DEFAULT_TENANT, false).await.map_err(internal)?})))
}

#[derive(serde::Deserialize)]
pub struct ApprovalBody {
    approve: bool,
    /// How long an approval holds (`10m`, `2h`, `30d`); the request's own when absent.
    #[serde(default, rename = "for")]
    hold: Option<String>,
}

/// `POST /v1/approvals/<id>` `{approve, for?}`: decide a pending request. Admin tokens only: approving widens what an
/// agent may do.
pub async fn post_approval(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<ApprovalBody>,
) -> ApiResult {
    if scope_of(&app, &headers).await != Some(Scope::Admin) {
        return Err((StatusCode::FORBIDDEN, "approving needs an admin token".into()));
    }
    let hold = body
        .hold
        .as_deref()
        .map(crate::policy::parse_duration_ms)
        .transpose()
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let done = crate::approvals::decide(&app.db, DEFAULT_TENANT, &id, body.approve, hold).await.map_err(internal)?;
    let Some(d) = done else {
        return Err((StatusCode::NOT_FOUND, format!("no pending request {id}")));
    };
    Ok(Json(json!({"id": id, "approved": body.approve, "from": d.subject, "verb": d.verb, "to": d.target})))
}

/// `GET /v1/windows`: the windowed roles and their state, and the recent windows (`docs/access-control.md`).
pub async fn get_windows(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    guard(&app, &headers).await?;
    let roles = match app.authz.policy(&app.db, DEFAULT_TENANT).await.map_err(internal)? {
        Some(p) => crate::windows::list_for(&app.db, DEFAULT_TENANT, &p, None).await.map_err(internal)?,
        None => vec![],
    };
    let recent = crate::windows::history(&app.db, DEFAULT_TENANT, 50).await.map_err(internal)?;
    Ok(Json(json!({"roles": roles, "windows": recent})))
}

/// `DELETE /v1/windows/<role>` (admin token): close a role's window now. `{closed}`.
pub async fn delete_window(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    axum::extract::Path(role): axum::extract::Path<String>,
) -> ApiResult {
    if scope_of(&app, &headers).await != Some(Scope::Admin) {
        return Err((StatusCode::FORBIDDEN, "closing a window needs an admin token".into()));
    }
    let closed = crate::windows::deactivate(&app, DEFAULT_TENANT, &role, "person (API)").await.map_err(internal)?;
    Ok(Json(json!({"role": role, "closed": closed})))
}

/// `/healthz`: ok when the database answers, with the number of linked nodes.
/// `{ok, linkedNodes, degraded: [reason]}`. 200 while the server can answer, so Kubernetes' probes don't restart it
/// over a problem a restart won't fix; `ok` is false while something is degraded (an audit write failed within the
/// last five minutes, or a policy file couldn't be applied). `?strict` answers 503 then, for an uptime check that only reads the status.
pub async fn healthz(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<(StatusCode, Json<Value>), StatusCode> {
    sqlx::query("SELECT 1").execute(&app.db).await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let linked = app.links.nodes.lock().await.len();
    let mut degraded: Vec<String> = app.audit.degraded().into_iter().collect();
    degraded.extend(app.authz.file_errors.lock().unwrap_or_else(|e| e.into_inner()).values().cloned());
    degraded.extend(crate::windows::problems(&app.db).await);
    let ok = degraded.is_empty();
    let status = if !ok && q.contains_key("strict") { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::OK };
    Ok((status, Json(json!({"ok": ok, "linkedNodes": linked, "degraded": degraded}))))
}

/// `GET /v1/audit/verify` (admin token): check the audit chain and anchors in the server, so a scheduled check doesn't
/// open the database itself. Answers only the result and counts, never rows or their content: 200 `{result: "OK",
/// keyed, rows, problems: 0, anchors}`, 409 with `result: "FAILED"` and how many problems. `audit verify` in the
/// container says which rows.
pub async fn get_audit_verify(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<Value>), (StatusCode, String)> {
    match scope_of(&app, &headers).await {
        Some(Scope::Admin) => {}
        Some(_) => return Err((StatusCode::FORBIDDEN, "an admin token is needed".into())),
        None => return Err((StatusCode::UNAUTHORIZED, "an admin token is needed".into())),
    }
    let (report, ok) = crate::audit::verify(&app.audit, &app.db, app.audit.anchors()).await.map_err(internal)?;
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log")
        .fetch_one(&app.db)
        .await
        .map_err(|e| internal(e.into()))?;
    let anchors = app.audit.anchors().map(|d| {
        std::fs::read_dir(d)
            .map(|r| r.filter(|e| e.as_ref().is_ok_and(|e| crate::audit::is_anchor(&e.path()))).count())
            .unwrap_or(0)
    });
    let problems = report.iter().filter(|l| crate::audit::is_problem(l)).count();
    let status = if ok { StatusCode::OK } else { StatusCode::CONFLICT };
    Ok((
        status,
        Json(json!({"result": if ok { "OK" } else { "FAILED" }, "keyed": app.audit.keyed(), "rows": rows,
            "problems": problems, "anchors": anchors})),
    ))
}

#[derive(Debug, serde::Deserialize)]
pub struct PostMessage {
    /// `<tab>@<node>`.
    to: String,
    text: String,
    /// Shown to the recipient as who it's from; default: the service's name.
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    subkind: Option<String>,
}

/// `POST /v1/messages`: a service sends a message to a session, as `service:<token name>`. Needs a `send` token; the
/// access policy decides (the `message` verb), it's audited and rate-limited like any message.
pub async fn post_message(State(app): State<Arc<App>>, headers: HeaderMap, Json(m): Json<PostMessage>) -> ApiResult {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or((StatusCode::UNAUTHORIZED, "a send token is needed".to_string()))?;
    let (tenant, scope, name) = db::token_info(&app.db, token)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::UNAUTHORIZED, "a send token is needed".to_string()))?;
    if scope != "send" {
        return Err((StatusCode::FORBIDDEN, "only a send token sends messages".into()));
    }
    let delivered =
        crate::route::send_as_service(&app, &tenant, &name, &m.to, m.text, m.label, m.subkind).await.map_err(|e| {
            let status = match e.code {
                fleetlayer_protocol::rpc::code::REFUSED => StatusCode::FORBIDDEN,
                fleetlayer_protocol::rpc::code::NO_SUCH => StatusCode::NOT_FOUND,
                fleetlayer_protocol::rpc::code::NOT_HERE | fleetlayer_protocol::rpc::code::BUSY => {
                    StatusCode::SERVICE_UNAVAILABLE
                }
                _ => StatusCode::BAD_REQUEST,
            };
            (status, serde_json::to_string(&e).unwrap_or(e.message))
        })?;
    Ok(Json(serde_json::to_value(delivered).unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn app() -> Arc<App> {
        Arc::new(App {
            db: db::open_memory().await.unwrap(),
            links: Default::default(),
            hook_limits: Default::default(),
            admin_token_hash: Some(db::hash("admin-token")),
            streams: Default::default(),
            authz: Default::default(),
            audit: crate::audit::Audit::new(None),
        })
    }

    fn query(strict: bool) -> Query<HashMap<String, String>> {
        Query(if strict { [("strict".to_string(), String::new())].into() } else { HashMap::new() })
    }

    #[tokio::test]
    async fn a_failed_audit_write_shows_in_healthz() {
        let app = app().await;
        let (status, Json(v)) = healthz(State(app.clone()), query(true)).await.unwrap();
        assert_eq!((status, v["ok"].clone()), (StatusCode::OK, json!(true)));

        // The table gone: every write fails, as when the database stays locked.
        sqlx::query("DROP TABLE audit_log").execute(&app.db).await.unwrap();
        let e = crate::audit::Entry {
            decision: "allowed".into(),
            subject: "a@laptop".into(),
            verb: "message".into(),
            target: "b@laptop".into(),
            reason: "test".into(),
        };
        assert!(app.audit.write(&app.db, "default", &e, 1).await.is_err());

        let (status, Json(v)) = healthz(State(app.clone()), query(false)).await.unwrap();
        assert_eq!(status, StatusCode::OK, "probes keep passing: a restart wouldn't fix it");
        assert_eq!(v["ok"], false);
        assert!(v["degraded"][0].as_str().unwrap().starts_with("the audit log can't be written"), "{v}");
        let (status, _) = healthz(State(app), query(true)).await.unwrap();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "?strict fails, for the uptime check");
    }

    #[tokio::test]
    async fn audit_verify_needs_the_admin_token() {
        let app = app().await;
        let mut h = HeaderMap::new();
        assert_eq!(get_audit_verify(State(app.clone()), h.clone()).await.unwrap_err().0, StatusCode::UNAUTHORIZED);
        h.insert("authorization", "Bearer admin-token".parse().unwrap());
        let (status, Json(v)) = get_audit_verify(State(app.clone()), h.clone()).await.unwrap();
        assert_eq!(v, json!({"result": "OK", "keyed": false, "rows": 0, "problems": 0, "anchors": null}));
        assert_eq!(status, StatusCode::OK);

        // A row changed: FAILED with a count, and nothing about the row.
        let e = crate::audit::Entry {
            decision: "allowed".into(),
            subject: "secret-subject@laptop".into(),
            verb: "message".into(),
            target: "b@laptop".into(),
            reason: "test".into(),
        };
        app.audit.write(&app.db, "default", &e, 1).await.unwrap();
        let triggers: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = 'audit_log'")
                .fetch_all(&app.db)
                .await
                .unwrap();
        for t in triggers {
            sqlx::query(sqlx::AssertSqlSafe(format!("DROP TRIGGER {t}"))).execute(&app.db).await.unwrap();
        }
        sqlx::query("UPDATE audit_log SET reason = 'edited'").execute(&app.db).await.unwrap();
        let (status, Json(v)) = get_audit_verify(State(app.clone()), h).await.unwrap();
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            (v["result"].clone(), v["rows"].clone(), v["problems"].clone()),
            (json!("FAILED"), json!(1), json!(1))
        );
        assert!(!v.to_string().contains("secret-subject"), "{v}");

        // A read token isn't enough.
        let read = db::add_token(&app.db, DEFAULT_TENANT, "reader", "read").await.unwrap();
        let mut h = HeaderMap::new();
        h.insert("authorization", format!("Bearer {read}").parse().unwrap());
        assert_eq!(get_audit_verify(State(app), h).await.unwrap_err().0, StatusCode::FORBIDDEN);
    }
}
