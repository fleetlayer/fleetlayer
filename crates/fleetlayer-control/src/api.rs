//! The control plane's API, for programs that read the fleet (the dashboard, the web and phone apps, Quietdeck's deck):
//! `GET /v1/fleet`, `/v1/handoffs`, `/v1/events`, `/v1/subscriptions`, `/v1/endpoints`, `/v1/summaries`, `/v1/readings`, `/v1/tools`,
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
    let mut sessions: Vec<Value> = db::sessions(&app.db, tenant)
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
    crate::handoffs::link_sessions(&mut sessions, &crate::handoffs::recent(&app.db, tenant, 1000).await?);
    Ok(json!({"nodes": nodes, "sessions": sessions}))
}

pub async fn get_fleet(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    guard(&app, &headers).await?;
    Ok(Json(fleet(&app, DEFAULT_TENANT).await.map_err(internal)?))
}

#[derive(serde::Deserialize)]
pub struct HandoffQuery {
    since: Option<String>,
    limit: Option<i64>,
}

/// `GET /v1/handoffs`: the newest 200 handoffs (without their notes). `?since=<cursor>` is the feed of those written
/// after it, oldest first, with `nextCursor` (`?since=` with nothing starts from the first).
pub async fn get_handoffs(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<HandoffQuery>,
) -> ApiResult {
    guard(&app, &headers).await?;
    if let Some(since) = &q.since {
        let limit = q.limit.unwrap_or(500).clamp(1, 5000);
        let (handoffs, next) = crate::handoffs::feed(&app.db, DEFAULT_TENANT, since, limit)
            .await
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("since: {e}")))?;
        return Ok(Json(json!({"handoffs": handoffs, "nextCursor": next})));
    }
    let handoffs = crate::handoffs::recent(&app.db, DEFAULT_TENANT, q.limit.unwrap_or(200).clamp(1, 1000))
        .await
        .map_err(internal)?;
    Ok(Json(json!({"handoffs": handoffs})))
}

/// `GET /v1/handoffs/<id>`: one handoff. Its note (the old session's own words) comes only with the admin token: a
/// read or delegate token can't be checked for `read` on the session it came from, so it gets the handoff without it.
pub async fn get_handoff(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult {
    guard(&app, &headers).await?;
    let admin = matches!(scope_of(&app, &headers).await, Some(Scope::Admin));
    match crate::handoffs::by_id(&app.db, DEFAULT_TENANT, &id).await.map_err(internal)? {
        Some(h) if admin => Ok(Json(json!(h))),
        Some(h) => Ok(Json(json!(fleetlayer_protocol::link::Handoff { note: None, ..h }))),
        None => Err((StatusCode::NOT_FOUND, format!("there's no handoff {id}"))),
    }
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

/// `GET /v1/token`: what the request's token is, so a service can show only what it may do, and which tenant it
/// belongs to, so a service people sign in to can turn away someone signed in to another. `{scope, name, tenant,
/// tenantSlug}` (`name` null for the admin token file's). Any token.
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
    let slug: Option<String> = sqlx::query_scalar("SELECT slug FROM tenants WHERE id = ?")
        .bind(DEFAULT_TENANT)
        .fetch_optional(&app.db)
        .await
        .map_err(|e| internal(e.into()))?;
    Ok(Json(json!({"scope": scope, "name": name, "tenant": DEFAULT_TENANT, "tenantSlug": slug})))
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

/// Who reads or changes the tenant's people with this request.
struct PeopleActor {
    /// As changes are recorded: `admin token`, or `person:<handle> (via <token>)`.
    by: String,
    /// The person a delegate token acts for (their ID); None for the admin token.
    person: Option<String>,
    /// One of the tenant's admins (the admin token counts as one).
    tenant_admin: bool,
}

/// Who reads the tenant's people with this request: the admin token, or a delegate token for the signed-in person it
/// names, when they're one of the tenant's admins or the policy allows them `admin` on `service:dashboard` (which is for
/// using the dashboard: they see people, but only the tenant's admins change them, see `post_people`).
async fn people_actor(app: &App, headers: &HeaderMap, target: &str) -> Result<PeopleActor, (StatusCode, String)> {
    match scope_of(app, headers).await {
        Some(Scope::Admin) => Ok(PeopleActor { by: "admin token".into(), person: None, tenant_admin: true }),
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
                return Err((
                    StatusCode::FORBIDDEN,
                    "a delegate token acts for a person: send Fleetlayer-Person".into(),
                ));
            };
            let fleet = app.authz.fleet(&app.db).await.map_err(internal)?;
            let tenant = fleet.tenant(DEFAULT_TENANT).ok_or((StatusCode::NOT_FOUND, "no tenant".to_string()))?;
            let Some(handle) = tenant.handles.get(person).cloned() else {
                return Err((StatusCode::FORBIDDEN, format!("{person} isn't a member of the tenant")));
            };
            let by = format!("person:{handle} (via {name})");
            let actor = |tenant_admin| PeopleActor { by: by.clone(), person: Some(person.to_string()), tenant_admin };
            if tenant.people.get(&handle).is_some_and(|p| p.role == "admin") {
                return Ok(actor(true));
            }
            let ask = fleetlayer_protocol::link::AccessCheck {
                tenant: tenant.id.clone(),
                who: person.to_string(),
                can: "admin".into(),
                to: "service:dashboard".into(),
                ..Default::default()
            };
            let d = crate::authz::check_as(app, &fleet, tenant, &ask)
                .await
                .map_err(|e| (StatusCode::BAD_REQUEST, e.message))?;
            if d.allowed {
                Ok(actor(false))
            } else {
                let why = format!("person:{handle} may not manage the tenant's people: {}", d.reason);
                audit_refusal(app, &by, target, why.clone()).await;
                Err((StatusCode::FORBIDDEN, why))
            }
        }
        Some(_) => Err((StatusCode::FORBIDDEN, "managing people needs an admin or delegate token".into())),
        None => Err((StatusCode::UNAUTHORIZED, "managing people needs an admin or delegate token".into())),
    }
}

/// `GET /v1/people`: the tenant's members, pending invites and teams (admin token, or a delegate token for one of the
/// tenant's admins). `{tenant, slug, name, members, invites, teams}`.
pub async fn get_people(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    people_actor(&app, &headers, "people").await?;
    let (slug, name): (String, String) = sqlx::query_as("SELECT slug, name FROM tenants WHERE id = ?")
        .bind(DEFAULT_TENANT)
        .fetch_one(&app.db)
        .await
        .map_err(|e| internal(e.into()))?;
    let members = crate::people::members(&app.db, DEFAULT_TENANT).await.map_err(internal)?;
    let invites = crate::people::invites(&app.db, DEFAULT_TENANT).await.map_err(internal)?;
    let teams = crate::people::teams(&app.db, DEFAULT_TENANT).await.map_err(internal)?;
    Ok(Json(
        json!({"tenant": DEFAULT_TENANT, "slug": slug, "name": name, "members": members, "invites": invites, "teams": teams}),
    ))
}

/// `POST /v1/people/<action>` (`invite`, `invite_revoke`, `member_role`, `member_remove`, `team_add`, `team_remove`,
/// `team_member`) with the change's fields: made with the same functions as the command line and recorded as the
/// person who made it. An invite answers `{code}`.
///
/// Every change grants or takes away rights, so: only the tenant's admins (or the admin token) make them, and through a
/// delegate token only when that person used their passkey in the last ten minutes, as the sign-in service reports
/// over its own token (`POST /v1/signin/passkey`): a session left open in a browser can't. Refused and failed attempts
/// are audited, as well as the changes themselves.
pub async fn post_people(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    axum::extract::Path(action): axum::extract::Path<String>,
    Json(c): Json<crate::people::Change>,
) -> ApiResult {
    let target = format!("people/{action}");
    let actor = people_actor(&app, &headers, &target).await?;
    if !actor.tenant_admin {
        let why = "only the tenant's admins change its people".to_string();
        audit_refusal(&app, &actor.by, &target, why.clone()).await;
        return Err((StatusCode::FORBIDDEN, why));
    }
    if let Some(p) = &actor.person
        && !crate::people::passkey_fresh(&app.db, p).await.map_err(internal)?
    {
        let why = "changing people needs your passkey from the last ten minutes: sign in again".to_string();
        audit_refusal(&app, &actor.by, &target, why.clone()).await;
        return Err((StatusCode::FORBIDDEN, why));
    }
    match crate::people::apply(&app.db, DEFAULT_TENANT, &action, &c, &actor.by).await {
        Ok(v) => Ok(Json(v)),
        Err(crate::people::ApplyError::NoSuchAction) => Err((StatusCode::NOT_FOUND, format!("no action {action}"))),
        Err(crate::people::ApplyError::Bad(m)) => {
            audit_refusal(&app, &actor.by, &target, format!("failed: {m}")).await;
            Err((StatusCode::BAD_REQUEST, m))
        }
    }
}

/// `GET /v1/links`: links to web services the fleet uses but Fleetlayer doesn't run, for the dashboard's Services page.
/// `{links: [{name, url, description, addedBy, addedAt}]}`. A read token.
pub async fn get_links(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    guard(&app, &headers).await?;
    let rows: Vec<(String, String, Option<String>, String, i64)> = sqlx::query_as(
        "SELECT name, url, description, added_by, added_at FROM service_links WHERE tenant = ? ORDER BY name",
    )
    .bind(DEFAULT_TENANT)
    .fetch_all(&app.db)
    .await
    .map_err(|e| internal(e.into()))?;
    let links: Vec<Value> = rows
        .into_iter()
        .map(|(name, url, description, by, at)| {
            json!({"name": name, "url": url, "description": description, "addedBy": by, "addedAt": at})
        })
        .collect();
    Ok(Json(json!({"links": links})))
}

#[derive(Debug, serde::Deserialize)]
pub struct NewLink {
    name: String,
    url: String,
    #[serde(default)]
    description: Option<String>,
}

/// What's wrong with a link, if anything: a name the policy can use (`service:<name>`), an `https://` (or `http://`)
/// address, a one-line description.
fn link_problem(l: &NewLink) -> Option<String> {
    let name_ok = !l.name.is_empty()
        && l.name.len() <= 63
        && l.name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !l.name.starts_with('-');
    if !name_ok {
        return Some(
            "a link's name is lower-case letters, digits and hyphens (the policy calls it service:<name>)".into(),
        );
    }
    let url_ok = (l.url.starts_with("https://") || l.url.starts_with("http://"))
        && l.url.len() <= 2000
        && !l.url.chars().any(|c| c.is_whitespace() || c.is_control());
    if !url_ok {
        return Some("a link's address starts with https:// (or http://)".into());
    }
    if l.description.as_deref().is_some_and(|d| d.chars().count() > 200 || d.contains('\n')) {
        return Some("a description is one line of at most 200 characters".into());
    }
    None
}

/// `POST /v1/links {name, url, description?}`: add (or replace) a link, as one of the tenant's admins (or the admin
/// token). Recorded in the audit log.
pub async fn post_link(State(app): State<Arc<App>>, headers: HeaderMap, Json(l): Json<NewLink>) -> ApiResult {
    let actor = people_actor(&app, &headers, "links").await?;
    if !actor.tenant_admin {
        let why = "only the tenant's admins add links".to_string();
        audit_refusal(&app, &actor.by, "links", why.clone()).await;
        return Err((StatusCode::FORBIDDEN, why));
    }
    if let Some(why) = link_problem(&l) {
        return Err((StatusCode::BAD_REQUEST, why));
    }
    sqlx::query(
        "INSERT INTO service_links (tenant, name, url, description, added_by, added_at) VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT (tenant, name) DO UPDATE SET url = excluded.url, description = excluded.description,
         added_by = excluded.added_by, added_at = excluded.added_at",
    )
    .bind(DEFAULT_TENANT)
    .bind(&l.name)
    .bind(&l.url)
    .bind(l.description.as_deref().filter(|d| !d.trim().is_empty()))
    .bind(&actor.by)
    .bind(db::now_ms())
    .execute(&app.db)
    .await
    .map_err(|e| internal(e.into()))?;
    crate::people::log(&app.db, DEFAULT_TENANT, &actor.by, "link.add", &format!("service:{}", l.name), &l.url)
        .await
        .map_err(internal)?;
    Ok(Json(json!({"name": l.name})))
}

/// `DELETE /v1/links/<name>`: remove a link, as one of the tenant's admins. `{removed}`.
pub async fn delete_link(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> ApiResult {
    let actor = people_actor(&app, &headers, "links").await?;
    if !actor.tenant_admin {
        return Err((StatusCode::FORBIDDEN, "only the tenant's admins remove links".into()));
    }
    let n = sqlx::query("DELETE FROM service_links WHERE tenant = ? AND name = ?")
        .bind(DEFAULT_TENANT)
        .bind(&name)
        .execute(&app.db)
        .await
        .map_err(|e| internal(e.into()))?
        .rows_affected();
    if n > 0 {
        crate::people::log(&app.db, DEFAULT_TENANT, &actor.by, "link.remove", &format!("service:{name}"), "removed")
            .await
            .map_err(internal)?;
    }
    Ok(Json(json!({"removed": n > 0})))
}

/// `GET /v1/policy` (admin token, or a delegate token for one of the tenant's admins): the access policy in force, as
/// its file says it and parsed. `{text, builtin, policy}`; `text` null when the tenant has none (the older ACL rules).
pub async fn get_policy(State(app): State<Arc<App>>, headers: HeaderMap) -> ApiResult {
    people_actor(&app, &headers, "policy").await?;
    let Some((text, builtin)) = crate::authz::effective(&app.db, DEFAULT_TENANT).await.map_err(internal)? else {
        return Ok(Json(json!({"text": null, "builtin": false, "policy": null})));
    };
    let parsed = app.authz.policy(&app.db, DEFAULT_TENANT).await.map_err(internal)?;
    let policy = parsed.map(|c| serde_json::to_value(&c.file).unwrap_or_default());
    Ok(Json(json!({"text": text, "builtin": builtin, "policy": policy})))
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

/// An RPC refusal as an HTTP answer: the status, and the error as JSON.
fn rpc_status(e: fleetlayer_protocol::rpc::RpcError) -> (StatusCode, String) {
    use fleetlayer_protocol::rpc::code;
    let status = match e.code {
        code::REFUSED => StatusCode::FORBIDDEN,
        code::NO_SUCH => StatusCode::NOT_FOUND,
        code::BUSY | code::PROMPT_GONE => StatusCode::CONFLICT,
        code::NOT_HERE => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_REQUEST,
    };
    (status, serde_json::to_string(&e).unwrap_or(e.message))
}

#[derive(Debug, serde::Deserialize)]
pub struct PostSessionAct {
    /// `<tab>@<node>`.
    to: String,
    #[serde(flatten)]
    action: fleetlayer_protocol::link::SessionAction,
    #[serde(default)]
    why: Option<String>,
}

/// `POST /v1/sessions/act` `{to, action: "close" | "answer" | "command", …, why?}`: a service closes a session,
/// answers its blocked prompt (`prompt`, `answers`) or types an allowlisted slash command into it (`command`), as
/// `service:<token name>`. Needs a `send` token; the access policy decides (`close`, `answer`, `command`, asked by
/// default), and what's typed is audited. Answers the node's `SessionActed`. Never `say`: see `POST /v1/sessions/say`.
pub async fn post_session_act(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(a): Json<PostSessionAct>,
) -> ApiResult {
    let token = bearer(&headers).ok_or((StatusCode::UNAUTHORIZED, "a send token is needed".to_string()))?;
    let (tenant, scope, name) = db::token_info(&app.db, token)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::UNAUTHORIZED, "a send token is needed".to_string()))?;
    if scope != "send" {
        return Err((StatusCode::FORBIDDEN, "only a send token acts on sessions as a service".into()));
    }
    let done = crate::route::act_as_service(&app, &tenant, &name, &a.to, a.action, a.why.as_deref())
        .await
        .map_err(rpc_status)?;
    Ok(Json(serde_json::to_value(done).unwrap_or_default()))
}

#[derive(Debug, serde::Deserialize)]
pub struct PostSay {
    /// `<tab>@<node>`.
    to: String,
    text: String,
    /// Where the person said it, put before the text (`From the Work tab: …`); default: the token's name.
    #[serde(default)]
    from: Option<String>,
}

/// `POST /v1/sessions/say` `{to, text, from?}`: a signed-in person says something in a session, typed in and
/// submitted as their own input while it's idle at an empty prompt. Only a delegate token naming the person in
/// `Fleetlayer-Person` (a service people sign in to, acting for them), checked as that person's `attach` on the
/// session and audited as them with the exact text: no service or agent says anything on its own.
pub async fn post_say(State(app): State<Arc<App>>, headers: HeaderMap, Json(s): Json<PostSay>) -> ApiResult {
    let token = bearer(&headers).ok_or((StatusCode::UNAUTHORIZED, "a delegate token is needed".to_string()))?;
    let (tenant, scope, name) = db::token_info(&app.db, token)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::UNAUTHORIZED, "a delegate token is needed".to_string()))?;
    if scope != "delegate" || tenant != DEFAULT_TENANT {
        return Err((
            StatusCode::FORBIDDEN,
            "only a person says something in a session: a delegate token acting for them is needed".into(),
        ));
    }
    let Some(person) = headers.get(PERSON_HEADER).and_then(|v| v.to_str().ok()).filter(|p| !p.is_empty()) else {
        return Err((StatusCode::FORBIDDEN, "a delegate token acts for a person: send Fleetlayer-Person".into()));
    };
    let fleet = app.authz.fleet(&app.db).await.map_err(internal)?;
    let t = fleet.tenant(DEFAULT_TENANT).ok_or((StatusCode::NOT_FOUND, "no tenant".to_string()))?;
    let Some(handle) = t.handles.get(person).cloned() else {
        return Err((StatusCode::FORBIDDEN, format!("{person} isn't a member of the tenant")));
    };
    let from: String = s.from.unwrap_or_default().chars().filter(|c| !c.is_control()).take(60).collect();
    let prefix = format!("From {}: ", if from.trim().is_empty() { name.as_str() } else { from.trim() });
    let done = crate::route::say_as_person(&app, &tenant, &format!("person:{handle}"), &s.to, &s.text, &prefix)
        .await
        .map_err(rpc_status)?;
    Ok(Json(serde_json::to_value(done).unwrap_or_default()))
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get("authorization")?.to_str().ok()?.strip_prefix("Bearer ")
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

    /// A handoff's note is the old session's own words: over the API only the admin token reads it; a read or
    /// delegate token gets the handoff without it.
    #[tokio::test]
    async fn a_handoff_note_is_only_for_the_admin_token() {
        let app = app().await;
        sqlx::query(
            "INSERT INTO handoffs (tenant, id, from_address, to_address, note, by_address, created_at)
             VALUES ('default', 'h00000000000000aa', 'billing@laptop', 'billing@home', 'the key is in vault x', 'billing@laptop', ?)",
        )
        .bind(db::now_ms())
        .execute(&app.db)
        .await
        .unwrap();
        let id = || axum::extract::Path("h00000000000000aa".to_string());
        let read = db::add_token(&app.db, DEFAULT_TENANT, "dashboard", "read").await.unwrap();
        let mut h = HeaderMap::new();
        h.insert("authorization", format!("Bearer {read}").parse().unwrap());
        let Json(v) = get_handoff(State(app.clone()), h, id()).await.unwrap();
        assert_eq!((v["from"].as_str(), v.get("note")), (Some("billing@laptop"), None), "{v}");
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer admin-token".parse().unwrap());
        let Json(v) = get_handoff(State(app.clone()), h, id()).await.unwrap();
        assert_eq!(v["note"], "the key is in vault x");
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
        assert_eq!(
            (t["scope"].clone(), t["name"].clone(), t["tenant"].clone()),
            (json!("delegate"), json!("dashboard"), json!("default"))
        );
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
    async fn people_are_managed_by_the_tenant_s_admins_through_the_dashboard_and_recorded_as_them() {
        let app = app().await;
        let db = &app.db;
        let mut ids = vec![];
        for (handle, role) in [("sam", "admin"), ("lee", "member")] {
            let i = crate::people::NewInvite { handle, role, teams: &[], person: None };
            let code = crate::people::invite(db, DEFAULT_TENANT, i, "admin").await.unwrap();
            let r = crate::people::invite_redeem(db, &code, crate::people::Redeemer::New { display_name: handle });
            ids.push(r.await.unwrap().person);
        }
        let token = db::add_token(db, DEFAULT_TENANT, "dashboard", "delegate").await.unwrap();
        let as_ = |person: &str| {
            let mut h = bearer(&token);
            h.insert(PERSON_HEADER, person.parse().unwrap());
            h
        };
        let change = |handle: &str| Json(crate::people::Change { handle: Some(handle.into()), ..Default::default() });
        let path = |a: &str| axum::extract::Path(a.to_string());
        // An admin reads them; changing them needs a passkey they used in the last ten minutes.
        let Json(v) = get_people(State(app.clone()), as_(&ids[0])).await.unwrap();
        assert_eq!(v["members"].as_array().unwrap().len(), 2, "{v}");
        let e = post_people(State(app.clone()), as_(&ids[0]), path("invite"), change("kim")).await.unwrap_err();
        assert!(e.0 == StatusCode::FORBIDDEN && e.1.contains("passkey"), "{}", e.1);
        crate::people::passkey_used(db, &ids[0]).await.unwrap();
        let Json(v) = post_people(State(app.clone()), as_(&ids[0]), path("invite"), change("kim")).await.unwrap();
        assert!(v["code"].as_str().is_some_and(|c| !c.is_empty()), "{v}");
        let by: String =
            sqlx::query_scalar("SELECT created_by FROM invites WHERE handle = 'kim'").fetch_one(db).await.unwrap();
        assert_eq!(by, "person:sam (via dashboard)");
        assert!(get_policy(State(app.clone()), as_(&ids[0])).await.is_ok());
        // The last admin can't be made a member; the change and the role it had are in the audit log.
        let demote = Json(crate::people::Change {
            handle: Some("sam".into()),
            role: Some("member".into()),
            ..Default::default()
        });
        let e = post_people(State(app.clone()), as_(&ids[0]), path("member_role"), demote).await.unwrap_err();
        assert!(e.1.contains("last admin"), "{}", e.1);
        let promote = Json(crate::people::Change {
            handle: Some("lee".into()),
            role: Some("admin".into()),
            ..Default::default()
        });
        let _ = post_people(State(app.clone()), as_(&ids[0]), path("member_role"), promote).await.unwrap();
        let reason: String = sqlx::query_scalar(
            "SELECT reason FROM audit_outbox WHERE verb = 'member.role' AND target = 'person:lee' ORDER BY rowid DESC LIMIT 1",
        )
        .fetch_one(db)
        .await
        .unwrap_or_default();
        assert_eq!(reason, "member -> admin");
        let back = Json(crate::people::Change {
            handle: Some("lee".into()),
            role: Some("member".into()),
            ..Default::default()
        });
        let _ = post_people(State(app.clone()), as_(&ids[0]), path("member_role"), back).await.unwrap();
        // Someone the policy lets use the dashboard as an admin (admin on service:dashboard) sees people but, not being
        // one of the tenant's admins, changes nothing, however fresh their passkey.
        crate::authz::store(
            db,
            DEFAULT_TENANT,
            "version: 2\ndefault: allow\nallow:\n  - { who: person:lee, can: admin, to: service:dashboard }\n",
        )
        .await
        .unwrap();
        crate::people::passkey_used(db, &ids[1]).await.unwrap();
        assert!(get_people(State(app.clone()), as_(&ids[1])).await.is_ok());
        let e = post_people(
            State(app.clone()),
            as_(&ids[1]),
            path("member_role"),
            Json(crate::people::Change {
                handle: Some("lee".into()),
                role: Some("admin".into()),
                ..Default::default()
            }),
        )
        .await
        .unwrap_err();
        assert!(e.0 == StatusCode::FORBIDDEN && e.1.contains("only the tenant's admins"), "{}", e.1);
        crate::authz::store(db, DEFAULT_TENANT, "version: 2\ndefault: deny\n").await.unwrap();
        // A member doesn't, nor a delegate token without a person, nor a read token.
        let e = post_people(State(app.clone()), as_(&ids[1]), path("invite"), change("x")).await.unwrap_err();
        assert_eq!(e.0, StatusCode::FORBIDDEN, "{}", e.1);
        assert_eq!(get_people(State(app.clone()), bearer(&token)).await.unwrap_err().0, StatusCode::FORBIDDEN);
        let read = db::add_token(db, DEFAULT_TENANT, "reader", "read").await.unwrap();
        assert_eq!(get_people(State(app.clone()), bearer(&read)).await.unwrap_err().0, StatusCode::FORBIDDEN);
        // The admin token does, and an unknown action is a 404.
        assert!(get_people(State(app.clone()), bearer("admin-token")).await.is_ok());
        let e = post_people(State(app.clone()), as_(&ids[0]), path("nope"), change("x")).await.unwrap_err();
        assert_eq!(e.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_tenant_s_admins_add_and_remove_links_and_read_tokens_list_them() {
        let app = app().await;
        let db = &app.db;
        let mut ids = vec![];
        for (handle, role) in [("sam", "admin"), ("lee", "member")] {
            let i = crate::people::NewInvite { handle, role, teams: &[], person: None };
            let code = crate::people::invite(db, DEFAULT_TENANT, i, "admin").await.unwrap();
            let r = crate::people::invite_redeem(db, &code, crate::people::Redeemer::New { display_name: handle });
            ids.push(r.await.unwrap().person);
        }
        let token = db::add_token(db, DEFAULT_TENANT, "dashboard", "delegate").await.unwrap();
        let as_ = |person: &str| {
            let mut h = bearer(&token);
            h.insert(PERSON_HEADER, person.parse().unwrap());
            h
        };
        let link = |name: &str, url: &str| {
            Json(NewLink { name: name.into(), url: url.into(), description: Some("The team wiki".into()) })
        };
        let _ = post_link(State(app.clone()), as_(&ids[0]), link("wiki", "https://wiki.example.com")).await.unwrap();
        let e = post_link(State(app.clone()), as_(&ids[1]), link("ci", "https://ci.example.com")).await.unwrap_err();
        assert_eq!(e.0, StatusCode::FORBIDDEN, "a member doesn't add links");
        let e = post_link(State(app.clone()), as_(&ids[0]), link("bad", "javascript:alert(1)")).await.unwrap_err();
        assert_eq!(e.0, StatusCode::BAD_REQUEST);
        let e = post_link(State(app.clone()), as_(&ids[0]), link("Not A Name", "https://x.example")).await.unwrap_err();
        assert_eq!(e.0, StatusCode::BAD_REQUEST);
        let read = db::add_token(db, DEFAULT_TENANT, "reader", "read").await.unwrap();
        let Json(v) = get_links(State(app.clone()), bearer(&read)).await.unwrap();
        assert_eq!(v["links"][0]["name"], "wiki");
        assert_eq!(v["links"][0]["addedBy"], "person:sam (via dashboard)");
        assert_eq!(v["links"].as_array().unwrap().len(), 1);
        let Json(v) = delete_link(State(app.clone()), as_(&ids[0]), axum::extract::Path("wiki".into())).await.unwrap();
        assert_eq!(v["removed"], true);
        let Json(v) = get_links(State(app.clone()), bearer(&read)).await.unwrap();
        assert_eq!(v["links"], json!([]));
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
