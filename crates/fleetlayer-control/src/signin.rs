//! What the sign-in service asks of the control plane (`docs/sso.md`, step 1c). The sign-in service holds every
//! credential (passkeys, upstream links, sessions, the token-signing key); the control plane holds who belongs where.
//! It reaches these routes with a sign-in token (`fleetlayer-control signin token`), separate from node and API
//! tokens, which may read memberships, redeem invites, ask checks and record sign-ins, for every tenant, and nothing
//! else. No other token reaches them.
//!
//! - `POST /v1/signin/person {person}` → the person and their tenants, with what the ID token says in each
//! - `POST /v1/signin/invite_redeem {code, display_name? | person?}` → `{tenant, person, handle, recovery}`
//! - `POST /v1/signin/check {tenant, who, can, to}` → `{allowed, rules, reason}`
//! - `POST /v1/signin/event {tenant, person, kind, client?, detail?}` → `{}`, into the tenant's audit log
//! - `POST /v1/signin/approval_view {tenant, approval}` → the approval, for the step-up page
//! - `POST /v1/signin/stepup {tenant, person, approval}` → `{approval, person, expiresAt}`: the person confirmed a
//!   role-window approval with their passkey (`stepup.rs`)
//! - `POST /v1/signin/admin/view {tenant, as}` and `/v1/signin/admin/<action> {tenant, as, …}` for the admin pages:
//!   the control plane checks `as` may administer the tenant (an admin, or `admin` on `service:signin`), and records
//!   each change as that person

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;

use crate::App;
use crate::db::{Db, hash, new_secret, now_ms};

/// Make (or replace) the sign-in token called `name`: returns it, shown once.
pub async fn token_add(db: &Db, name: &str) -> anyhow::Result<String> {
    anyhow::ensure!(crate::db::valid_name(name), "a token name is letters, digits, - and _");
    let token = format!("fls_{}", new_secret());
    sqlx::query(
        "INSERT INTO signin_tokens (name, token_hash, created_at) VALUES (?, ?, ?)
         ON CONFLICT (name) DO UPDATE SET token_hash = excluded.token_hash, created_at = excluded.created_at",
    )
    .bind(name)
    .bind(hash(&token))
    .bind(now_ms())
    .execute(db)
    .await?;
    Ok(token)
}

pub async fn token_remove(db: &Db, name: &str) -> anyhow::Result<bool> {
    let r = sqlx::query("DELETE FROM signin_tokens WHERE name = ?").bind(name).execute(db).await?;
    Ok(r.rows_affected() > 0)
}

pub async fn tokens(db: &Db) -> anyhow::Result<Vec<(String, i64)>> {
    let rows = sqlx::query("SELECT name, created_at FROM signin_tokens ORDER BY name").fetch_all(db).await?;
    Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
}

type ApiResult = Result<Json<Value>, (StatusCode, String)>;

fn bad(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, e.to_string())
}

fn internal(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// Only a sign-in token passes. Returns its name, which audit entries record as `signin:<name>`.
async fn guard(app: &App, headers: &HeaderMap) -> Result<String, (StatusCode, String)> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or((StatusCode::UNAUTHORIZED, "a sign-in token is needed".to_string()))?;
    let row = sqlx::query("SELECT name FROM signin_tokens WHERE token_hash = ?")
        .bind(hash(token))
        .fetch_optional(&app.db)
        .await
        .map_err(internal)?;
    row.map(|r| r.get::<String, _>(0)).ok_or((StatusCode::UNAUTHORIZED, "a sign-in token is needed".to_string()))
}

/// One of a person's tenants, as the ID token for it says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Membership {
    pub tenant: String,
    pub slug: String,
    pub name: String,
    /// `preferred_username`; `fleetlayer.dev/principal` is `person:<handle>`.
    pub handle: String,
    pub role: String,
    /// The teams they're in, directly or through a sub-team, without `everyone`.
    pub teams: Vec<String>,
    /// The flat `groups` claim: the tenant, `<tenant>/role:<role>`, and each team with its parents' path.
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PersonView {
    pub id: String,
    pub display_name: String,
    /// Set by an operator, so services may treat it as verified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub tenants: Vec<Membership>,
}

/// A person and every tenant they're a member of.
pub async fn person(db: &Db, id: &str) -> anyhow::Result<Option<PersonView>> {
    let Some(row) =
        sqlx::query("SELECT display_name, email FROM people WHERE id = ?").bind(id).fetch_optional(db).await?
    else {
        return Ok(None);
    };
    let fleet = crate::directory::Fleet::load(db).await?;
    let mut tenants = vec![];
    for t in &fleet.tenants {
        let Some(handle) = t.handles.get(id) else { continue };
        let Some(member) = t.people.get(handle) else { continue };
        let name: String =
            sqlx::query_scalar("SELECT name FROM tenants WHERE id = ?").bind(&t.id).fetch_one(db).await?;
        let teams: Vec<String> =
            t.teams_of(handle).into_iter().filter(|team| team != crate::people::EVERYONE).collect();
        let mut groups = vec![t.slug.clone(), format!("{}/role:{}", t.slug, member.role)];
        for team in &teams {
            // Engineering > Platform is `acme/engineering/platform`.
            let mut path = t.with_parents(team);
            path.reverse();
            groups.push(format!("{}/{}", t.slug, path.join("/")));
        }
        groups.sort();
        tenants.push(Membership {
            tenant: t.id.clone(),
            slug: t.slug.clone(),
            name,
            handle: handle.clone(),
            role: member.role.clone(),
            teams,
            groups,
        });
    }
    Ok(Some(PersonView { id: id.into(), display_name: row.get(0), email: row.get(1), tenants }))
}

#[derive(Deserialize)]
pub struct PersonQuery {
    person: String,
}

pub async fn post_person(State(app): State<Arc<App>>, headers: HeaderMap, Json(q): Json<PersonQuery>) -> ApiResult {
    guard(&app, &headers).await?;
    match person(&app.db, &q.person).await.map_err(internal)? {
        Some(p) => Ok(Json(serde_json::to_value(p).map_err(internal)?)),
        None => Err((StatusCode::NOT_FOUND, format!("no person {}", q.person))),
    }
}

#[derive(Deserialize)]
pub struct RedeemRequest {
    code: String,
    /// A new account: the name they gave.
    #[serde(default)]
    display_name: Option<String>,
    /// Someone already signed in: their person ID.
    #[serde(default)]
    person: Option<String>,
}

pub async fn post_invite_redeem(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(r): Json<RedeemRequest>,
) -> ApiResult {
    guard(&app, &headers).await?;
    let who = match (&r.display_name, &r.person) {
        (Some(n), None) => crate::people::Redeemer::New { display_name: n },
        (None, Some(p)) => crate::people::Redeemer::Existing { person: p },
        _ => return Err(bad("give display_name (a new account) or person (an existing one), not both")),
    };
    let done = crate::people::invite_redeem(&app.db, &r.code, who).await.map_err(bad)?;
    Ok(Json(json!({
        "tenant": done.tenant, "person": done.person, "handle": done.handle, "recovery": done.recovery
    })))
}

pub async fn post_check(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(req): Json<fleetlayer_protocol::link::AccessCheck>,
) -> ApiResult {
    guard(&app, &headers).await?;
    let fleet = app.authz.fleet(&app.db).await.map_err(internal)?;
    let tenant = fleet.tenant(&req.tenant).ok_or_else(|| bad(format!("no tenant {}", req.tenant)))?;
    let d = crate::authz::check_as(&app, &fleet, tenant, &req).await.map_err(|e| bad(e.message))?;
    Ok(Json(serde_json::to_value(d).map_err(internal)?))
}

/// What the sign-in service records: a sign-in, a sign-out, a refusal, a passkey added or removed.
const EVENTS: &[&str] = &["signed_in", "signed_out", "refused", "passkey_added", "passkey_removed", "upstream_linked"];

#[derive(Deserialize)]
pub struct EventRequest {
    tenant: String,
    person: String,
    kind: String,
    /// The client (`service:<name>`) it was for, if any.
    #[serde(default)]
    client: Option<String>,
    /// One line: how (passkey, Google), or why refused. Never a credential.
    #[serde(default)]
    detail: Option<String>,
}

pub async fn post_event(State(app): State<Arc<App>>, headers: HeaderMap, Json(e): Json<EventRequest>) -> ApiResult {
    let by = guard(&app, &headers).await?;
    if !EVENTS.contains(&e.kind.as_str()) {
        return Err(bad(format!("kind is one of {}", EVENTS.join(", "))));
    }
    let fleet = app.authz.fleet(&app.db).await.map_err(internal)?;
    let tenant = fleet.tenant(&e.tenant).ok_or_else(|| bad(format!("no tenant {}", e.tenant)))?;
    let subject = match tenant.handles.get(&e.person) {
        Some(h) => format!("person:{h}"),
        // Someone refused for not being a member has no handle here.
        None => e.person.clone(),
    };
    let detail = e.detail.unwrap_or_default();
    if detail.contains('\n') || detail.chars().count() > 300 {
        return Err(bad("detail is one line of at most 300 characters"));
    }
    let entry = crate::audit::Entry {
        decision: "recorded".into(),
        subject,
        verb: format!("signin.{}", e.kind),
        target: e.client.unwrap_or_else(|| "service:signin".into()),
        reason: if detail.is_empty() { format!("by signin:{by}") } else { format!("{detail} (by signin:{by})") },
    };
    app.audit.record(&app.db, &tenant.id, entry).await.map_err(internal)?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
pub struct ApprovalViewRequest {
    tenant: String,
    approval: String,
}

/// `POST /v1/signin/approval_view {tenant, approval}`: one approval, for the step-up page to show what's being
/// approved. 404 when there's none by that ID in the tenant.
pub async fn post_approval_view(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(r): Json<ApprovalViewRequest>,
) -> ApiResult {
    guard(&app, &headers).await?;
    let fleet = app.authz.fleet(&app.db).await.map_err(internal)?;
    let tenant = fleet.tenant(&r.tenant).ok_or((StatusCode::NOT_FOUND, format!("no tenant {}", r.tenant)))?;
    let v = crate::stepup::view(&app.db, &tenant.id, &r.approval)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, format!("no request {}", r.approval)))?;
    Ok(Json(v))
}

#[derive(Deserialize)]
pub struct StepupRequest {
    tenant: String,
    /// The person's ID (the sign-in's `sub`).
    person: String,
    approval: String,
}

/// `POST /v1/signin/stepup {tenant, person, approval}`: the person just confirmed this approval with a fresh passkey
/// check. Recorded for 5 minutes, single use (`stepup.rs`), and audited. 403 with the reason when the person isn't one
/// of the tenant's admins or the approval isn't a pending role-window request; 404 when there's no such approval.
pub async fn post_stepup(State(app): State<Arc<App>>, headers: HeaderMap, Json(r): Json<StepupRequest>) -> ApiResult {
    let by = guard(&app, &headers).await?;
    let fleet = app.authz.fleet(&app.db).await.map_err(internal)?;
    let tenant = fleet.tenant(&r.tenant).ok_or((StatusCode::NOT_FOUND, format!("no tenant {}", r.tenant)))?;
    if crate::stepup::view(&app.db, &tenant.id, &r.approval).await.map_err(internal)?.is_none() {
        return Err((StatusCode::NOT_FOUND, format!("no request {}", r.approval)));
    }
    let (handle, role) = match tenant.handles.get(&r.person).and_then(|h| tenant.people.get(h).map(|p| (h, p))) {
        Some((h, p)) => (h.clone(), p.role.clone()),
        None => return Err((StatusCode::FORBIDDEN, "this person isn't a member of the tenant".into())),
    };
    if let Some(why) = crate::stepup::refusal(&app.db, &tenant.id, &r.approval, &role).await.map_err(internal)? {
        return Err((StatusCode::FORBIDDEN, why));
    }
    let person = format!("person:{handle}");
    let entry = crate::audit::Entry {
        decision: "recorded".into(),
        subject: person.clone(),
        verb: "signin.stepup".into(),
        target: r.approval.clone(),
        reason: format!("confirmed with a passkey, for 5 minutes (by signin:{by})"),
    };
    app.audit.record(&app.db, &tenant.id, entry).await.map_err(internal)?;
    let expires = crate::stepup::record(&app.db, &tenant.id, &r.approval, &person, &by).await.map_err(internal)?;
    Ok(Json(json!({"approval": r.approval, "person": person, "expiresAt": expires})))
}

#[derive(Deserialize)]
pub struct JoinRequest {
    tenant: String,
    display_name: String,
    /// What to make the handle from (an email's local part).
    hint: String,
    /// The upstream provider that vouched for them, for the audit log.
    via: String,
}

/// `POST /v1/signin/join`: someone an upstream provider vouches for joins its tenant as a member (`join: {role:
/// member}` in the sign-in service's settings). Never another role; recorded as the sign-in token, via the upstream.
pub async fn post_join(State(app): State<Arc<App>>, headers: HeaderMap, Json(j): Json<JoinRequest>) -> ApiResult {
    let token = guard(&app, &headers).await?;
    let fleet = app.authz.fleet(&app.db).await.map_err(internal)?;
    let tenant = fleet.tenant(&j.tenant).ok_or_else(|| bad(format!("no tenant {}", j.tenant)))?;
    let name = j.display_name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(bad("a display name is 1 to 100 characters"));
    }
    let by = format!("signin:{token} via {}", j.via);
    let done = crate::people::join(&app.db, &tenant.id, name, &j.hint, &by).await.map_err(bad)?;
    Ok(Json(json!({"tenant": done.tenant, "person": done.person, "handle": done.handle, "recovery": false})))
}

/// Teams from an upstream provider's groups, at a sign-in through it or the sign-in service's daily resync.
#[derive(Deserialize)]
pub struct TeamsSyncRequest {
    tenant: String,
    person: String,
    upstream: String,
    teams: Vec<String>,
}

pub async fn post_teams_sync(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(r): Json<TeamsSyncRequest>,
) -> ApiResult {
    let token = guard(&app, &headers).await?;
    let fleet = app.authz.fleet(&app.db).await.map_err(internal)?;
    let tenant = fleet.tenant(&r.tenant).ok_or_else(|| bad(format!("no tenant {}", r.tenant)))?;
    let by = format!("signin:{token} via upstream {}", r.upstream);
    let done =
        crate::people::teams_sync(&app.db, &tenant.id, &r.person, &r.upstream, &r.teams, &by).await.map_err(bad)?;
    Ok(Json(json!(done)))
}

// ---------------------------------------------------------------- admin pages

/// A change an admin makes on the sign-in service's admin pages, for a tenant, as that person.
#[derive(Deserialize)]
pub struct AdminRequest {
    tenant: String,
    /// The person acting (their ID). The control plane checks they may administer the tenant.
    #[serde(rename = "as")]
    actor: String,
    #[serde(default)]
    handle: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    teams: Vec<String>,
    #[serde(default)]
    team: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    add: Option<bool>,
    #[serde(default)]
    maintainer: Option<bool>,
}

/// Who may use the admin pages for a tenant: its admins, and anyone its policy allows `admin` on `service:signin`.
/// Returns the tenant's ID and the actor's handle, for the audit log.
async fn admin_of(app: &App, r: &AdminRequest) -> Result<(String, String), (StatusCode, String)> {
    let fleet = app.authz.fleet(&app.db).await.map_err(internal)?;
    let tenant = fleet.tenant(&r.tenant).ok_or_else(|| bad(format!("no tenant {}", r.tenant)))?;
    let Some(handle) = tenant.handles.get(&r.actor).cloned() else {
        return Err((StatusCode::FORBIDDEN, format!("{} isn't a member of {}", r.actor, tenant.slug)));
    };
    let is_admin = tenant.people.get(&handle).is_some_and(|p| p.role == "admin");
    if !is_admin {
        let ask = fleetlayer_protocol::link::AccessCheck {
            tenant: tenant.id.clone(),
            who: r.actor.clone(),
            can: "admin".into(),
            to: "service:signin".into(),
            ..Default::default()
        };
        let d = crate::authz::check_as(app, &fleet, tenant, &ask).await.map_err(|e| bad(e.message))?;
        if !d.allowed {
            return Err((
                StatusCode::FORBIDDEN,
                format!("person:{handle} may not manage {}: {}", tenant.slug, d.reason),
            ));
        }
    }
    Ok((tenant.id.clone(), handle))
}

fn need<'a>(v: &'a Option<String>, what: &str) -> Result<&'a str, (StatusCode, String)> {
    v.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| bad(format!("{what} is needed")))
}

/// `POST /v1/signin/admin/view`: the tenant's members, pending invites and teams.
pub async fn post_admin_view(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(r): Json<AdminRequest>,
) -> ApiResult {
    guard(&app, &headers).await?;
    let (tenant, _) = admin_of(&app, &r).await?;
    let slug: (String, String) = sqlx::query_as("SELECT slug, name FROM tenants WHERE id = ?")
        .bind(&tenant)
        .fetch_one(&app.db)
        .await
        .map_err(internal)?;
    let members = crate::people::members(&app.db, &tenant).await.map_err(internal)?;
    let invites = crate::people::invites(&app.db, &tenant).await.map_err(internal)?;
    let teams = crate::people::teams(&app.db, &tenant).await.map_err(internal)?;
    Ok(Json(json!({
        "tenant": tenant, "slug": slug.0, "name": slug.1,
        "members": members, "invites": invites, "teams": teams,
    })))
}

/// `POST /v1/signin/admin/<action>`: one change, made with the same functions as the command line and recorded in
/// the tenant's audit log as the person who made it.
pub async fn post_admin_action(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    axum::extract::Path(action): axum::extract::Path<String>,
    Json(r): Json<AdminRequest>,
) -> ApiResult {
    let token = guard(&app, &headers).await?;
    let (tenant, handle) = admin_of(&app, &r).await?;
    let by = format!("person:{handle} (signin:{token})");
    let db = &app.db;
    let out = match action.as_str() {
        "invite" => {
            let i = crate::people::NewInvite {
                handle: need(&r.handle, "handle")?,
                role: r.role.as_deref().unwrap_or("member"),
                teams: &r.teams,
                person: None,
            };
            let code = crate::people::invite(db, &tenant, i, &by).await.map_err(bad)?;
            json!({"code": code})
        }
        "invite_revoke" => {
            crate::people::invite_revoke(db, &tenant, need(&r.handle, "handle")?, &by).await.map_err(bad)?;
            json!({})
        }
        "member_role" => {
            let role = need(&r.role, "role")?;
            crate::people::member_role(db, &tenant, need(&r.handle, "handle")?, role, &by).await.map_err(bad)?;
            json!({})
        }
        "member_remove" => {
            crate::people::member_remove(db, &tenant, need(&r.handle, "handle")?, &by).await.map_err(bad)?;
            json!({})
        }
        "team_add" => {
            let parent = r.parent.as_deref().filter(|p| !p.is_empty());
            crate::people::team_add(db, &tenant, need(&r.team, "team")?, parent, &by).await.map_err(bad)?;
            json!({})
        }
        "team_remove" => {
            crate::people::team_remove(db, &tenant, need(&r.team, "team")?, &by).await.map_err(bad)?;
            json!({})
        }
        "team_member" => {
            crate::people::team_member(
                db,
                &tenant,
                need(&r.team, "team")?,
                need(&r.handle, "handle")?,
                r.add.unwrap_or(true),
                r.maintainer.unwrap_or(false),
                &by,
            )
            .await
            .map_err(bad)?;
            json!({})
        }
        other => return Err((StatusCode::NOT_FOUND, format!("no admin action {other}"))),
    };
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::people::{self, NewInvite, Redeemer};

    #[tokio::test]
    async fn a_persons_tenants_say_what_their_id_tokens_will() {
        let db = crate::db::open_memory().await.unwrap();
        let acme = people::tenant_add(&db, "acme", "Acme", "admin").await.unwrap().id;
        people::team_add(&db, &acme, "engineering", None, "admin").await.unwrap();
        people::team_add(&db, &acme, "platform", Some("engineering"), "admin").await.unwrap();
        let teams = vec!["platform".to_string()];
        let code =
            people::invite(&db, &acme, NewInvite { handle: "a", role: "admin", teams: &teams, person: None }, "admin")
                .await
                .unwrap();
        let a = people::invite_redeem(&db, &code, Redeemer::New { display_name: "A" }).await.unwrap().person;
        let home = people::invite(
            &db,
            "default",
            NewInvite { handle: "a", role: "member", teams: &[], person: Some(&a) },
            "admin",
        )
        .await
        .unwrap();
        people::invite_redeem(&db, &home, Redeemer::Existing { person: &a }).await.unwrap();
        let p = person(&db, &a).await.unwrap().unwrap();
        assert_eq!(p.display_name, "A");
        assert_eq!(p.email, None);
        assert!(people::person_email(&db, &a, Some("not an address"), "admin").await.is_err());
        assert!(people::person_email(&db, "p_nobody", Some("a@acme.example"), "admin").await.is_err());
        people::person_email(&db, &a, Some(" a@acme.example "), "admin").await.unwrap();
        assert_eq!(person(&db, &a).await.unwrap().unwrap().email.as_deref(), Some("a@acme.example"));
        let logged: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_outbox WHERE verb = 'person.email'")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(logged, 2, "logged in both of their tenants");
        people::person_email(&db, &a, None, "admin").await.unwrap();
        assert_eq!(person(&db, &a).await.unwrap().unwrap().email, None);
        let acme = p.tenants.iter().find(|t| t.slug == "acme").unwrap();
        assert_eq!(acme.handle, "a");
        assert_eq!(acme.teams, ["engineering", "platform"]);
        assert_eq!(acme.groups, ["acme", "acme/engineering", "acme/engineering/platform", "acme/role:admin"]);
        let default = p.tenants.iter().find(|t| t.tenant == "default").unwrap();
        assert_eq!(default.groups, ["default", "default/role:member"]);
        assert!(person(&db, "p_nobody").await.unwrap().is_none());
    }

    fn bearer(t: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("authorization", format!("Bearer {t}").parse().unwrap());
        h
    }

    #[tokio::test]
    async fn only_a_sign_in_token_reaches_the_routes() {
        let app = Arc::new(App {
            db: crate::db::open_memory().await.unwrap(),
            links: Default::default(),
            hook_limits: Default::default(),
            admin_token_hash: None,
            streams: Default::default(),
            authz: Default::default(),
            audit: crate::audit::Audit::new(None),
        });
        let code = people::invite(
            &app.db,
            "default",
            NewInvite { handle: "a", role: "admin", teams: &[], person: None },
            "admin",
        )
        .await
        .unwrap();
        let admin = crate::db::add_token(&app.db, "default", "dash", "admin").await.unwrap();
        let q = |c: &str| Json(RedeemRequest { code: c.into(), display_name: Some("A".into()), person: None });
        let refused = post_invite_redeem(State(app.clone()), bearer(&admin), q(&code)).await.unwrap_err();
        assert_eq!(refused.0, StatusCode::UNAUTHORIZED, "an admin API token isn't a sign-in token");
        let t = token_add(&app.db, "signin").await.unwrap();
        let done = post_invite_redeem(State(app.clone()), bearer(&t), q(&code)).await.unwrap();
        let person = done.0["person"].as_str().unwrap().to_string();
        let p =
            post_person(State(app.clone()), bearer(&t), Json(PersonQuery { person: person.clone() })).await.unwrap();
        assert_eq!(p.0["tenants"][0]["handle"], "a");
        let ask = fleetlayer_protocol::link::AccessCheck {
            tenant: "default".into(),
            who: person.clone(),
            can: "read".into(),
            to: "service:quietdeck/a/inbox".into(),
            ..Default::default()
        };
        let d = post_check(State(app.clone()), bearer(&t), Json(ask)).await.unwrap();
        assert_eq!(d.0["allowed"], true, "{}", d.0);
        let e = EventRequest {
            tenant: "default".into(),
            person: person.clone(),
            kind: "signed_in".into(),
            client: Some("service:hub".into()),
            detail: Some("passkey".into()),
        };
        let _done = post_event(State(app.clone()), bearer(&t), Json(e)).await.unwrap();
        let logged: String = sqlx::query_scalar(
            "SELECT subject || ' ' || verb || ' ' || target || ' ' || reason FROM audit_log WHERE verb LIKE 'signin.%'",
        )
        .fetch_one(&app.db)
        .await
        .unwrap();
        assert_eq!(logged, "person:a signin.signed_in service:hub passkey (by signin:signin)");
        token_remove(&app.db, "signin").await.unwrap();
        let gone = post_person(State(app), bearer(&t), Json(PersonQuery { person })).await.unwrap_err();
        assert_eq!(gone.0, StatusCode::UNAUTHORIZED, "a revoked token stops at once");
    }

    #[tokio::test]
    async fn admins_manage_their_tenant_from_the_sign_in_pages() {
        let app = Arc::new(App {
            db: crate::db::open_memory().await.unwrap(),
            links: Default::default(),
            hook_limits: Default::default(),
            admin_token_hash: None,
            streams: Default::default(),
            authz: Default::default(),
            audit: crate::audit::Audit::new(None),
        });
        let t = token_add(&app.db, "signin").await.unwrap();
        let join = |handle: &'static str, role: &'static str| {
            let app = app.clone();
            async move {
                let i = NewInvite { handle, role, teams: &[], person: None };
                let code = people::invite(&app.db, "default", i, "admin").await.unwrap();
                people::invite_redeem(&app.db, &code, Redeemer::New { display_name: handle }).await.unwrap().person
            }
        };
        let a = join("a", "admin").await;
        let b = join("b", "member").await;
        let req = |actor: &str, handle: Option<&str>, role: Option<&str>| {
            Json(AdminRequest {
                tenant: "default".into(),
                actor: actor.into(),
                handle: handle.map(String::from),
                role: role.map(String::from),
                teams: vec![],
                team: None,
                parent: None,
                add: None,
                maintainer: None,
            })
        };
        let action = |name: &str| axum::extract::Path(name.to_string());
        let view = post_admin_view(State(app.clone()), bearer(&t), req(&a, None, None)).await.unwrap();
        assert_eq!(view.0["members"].as_array().unwrap().len(), 2);
        // A member isn't an admin, and the policy gives them nothing more.
        let no = post_admin_view(State(app.clone()), bearer(&t), req(&b, None, None)).await.unwrap_err();
        assert_eq!(no.0, StatusCode::FORBIDDEN);
        // An admin invites; the change is recorded as them.
        let done = post_admin_action(State(app.clone()), bearer(&t), action("invite"), req(&a, Some("c"), None))
            .await
            .unwrap();
        assert!(done.0["code"].as_str().is_some());
        let by: String =
            sqlx::query_scalar("SELECT subject FROM audit_outbox WHERE verb = 'invite.create' ORDER BY id DESC")
                .fetch_one(&app.db)
                .await
                .unwrap();
        assert_eq!(by, "person:a (signin:signin)");
        // The last admin can't demote or remove themselves, and is told why.
        let e = post_admin_action(
            State(app.clone()),
            bearer(&t),
            action("member_role"),
            req(&a, Some("a"), Some("member")),
        )
        .await
        .unwrap_err();
        assert!(e.1.contains("last admin"), "{}", e.1);
        let e = post_admin_action(State(app.clone()), bearer(&t), action("member_remove"), req(&a, Some("a"), None))
            .await
            .unwrap_err();
        assert!(e.1.contains("last admin"), "{}", e.1);
        // B made an admin can then do it, and the policy's `admin` on service:signin works too.
        let _ =
            post_admin_action(State(app.clone()), bearer(&t), action("member_role"), req(&a, Some("b"), Some("admin")))
                .await
                .unwrap();
        let _ = post_admin_action(
            State(app.clone()),
            bearer(&t),
            action("member_role"),
            req(&b, Some("a"), Some("member")),
        )
        .await
        .unwrap();
        assert_eq!(
            post_admin_view(State(app.clone()), bearer(&t), req(&a, None, None)).await.unwrap_err().0,
            StatusCode::FORBIDDEN
        );
        crate::authz::store(
            &app.db,
            "default",
            "version: 2\ndefault: allow\nallow:\n  - { who: person:a, can: admin, to: service:signin }\n",
        )
        .await
        .unwrap();
        assert!(post_admin_view(State(app.clone()), bearer(&t), req(&a, None, None)).await.is_ok());
    }

    #[tokio::test]
    async fn sign_in_tokens_are_their_own() {
        let db = crate::db::open_memory().await.unwrap();
        let t = token_add(&db, "signin").await.unwrap();
        assert!(t.starts_with("fls_"));
        assert_eq!(tokens(&db).await.unwrap().len(), 1);
        // Not an API token: the read and admin routes don't take it.
        assert!(crate::db::token_scope(&db, &t).await.unwrap().is_none());
        assert!(token_remove(&db, "signin").await.unwrap());
    }

    #[tokio::test]
    async fn the_signin_service_records_a_passkey_stepup_for_an_admin_and_a_pending_window_request() {
        let app = Arc::new(App {
            db: crate::db::open_memory().await.unwrap(),
            links: Default::default(),
            hook_limits: Default::default(),
            admin_token_hash: None,
            streams: Default::default(),
            authz: Default::default(),
            audit: crate::audit::Audit::new(None),
        });
        let t = token_add(&app.db, "signin").await.unwrap();
        let mut h = HeaderMap::new();
        h.insert("authorization", format!("Bearer {t}").parse().unwrap());
        let join = |handle: &'static str, role: &'static str| {
            let app = app.clone();
            async move {
                let i = NewInvite { handle, role, teams: &[], person: None };
                let code = people::invite(&app.db, "default", i, "admin").await.unwrap();
                people::invite_redeem(&app.db, &code, Redeemer::New { display_name: handle }).await.unwrap().person
            }
        };
        let a = join("a", "admin").await;
        let b = join("b", "member").await;
        sqlx::query(
            "INSERT INTO approvals (tenant, id, subject, verb, target, status, hold_ms, created_at, detail)
             VALUES ('default', 'ap_w', 'ops@admin', 'activate', 'window:admin', 'pending', 3600000, 0, 'upgrade postgres')",
        )
        .execute(&app.db)
        .await
        .unwrap();
        let view = |tenant: &str, approval: &str| {
            Json(ApprovalViewRequest { tenant: tenant.into(), approval: approval.into() })
        };
        let Json(v) = post_approval_view(State(app.clone()), h.clone(), view("default", "ap_w")).await.unwrap();
        assert_eq!(
            (v["to"].as_str(), v["status"].as_str(), v["detail"].as_str()),
            (Some("window:admin"), Some("pending"), Some("upgrade postgres"))
        );
        assert_eq!(
            post_approval_view(State(app.clone()), h.clone(), view("default", "ap_x")).await.unwrap_err().0,
            StatusCode::NOT_FOUND
        );
        assert!(
            post_approval_view(State(app.clone()), HeaderMap::new(), view("default", "ap_w")).await.is_err(),
            "the sign-in token only"
        );
        let step = |person: &str| {
            Json(StepupRequest { tenant: "default".into(), person: person.into(), approval: "ap_w".into() })
        };
        let e = post_stepup(State(app.clone()), h.clone(), step(&b)).await.unwrap_err();
        assert_eq!(e.0, StatusCode::FORBIDDEN, "a member isn't an admin: {}", e.1);
        let Json(ok) = post_stepup(State(app.clone()), h.clone(), step(&a)).await.unwrap();
        assert_eq!(ok["person"], "person:a");
        assert!(crate::stepup::consume(&app.db, "default", "ap_w", "person:a").await.unwrap());
        let logged: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE verb = 'signin.stepup' AND target = 'ap_w'")
                .fetch_one(&app.db)
                .await
                .unwrap();
        assert_eq!(logged, 1);
    }
}
