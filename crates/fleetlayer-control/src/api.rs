//! The control plane's API, for programs that read the fleet (the dashboard, the web and phone apps, Quietdeck's deck):
//! `GET /v1/fleet`, `/v1/events`, `/v1/subscriptions`, `/v1/endpoints`, `/v1/summaries`, `/v1/readings`, `/v1/tools`,
//! and `POST
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
    /// A deck that reads approvals and windows, decides approvals and closes windows (`stepup.rs`).
    Approve,
    /// A service that people sign in to (the dashboard): reads like `Read`, and acts only for the person it names in
    /// `Fleetlayer-Person`, as that person.
    Delegate,
}

/// The header a delegate token names the signed-in person in (their person ID, the sign-in's `sub`).
pub const PERSON_HEADER: &str = "fleetlayer-person";

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
            "approve" => Scope::Approve,
            "delegate" => Scope::Delegate,
            _ => Scope::Read,
        }),
        _ => None,
    }
}

type ApiResult = Result<Json<Value>, (StatusCode, String)>;

/// Every route here needs at least the read scope.
async fn guard(app: &App, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    match scope_of(app, headers).await {
        Some(Scope::Read | Scope::Admin | Scope::Delegate) => Ok(()),
        Some(Scope::Send) => {
            Err((StatusCode::FORBIDDEN, "a send token only sends messages (POST /v1/messages)".into()))
        }
        Some(Scope::Approve) => Err((
            StatusCode::FORBIDDEN,
            "an approve token only reads and decides approvals and windows (/v1/approvals, /v1/windows)".into(),
        )),
        None => Err((StatusCode::UNAUTHORIZED, "a read or admin token is needed".into())),
    }
}

/// The routes an approve token reaches too: reading approvals and windows.
async fn guard_approvals(app: &App, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    match scope_of(app, headers).await {
        Some(Scope::Approve) => Ok(()),
        _ => guard(app, headers).await,
    }
}

/// Who decides with the request's token.
struct Approver {
    /// As recorded: `admin token`, `person:<handle> (token <name>)`, or `person:<handle> (via <name>)`.
    by: String,
    /// An approve token's owner, or the person a delegate token acts for (a person ID); None for the admin token.
    person: Option<String>,
}

/// Write a refused deck decision to the audit log (an approve token refused by its owner's standing or a missing
/// passkey), so attempts with a stolen token show.
async fn audit_refusal(app: &App, subject: &str, target: &str, reason: String) {
    let entry = crate::audit::Entry {
        decision: "denied".into(),
        subject: subject.into(),
        verb: "approve".into(),
        target: target.into(),
        reason,
    };
    if let Err(e) = app.audit.record(&app.db, DEFAULT_TENANT, entry).await {
        tracing::error!(target, "a refused approval's audit entry couldn't be written: {e}");
    }
}

/// Who decides with the request's token: the admin token, or an approve token's owner, checked to be one of the
/// tenant's admins now. Anything else is refused; an approve token refused for its owner is audited, as about `target`.
async fn approver(app: &App, headers: &HeaderMap, what: &str, target: &str) -> Result<Approver, (StatusCode, String)> {
    match scope_of(app, headers).await {
        Some(Scope::Admin) => Ok(Approver { by: "admin token".into(), person: None }),
        Some(Scope::Approve) => {
            let token = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .unwrap_or_default();
            let (_, _, name, owner) = db::token_owner(&app.db, token)
                .await
                .map_err(internal)?
                .ok_or((StatusCode::UNAUTHORIZED, "unknown token".to_string()))?;
            let fleet = app.authz.fleet(&app.db).await.map_err(internal)?;
            match crate::stepup::owner_may_approve(&fleet, DEFAULT_TENANT, owner.as_deref()) {
                Ok((id, handle)) => Ok(Approver { by: format!("{handle} (token {name})"), person: Some(id) }),
                Err(e) => {
                    audit_refusal(app, &format!("token {name}"), target, e.clone()).await;
                    Err((StatusCode::FORBIDDEN, e))
                }
            }
        }
        Some(Scope::Delegate) => {
            let token = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .unwrap_or_default();
            let (_, _, name) = db::token_info(&app.db, token)
                .await
                .map_err(internal)?
                .ok_or((StatusCode::UNAUTHORIZED, "unknown token".to_string()))?;
            let Some(person) = headers.get(PERSON_HEADER).and_then(|v| v.to_str().ok()).filter(|p| !p.is_empty())
            else {
                let why = format!("a delegate token acts for a person: {what} needs their ID in Fleetlayer-Person");
                audit_refusal(app, &format!("token {name}"), target, why.clone()).await;
                return Err((StatusCode::FORBIDDEN, why));
            };
            let fleet = app.authz.fleet(&app.db).await.map_err(internal)?;
            match crate::stepup::person_may_approve(&fleet, DEFAULT_TENANT, person) {
                Ok((id, handle)) => Ok(Approver { by: format!("{handle} (via {name})"), person: Some(id) }),
                Err(e) => {
                    audit_refusal(app, &format!("{person} (via {name})"), target, e.clone()).await;
                    Err((StatusCode::FORBIDDEN, e))
                }
            }
        }
        Some(_) => Err((StatusCode::FORBIDDEN, format!("{what} needs an admin, approve or delegate token"))),
        None => Err((StatusCode::UNAUTHORIZED, format!("{what} needs an admin, approve or delegate token"))),
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

/// `GET /v1/token`: what the request's token is, so a service can show only what it may do. `{scope, name}` (`name`
/// null for the admin token file's). Any token.
pub async fn get_token(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    let Some(scope) = scope_of(&app, &headers).await else {
        return Err((StatusCode::UNAUTHORIZED, "a token is needed".into()));
    };
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    let name = db::token_info(&app.db, token).await.map_err(internal)?.map(|(_, _, n)| n);
    let scope = match scope {
        Scope::Read => "read",
        Scope::Admin => "admin",
        Scope::Send => "send",
        Scope::Approve => "approve",
        Scope::Delegate => "delegate",
    };
    Ok(Json(json!({"scope": scope, "name": name})))
}

#[derive(serde::Deserialize)]
pub struct ReadingsQuery {
    /// Comma-separated kinds (`node.status,plugin.status`); every kind when absent.
    kind: Option<String>,
}

/// `GET /v1/readings?kind=a,b`: the newest reading of each kind, key and source node (`docs/protocol.md`, section 9),
/// every kind when `kind` is absent. `{readings: [{kind, key, node, at, data}]}`.
pub async fn get_readings(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<ReadingsQuery>,
) -> ApiResult {
    guard(&app, &headers).await?;
    let kinds: Vec<String> = match q.kind.as_deref().filter(|k| !k.trim().is_empty()) {
        Some(k) => k.split(',').map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect(),
        None => crate::readings::kinds(&app.db, DEFAULT_TENANT).await.map_err(internal)?,
    };
    let readings = crate::readings::latest(&app.db, DEFAULT_TENANT, &kinds).await.map_err(internal)?;
    Ok(Json(json!({"readings": readings})))
}

/// `GET /v1/tools`: the tools each node shares, as it last said (names, descriptions, its own `allow:` patterns and
/// folder names; never paths or secrets), with whether the node is linked now. `{tools: [{node, online, ...ToolInfo}]}`.
pub async fn get_tools(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    guard(&app, &headers).await?;
    let online = app.links.online(DEFAULT_TENANT).await;
    let mut tools: Vec<Value> = vec![];
    for ((tenant, node), list) in app.links.tools.lock().await.iter() {
        if tenant != DEFAULT_TENANT {
            continue;
        }
        for t in list {
            let mut v = serde_json::to_value(t).unwrap_or_default();
            v["node"] = json!(node);
            v["online"] = json!(online.contains(node));
            tools.push(v);
        }
    }
    tools.sort_by(|a, b| (a["node"].as_str(), a["name"].as_str()).cmp(&(b["node"].as_str(), b["name"].as_str())));
    Ok(Json(json!({"tools": tools})))
}

pub async fn get_approvals(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    guard_approvals(&app, &headers).await?;
    Ok(Json(json!({"approvals": crate::approvals::list(&app.db, DEFAULT_TENANT, false).await.map_err(internal)?})))
}

#[derive(serde::Deserialize)]
pub struct ApprovalBody {
    approve: bool,
    /// How long an approval holds (`10m`, `2h`, `30d`); the request's own when absent.
    #[serde(default, rename = "for")]
    hold: Option<String>,
}

/// `POST /v1/approvals/<id>` `{approve, for?}`: decide a pending request, with the admin token or an approve token
/// (`stepup.rs`): approving widens what an agent may do.
pub async fn post_approval(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<ApprovalBody>,
) -> ApiResult {
    let Approver { by, person } = approver(&app, &headers, "deciding a request", &id).await?;
    let asked = body
        .hold
        .as_deref()
        .map(crate::policy::parse_duration_ms)
        .transpose()
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let view = crate::stepup::view(&app.db, DEFAULT_TENANT, &id)
        .await
        .map_err(internal)?
        .filter(|v| v["status"] == "pending")
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("no pending request {id}")))?;
    // An approve token grants no longer than the request asked for (`stepup::capped_hold`); the admin token may.
    let request_hold = view["holdMs"].as_i64().unwrap_or(0);
    let hold = match person {
        Some(_) => Some(crate::stepup::capped_hold(asked, request_hold)),
        None => asked,
    };
    // Opening a role window from a deck needs its owner's passkey for this very request, covering the window it opens
    // (`stepup.rs`).
    let window = crate::stepup::needs_stepup(view["verb"].as_str().unwrap_or(""), view["to"].as_str().unwrap_or(""));
    if body.approve
        && window
        && let Some(p) = &person
        && !crate::stepup::consume(&app.db, DEFAULT_TENANT, &id, p, hold.unwrap_or(request_hold))
            .await
            .map_err(internal)?
    {
        let why = format!(
            "opening a role window needs {} to confirm request {id} with their passkey first, at the sign-in \
             service's /stepup page; then approve within 5 minutes, for no longer than the page showed",
            by.split(' ').next().unwrap_or("its owner")
        );
        audit_refusal(&app, &by, &id, why.clone()).await;
        return Err((StatusCode::FORBIDDEN, why));
    }
    // Fail closed, like every decision: nothing is approved without its entry in the audit log.
    let entry = crate::audit::Entry {
        decision: if body.approve { "approved" } else { "refused" }.into(),
        subject: by.clone(),
        verb: "approve".into(),
        target: id.clone(),
        reason: format!(
            "{} may {} {}{}",
            view["from"].as_str().unwrap_or("?"),
            view["verb"].as_str().unwrap_or("?"),
            view["to"].as_str().unwrap_or("?"),
            if body.approve && window && person.is_some() { " (confirmed with a passkey)" } else { "" }
        ),
    };
    app.audit.record(&app.db, DEFAULT_TENANT, entry).await.map_err(|e| {
        tracing::error!(id, "the audit log can't be written, so refusing to decide a request: {e}");
        (StatusCode::SERVICE_UNAVAILABLE, "the control plane's audit log can't be written now".to_string())
    })?;
    let done =
        crate::approvals::decide(&app.db, DEFAULT_TENANT, &id, body.approve, hold, &by).await.map_err(internal)?;
    let Some(d) = done else {
        // Decided by someone else between the check and now: the entry above didn't take effect, and says so here.
        audit_refusal(
            &app,
            &by,
            &id,
            format!("request {id} was decided by someone else first; this decision didn't count"),
        )
        .await;
        return Err((StatusCode::NOT_FOUND, format!("no pending request {id}")));
    };
    // A window opens at once, rather than at the loop's next pass.
    if d.verb == "activate"
        && let Err(e) = crate::windows::tick(&app).await
    {
        tracing::warn!("opening a window just approved: {e}");
    }
    Ok(Json(json!({"id": id, "approved": body.approve, "from": d.subject, "verb": d.verb, "to": d.target})))
}

/// `GET /v1/windows`: the windowed roles and their state, and the recent windows (`docs/access-control.md`).
pub async fn get_windows(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    guard_approvals(&app, &headers).await?;
    let roles = match app.authz.policy(&app.db, DEFAULT_TENANT).await.map_err(internal)? {
        Some(p) => crate::windows::list_for(&app.db, DEFAULT_TENANT, &p, None).await.map_err(internal)?,
        None => vec![],
    };
    let recent = crate::windows::history(&app.db, DEFAULT_TENANT, 50).await.map_err(internal)?;
    Ok(Json(json!({"roles": roles, "windows": recent})))
}

/// `DELETE /v1/windows/<role>` (admin or approve token): close a role's window now. `{closed}`.
pub async fn delete_window(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    axum::extract::Path(role): axum::extract::Path<String>,
) -> ApiResult {
    let Approver { by, .. } = approver(&app, &headers, "closing a window", &format!("window:{role}")).await?;
    let closed = crate::windows::deactivate(&app, DEFAULT_TENANT, &role, &by).await.map_err(internal)?;
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
    async fn readings_and_tools_need_a_read_token_and_filter_by_kind() {
        let app = app().await;
        sqlx::query(
            "INSERT INTO readings (tenant, kind, key, node, at, data) VALUES
             ('default', 'node.status', 'laptop', 'laptop', 1, '{\"healthy\":true}'),
             ('default', 'claude.usage', 'a1', 'hub', 2, '{\"sevenDay\":40}'),
             ('other', 'node.status', 'x', 'x', 3, '{}')",
        )
        .execute(&app.db)
        .await
        .unwrap();
        app.links.tools.lock().await.insert(
            ("default".into(), "laptop".into()),
            vec![fleetlayer_protocol::link::ToolInfo { name: "browser".into(), ..Default::default() }],
        );
        let none = HeaderMap::new();
        let q = |k: Option<&str>| Query(ReadingsQuery { kind: k.map(str::to_string) });
        assert_eq!(
            get_readings(State(app.clone()), none.clone(), q(None)).await.unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(get_tools(State(app.clone()), none).await.unwrap_err().0, StatusCode::UNAUTHORIZED);

        let read = db::add_token(&app.db, DEFAULT_TENANT, "dashboard", "read").await.unwrap();
        let mut h = HeaderMap::new();
        h.insert("authorization", format!("Bearer {read}").parse().unwrap());
        let Json(all) = get_readings(State(app.clone()), h.clone(), q(None)).await.unwrap();
        assert_eq!(all["readings"].as_array().unwrap().len(), 2, "every kind, this tenant only: {all}");
        let Json(some) = get_readings(State(app.clone()), h.clone(), q(Some("node.status"))).await.unwrap();
        assert_eq!(some["readings"][0]["data"]["healthy"], true);
        assert_eq!(some["readings"].as_array().unwrap().len(), 1);
        let Json(tools) = get_tools(State(app.clone()), h).await.unwrap();
        assert_eq!(tools["tools"][0]["name"], "browser");
        assert_eq!(
            (tools["tools"][0]["node"].clone(), tools["tools"][0]["online"].clone()),
            (json!("laptop"), json!(false))
        );
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

    fn bearer(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("authorization", format!("Bearer {token}").parse().unwrap());
        h
    }

    async fn pending(app: &App, id: &str, verb: &str, target: &str) {
        sqlx::query(
            "INSERT INTO approvals (tenant, id, subject, verb, target, status, hold_ms, created_at)
             VALUES ('default', ?, 'ops@admin', ?, ?, 'pending', 3600000, 0)",
        )
        .bind(id)
        .bind(verb)
        .bind(target)
        .execute(&app.db)
        .await
        .unwrap();
    }

    fn body(approve: bool) -> Json<ApprovalBody> {
        Json(ApprovalBody { approve, hold: None })
    }

    #[tokio::test]
    async fn a_delegate_token_acts_as_the_person_it_names_and_windows_still_need_their_passkey() {
        let app = app().await;
        let db = &app.db;
        let mut ids = vec![];
        for (handle, role) in [("sam", "admin"), ("lee", "member")] {
            let i = crate::people::NewInvite { handle, role, teams: &[], person: None };
            let code = crate::people::invite(db, DEFAULT_TENANT, i, "admin").await.unwrap();
            let r = crate::people::invite_redeem(db, &code, crate::people::Redeemer::New { display_name: handle });
            ids.push(r.await.unwrap().person);
        }
        let (sam, lee) = (ids[0].clone(), ids[1].clone());
        let token = db::add_token(db, DEFAULT_TENANT, "dashboard", "delegate").await.unwrap();
        let h = bearer(&token);
        let with = |person: &str| {
            let mut h = h.clone();
            h.insert(PERSON_HEADER, person.parse().unwrap());
            h
        };
        // It reads like a read token, and says what it is.
        assert!(get_fleet(State(app.clone()), h.clone()).await.is_ok());
        assert!(get_approvals(State(app.clone()), h.clone()).await.is_ok());
        let Json(t) = get_token(State(app.clone()), h.clone()).await.unwrap();
        assert_eq!(t, json!({"scope": "delegate", "name": "dashboard"}));
        // Deciding needs a named person, who is one of the tenant's admins; it's recorded as theirs.
        pending(&app, "ap_tool", "use_tool", "tool:browser@laptop").await;
        let path = |id: &str| axum::extract::Path(id.to_string());
        let e = post_approval(State(app.clone()), h.clone(), path("ap_tool"), body(true)).await.unwrap_err();
        assert!(e.0 == StatusCode::FORBIDDEN && e.1.contains("Fleetlayer-Person"), "{}", e.1);
        let e = post_approval(State(app.clone()), with(&lee), path("ap_tool"), body(true)).await.unwrap_err();
        assert!(e.0 == StatusCode::FORBIDDEN && e.1.contains("isn't one of the tenant's admins"), "{}", e.1);
        let _ = post_approval(State(app.clone()), with(&sam), path("ap_tool"), body(true)).await.unwrap();
        let by: String =
            sqlx::query_scalar("SELECT decided_by FROM approvals WHERE id = 'ap_tool'").fetch_one(db).await.unwrap();
        assert_eq!(by, "person:sam (via dashboard)");
        // A window: only after that person's own passkey step-up.
        pending(&app, "ap_win", "activate", "window:admin").await;
        let e = post_approval(State(app.clone()), with(&sam), path("ap_win"), body(true)).await.unwrap_err();
        assert!(e.0 == StatusCode::FORBIDDEN && e.1.contains("passkey"), "{}", e.1);
        crate::stepup::record(db, DEFAULT_TENANT, "ap_win", &sam, "signin", 3_600_000).await.unwrap();
        let _ = post_approval(State(app.clone()), with(&sam), path("ap_win"), body(true)).await.unwrap();
        // Closing a window is theirs too.
        let e = delete_window(State(app.clone()), with(&lee), path("admin")).await.unwrap_err();
        assert_eq!(e.0, StatusCode::FORBIDDEN);
        let refused: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE decision = 'denied'").fetch_one(db).await.unwrap();
        assert!(refused >= 3, "refusals are audited");
    }

    #[tokio::test]
    async fn an_approve_token_decides_as_its_owner_and_opens_windows_only_after_a_passkey() {
        let app = app().await;
        let db = &app.db;
        let i = crate::people::NewInvite { handle: "sam", role: "admin", teams: &[], person: None };
        let code = crate::people::invite(db, DEFAULT_TENANT, i, "admin").await.unwrap();
        let sam = crate::people::invite_redeem(db, &code, crate::people::Redeemer::New { display_name: "Sam" })
            .await
            .unwrap()
            .person;
        let deck = db::add_owned_token(db, DEFAULT_TENANT, "deck", "approve", Some(&sam)).await.unwrap();
        let h = bearer(&deck);
        // It reads approvals and windows, and nothing else.
        assert!(get_approvals(State(app.clone()), h.clone()).await.is_ok());
        assert!(get_windows(State(app.clone()), h.clone()).await.is_ok());
        assert_eq!(get_fleet(State(app.clone()), h.clone()).await.unwrap_err().0, StatusCode::FORBIDDEN);
        assert_eq!(get_events(State(app.clone()), h.clone()).await.unwrap_err().0, StatusCode::FORBIDDEN);
        // An ordinary request: approved as its owner, and audited.
        pending(&app, "ap_tool", "use_tool", "tool:browser@laptop").await;
        let _ = post_approval(State(app.clone()), h.clone(), axum::extract::Path("ap_tool".into()), body(true))
            .await
            .unwrap();
        let by: String =
            sqlx::query_scalar("SELECT decided_by FROM approvals WHERE id = 'ap_tool'").fetch_one(db).await.unwrap();
        assert_eq!(by, "person:sam (token deck)");
        let audited: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_log WHERE verb = 'approve' AND target = 'ap_tool' AND subject = 'person:sam (token deck)'",
        )
        .fetch_one(db)
        .await
        .unwrap();
        assert_eq!(audited, 1);
        // A window: refused without a passkey step-up, refusing needs none, and with one it's approved, once.
        pending(&app, "ap_win", "activate", "window:admin").await;
        let e = post_approval(State(app.clone()), h.clone(), axum::extract::Path("ap_win".into()), body(true))
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::FORBIDDEN);
        assert!(e.1.contains("passkey"), "{}", e.1);
        crate::stepup::record(db, DEFAULT_TENANT, "ap_win", "p_kim", "signin", 3_600_000).await.unwrap();
        assert!(
            post_approval(State(app.clone()), h.clone(), axum::extract::Path("ap_win".into()), body(true))
                .await
                .is_err(),
            "someone else's passkey doesn't count"
        );
        crate::stepup::record(db, DEFAULT_TENANT, "ap_win", &sam, "signin", 3_600_000).await.unwrap();
        // A longer `for` is capped at the request's own hold, which the step-up page showed, so it's covered.
        let long = Json(ApprovalBody { approve: true, hold: Some("2h".into()) });
        let _ = post_approval(State(app.clone()), h.clone(), axum::extract::Path("ap_win".into()), long).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT hold_ms FROM approvals WHERE id = 'ap_win'")
                .fetch_one(db)
                .await
                .unwrap(),
            3_600_000
        );
        // The admin token needs no step-up; it's the operator's.
        pending(&app, "ap_win2", "activate", "window:admin").await;
        let _ =
            post_approval(State(app.clone()), bearer("admin-token"), axum::extract::Path("ap_win2".into()), body(true))
                .await
                .unwrap();
        // Refusing a window needs no passkey.
        pending(&app, "ap_win3", "activate", "window:admin").await;
        let _ = post_approval(State(app.clone()), h.clone(), axum::extract::Path("ap_win3".into()), body(false))
            .await
            .unwrap();
        // An owner who's no longer an admin can't decide anything with it.
        let i = crate::people::NewInvite { handle: "kim", role: "admin", teams: &[], person: None };
        let code = crate::people::invite(db, DEFAULT_TENANT, i, "admin").await.unwrap();
        crate::people::invite_redeem(db, &code, crate::people::Redeemer::New { display_name: "Kim" }).await.unwrap();
        crate::people::member_role(db, DEFAULT_TENANT, "sam", "member", "admin").await.unwrap();
        pending(&app, "ap_tool2", "use_tool", "tool:browser@laptop").await;
        let e = post_approval(State(app.clone()), h.clone(), axum::extract::Path("ap_tool2".into()), body(true))
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::FORBIDDEN);
        assert!(e.1.contains("isn't one of the tenant's admins"), "{}", e.1);
        // Refused attempts are in the audit log.
        // (repeats of one refusal within a minute are one entry, with a count)
        let refused: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT target FROM audit_log WHERE verb = 'approve' AND decision = 'denied' ORDER BY target",
        )
        .fetch_all(db)
        .await
        .unwrap();
        assert_eq!(refused, ["ap_tool2", "ap_win"], "without a passkey, and by an owner who isn't an admin");
        // A read token decides nothing.
        let read = db::add_token(db, DEFAULT_TENANT, "reader", "read").await.unwrap();
        assert_eq!(
            post_approval(State(app.clone()), bearer(&read), axum::extract::Path("ap_tool2".into()), body(true))
                .await
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn an_approve_token_grants_no_longer_than_asked_and_a_new_person_with_an_old_handle_inherits_nothing() {
        let app = app().await;
        let db = &app.db;
        let join = |handle: &'static str| async move {
            let i = crate::people::NewInvite { handle, role: "admin", teams: &[], person: None };
            let code = crate::people::invite(db, DEFAULT_TENANT, i, "admin").await.unwrap();
            crate::people::invite_redeem(db, &code, crate::people::Redeemer::New { display_name: handle })
                .await
                .unwrap()
                .person
        };
        let sam = join("sam").await;
        join("kim").await;
        let deck = db::add_owned_token(db, DEFAULT_TENANT, "deck", "approve", Some(&sam)).await.unwrap();
        let h = bearer(&deck);
        // A `for: once` ask stays once, and a short one stays short, whatever `for` the deck sends.
        sqlx::query(
            "INSERT INTO approvals (tenant, id, subject, verb, target, status, hold_ms, created_at)
             VALUES ('default', 'ap_once', 'x@laptop', 'use_tool', 'tool:db@server', 'pending', 0, 0),
                    ('default', 'ap_10m', 'x@laptop', 'start', '-@server', 'pending', 600000, 0)",
        )
        .execute(db)
        .await
        .unwrap();
        for id in ["ap_once", "ap_10m"] {
            let b = Json(ApprovalBody { approve: true, hold: Some("3650d".into()) });
            let _ = post_approval(State(app.clone()), h.clone(), axum::extract::Path(id.into()), b).await.unwrap();
        }
        let holds: Vec<i64> =
            sqlx::query_scalar("SELECT hold_ms FROM approvals WHERE id IN ('ap_once', 'ap_10m') ORDER BY id")
                .fetch_all(db)
                .await
                .unwrap();
        assert_eq!(holds, [600_000, crate::policy::ONCE]);
        // A token is its owner's by person ID: one whose owner has gone is refused even though an admin with the same
        // handle is there (handles stay reserved after someone leaves, and IDs make sure).
        crate::people::member_remove(db, DEFAULT_TENANT, "sam", "admin").await.unwrap();
        let gone = db::add_owned_token(db, DEFAULT_TENANT, "old", "approve", Some(&sam)).await.unwrap();
        let by_handle = db::add_owned_token(db, DEFAULT_TENANT, "handle", "approve", Some("person:kim")).await.unwrap();
        pending(&app, "ap_tool", "use_tool", "tool:browser@laptop").await;
        let e =
            post_approval(State(app.clone()), bearer(&by_handle), axum::extract::Path("ap_tool".into()), body(true))
                .await
                .unwrap_err();
        assert!(e.1.contains("isn't a member"), "an owner is a person ID, never a handle: {}", e.1);
        let h = bearer(&gone);
        let e = post_approval(State(app.clone()), h.clone(), axum::extract::Path("ap_tool".into()), body(true))
            .await
            .unwrap_err();
        assert_eq!(e.0, StatusCode::FORBIDDEN);
        assert!(e.1.contains("isn't a member"), "{}", e.1);
    }
}
