//! Authorization for every request the control plane handles. Each request is decided in the tenants that own what it
//! touches (`docs/sso.md`): every tenant either agent is a member of, or for a tenant's own things (services, pages,
//! apps) that tenant; membership first, then the tenant's policy. A tenant's version 2 policy (`policy.rs`), once
//! applied, decides everything there; before one is applied, the version 1 rules (`acl.rs`) decide agents' messages,
//! tools and sync as before, and a built-in policy decides people. Every refusal is published as `access.denied`,
//! every allowed request an audit rule would refuse as `access.audit`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use fleetlayer_protocol::link::method as m;
use fleetlayer_protocol::rpc::RpcError;
use sqlx::Row;
use tokio::sync::{Mutex, RwLock};

use crate::App;
use crate::db::{Db, now_ms};
use crate::directory::Fleet;
use crate::policy::{Compiled, Scope};

/// Every link method a node can call, and the verb it needs (None: needs none, it only concerns the node itself).
/// A test checks every method is listed, so a new one can't skip authorization by being forgotten.
pub const LINK_METHODS: &[(&str, Option<&str>)] = &[
    (m::HELLO, None),
    (m::SESSIONS, None),
    (m::SESSION, None),
    (m::SESSION_GONE, None),
    (m::SUMMARY, None),
    (m::SEND, Some("message")),
    (m::PUBLISH, Some("publish")),
    (m::SUBSCRIBE, Some("subscribe")),
    (m::UNSUBSCRIBE, Some("subscribe")),
    (m::SUBSCRIPTIONS, Some("read")),
    (m::FLEET, Some("read")),
    (m::SUMMARIES_SEARCH, Some("read")),
    (m::TOOL_OPEN, Some("use_tool")),
    (m::TERMINAL_OPEN, Some("read")),
    (m::SYNC_PUBLISH, Some("sync")),
    (m::READINGS, None),
    (m::READING_SUBSCRIBE, Some("read")),
    (m::READING_LATEST, Some("read")),
    // Decided as the person or agent it names, in the tenant it names, once the asking node is a member there.
    (m::ACCESS_CHECK, None),
    (m::PAGES_SPACES, None),
    (m::PAGES_OPEN, Some("write")),
    (m::SESSION_START, Some("start")),
];

#[derive(Default)]
pub struct Authz {
    /// Each tenant's policy, by tenant ID, with the text it was compiled from.
    policy: RwLock<HashMap<String, (String, Arc<Compiled>)>>,
    /// Who belongs where, with the change marker it was loaded at.
    fleet: RwLock<Option<(i64, Arc<Fleet>)>>,
    /// Recent messages per sender, for rate limits: (when, recipient).
    sent: Mutex<HashMap<String, VecDeque<(Instant, String)>>>,
}

pub async fn stored(db: &Db, tenant: &str) -> anyhow::Result<Option<String>> {
    let row = sqlx::query("SELECT text FROM policy_files WHERE tenant = ?").bind(tenant).fetch_optional(db).await?;
    Ok(row.map(|r| r.get(0)))
}

pub async fn store(db: &Db, tenant: &str, text: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO policy_files (tenant, text, applied_at) VALUES (?, ?, ?)
         ON CONFLICT (tenant) DO UPDATE SET text = excluded.text, applied_at = excluded.applied_at",
    )
    .bind(tenant)
    .bind(text)
    .bind(now_ms())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn remove(db: &Db, tenant: &str) -> anyhow::Result<bool> {
    let r = sqlx::query("DELETE FROM policy_files WHERE tenant = ?").bind(tenant).execute(db).await?;
    Ok(r.rows_affected() > 0)
}

impl Authz {
    /// A tenant's version 2 policy, reloaded when the stored text changed (`acl apply` from another process). A tenant
    /// with neither a policy nor version 1 rules gets the built-in default ([`effective`]); None means version 1 rules.
    pub async fn policy(&self, db: &Db, tenant: &str) -> anyhow::Result<Option<Arc<Compiled>>> {
        let text = effective(db, tenant).await?.map(|(t, _)| t);
        {
            let cur = self.policy.read().await;
            match (cur.get(tenant), &text) {
                (Some((t, c)), Some(new)) if t == new => return Ok(Some(c.clone())),
                (None, None) => return Ok(None),
                _ => {}
            }
        }
        let mut cur = self.policy.write().await;
        match text {
            Some(t) => {
                let c = Arc::new(Compiled::parse(&t)?);
                tracing::info!(tenant, "access policy (version 2) loaded");
                cur.insert(tenant.to_string(), (t, c.clone()));
                Ok(Some(c))
            }
            None => {
                cur.remove(tenant);
                Ok(None)
            }
        }
    }

    /// Who belongs where, reloaded when it changed.
    pub async fn fleet(&self, db: &Db) -> anyhow::Result<Arc<Fleet>> {
        let version = Fleet::version(db).await?;
        if let Some((v, f)) = &*self.fleet.read().await
            && *v == version
        {
            return Ok(f.clone());
        }
        let f = Arc::new(Fleet::load(db).await?);
        *self.fleet.write().await = Some((version, f.clone()));
        Ok(f)
    }
}

/// The policy a tenant without a version 2 policy decides people and its own things (services, pages, apps) with:
/// agents as `default: allow`, people their own things, Fleetlayer's default approvals.
fn builtin() -> Arc<Compiled> {
    static P: OnceLock<Arc<Compiled>> = OnceLock::new();
    P.get_or_init(|| Arc::new(Compiled::parse(crate::policy::BUILTIN).expect("the built-in policy compiles"))).clone()
}

/// The policy text that decides in a tenant, and whether it's the built-in default: the stored version 2 policy, or the
/// built-in default when there are no version 1 rules either. None: version 1 rules decide.
pub async fn effective(db: &Db, tenant: &str) -> anyhow::Result<Option<(String, bool)>> {
    if let Some(t) = stored(db, tenant).await? {
        return Ok(Some((t, false)));
    }
    if crate::acl::configured(db, tenant).await? {
        return Ok(None);
    }
    Ok(Some((crate::policy::BUILTIN.to_string(), true)))
}

/// One tenant's say on a request.
struct Say {
    tenant: String,
    slug: String,
    allowed: bool,
    because: String,
    /// The policy that decided, for its `justify:` and `ask:` rules; None for version 1 rules.
    policy: Option<Arc<Compiled>>,
}

/// The outcome of a request, in every tenant that decides it.
pub struct Outcome {
    pub allowed: bool,
    /// The rules behind it.
    pub because: Vec<String>,
    /// The refusal to send back, when it isn't allowed.
    pub refusal: Option<RpcError>,
}

/// Whether `who` may `verb` `target`, publishing refusals and audit matches. Err is the refusal to send back.
pub async fn check(app: &App, home: &str, who: &str, verb: &str, target: &str) -> Result<(), RpcError> {
    check_why(app, home, who, verb, target, None).await
}

/// [`check`], with the request's justification, which `justify:` rules can require; it goes in the audit log. `home`
/// is the tenant the request's data is kept in (the node's); it's decided in every tenant either side is a member of
/// (`docs/sso.md`).
pub async fn check_why(
    app: &App,
    home: &str,
    who: &str,
    verb: &str,
    target: &str,
    why: Option<&str>,
) -> Result<(), RpcError> {
    let fleet = app.authz.fleet(&app.db).await.map_err(RpcError::internal)?;
    let deciders: Vec<String> = fleet.deciders(who, target).iter().map(|t| t.id.clone()).collect();
    let reply = crate::acl::replying(&app.db, home, who, target).await.map_err(RpcError::internal)?;
    let out = decide_in(app, &fleet, &deciders, Some(home), (who, verb, target), why, reply).await?;
    match out.refusal {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// A service asking about a person or agent in one tenant (`link/access_check`, `docs/sso.md`): answered only if the
/// asking node's `-@<node>` is a member of that tenant, and only about what belongs to a tenant (services, pages,
/// apps). Decided and audited like any other request; a refusal is an answer, not an error.
pub async fn access_check(
    app: &App,
    node: &str,
    req: &fleetlayer_protocol::link::AccessCheck,
) -> Result<fleetlayer_protocol::link::AccessDecision, RpcError> {
    let fleet = app.authz.fleet(&app.db).await.map_err(RpcError::internal)?;
    let not_member = || RpcError::refused(format!("node {node} isn't a member of tenant {}", req.tenant));
    let tenant = fleet.tenant(&req.tenant).ok_or_else(not_member)?;
    if !tenant.is_member(&fleet, &format!("-@{node}")) {
        return Err(not_member());
    }
    check_as(app, &fleet, tenant, req).await
}

/// [`access_check`] once the asker may ask about the tenant: the sign-in service (`signin.rs`), or a member node.
pub async fn check_as(
    app: &App,
    fleet: &Fleet,
    tenant: &crate::directory::Tenant,
    req: &fleetlayer_protocol::link::AccessCheck,
) -> Result<fleetlayer_protocol::link::AccessDecision, RpcError> {
    if !crate::directory::is_tenant_thing(&req.to) || req.to.starts_with("person:") {
        return Err(RpcError::invalid_params(format!(
            "{} isn't something a tenant owns: ask about service:…, page:… or app:…",
            req.to
        )));
    }
    if !crate::policy::VERBS.contains(&req.can.as_str()) {
        return Err(RpcError::invalid_params(format!("unknown verb {}", req.can)));
    }
    // A person may be named by their ID (the sign-in's `sub`) or as `person:<handle>`.
    let who = match tenant.handles.get(&req.who) {
        Some(h) => format!("person:{h}"),
        None if req.who.starts_with("p_") => "person:?".into(),
        None => req.who.clone(),
    };
    let tenants = [tenant.id.clone()];
    let out = decide_in(app, fleet, &tenants, None, (&who, &req.can, &req.to), None, false).await?;
    let reason = match &out.refusal {
        Some(e) => e.message.clone(),
        None => out.because.join(", "),
    };
    Ok(fleetlayer_protocol::link::AccessDecision { allowed: out.allowed, rules: out.because, reason })
}

/// Decide a request in `tenants`: allowed only if every one allows it, each recording its own say in its audit log.
/// `home` is where a refusal is published as `access.denied` (None: not published, for a service's checks).
async fn decide_in(
    app: &App,
    fleet: &Fleet,
    tenants: &[String],
    home: Option<&str>,
    (who, verb, target): (&str, &str, &str),
    why: Option<&str>,
    reply: bool,
) -> Result<Outcome, RpcError> {
    let entry = |decision: &str, reason: String| crate::audit::Entry {
        decision: decision.into(),
        subject: who.into(),
        verb: verb.into(),
        target: target.into(),
        reason,
    };
    // Fail closed: nothing goes ahead without its entry in the audit log.
    let record = |tenant: String, e: crate::audit::Entry| async move {
        app.audit.record(&app.db, &tenant, e).await.map_err(|err| {
            tracing::error!("the audit log can't be written, so refusing {who} {verb} {target}: {err}");
            RpcError::refused("the control plane's audit log can't be written now, so nothing is allowed")
        })
    };
    if tenants.is_empty() {
        let because = format!("neither {who} nor {target} is a member of any tenant");
        if let Some(h) = home {
            record(h.to_string(), entry("denied", because.clone())).await?;
            crate::route::denied(app, h, who, target, verb).await;
        }
        return Ok(Outcome {
            allowed: false,
            because: vec![because.clone()],
            refusal: Some(RpcError::refused(format!(
                "{who} may not {verb} {target}: {because}. A tenant's admin admits agents with `fleetlayer-control \
                 agent admit`"
            ))),
        });
    }
    // Each tenant's say, from its rules alone.
    let mut says = vec![];
    for id in tenants {
        let Some(tenant) = fleet.tenant(id) else { continue };
        let scope = Scope { fleet, tenant };
        let policy = app.authz.policy(&app.db, id).await.map_err(RpcError::internal)?;
        let people_or_tenant_thing = who.starts_with("person:") || crate::directory::is_tenant_thing(target);
        let say = match policy {
            Some(p) => {
                let d = p.decide_in(Some(scope), who, verb, target, reply).map_err(RpcError::internal)?;
                if d.allowed && !d.audit_would_deny.is_empty() {
                    audit(app, home.unwrap_or(id), who, verb, target, &d.audit_would_deny).await;
                }
                Say {
                    tenant: id.clone(),
                    slug: tenant.slug.clone(),
                    allowed: d.allowed,
                    because: d.because.join(", "),
                    policy: Some(p),
                }
            }
            None if people_or_tenant_thing => {
                let p = builtin();
                let d = p.decide_in(Some(scope), who, verb, target, reply).map_err(RpcError::internal)?;
                Say {
                    tenant: id.clone(),
                    slug: tenant.slug.clone(),
                    allowed: d.allowed,
                    because: d.because.join(", "),
                    policy: Some(p),
                }
            }
            None => {
                // Version 1: membership, then rules on messages, tools (tool:…) and sync (sync:…); else allowed.
                let (ok, because) = match builtin().refused_up_front(scope, who, target) {
                    Some(r) => (false, r),
                    None => {
                        let v1 = matches!(verb, "message" | "use_tool" | "sync");
                        let ok = !v1
                            || reply
                            || crate::acl::allowed(&app.db, id, who, target).await.map_err(RpcError::internal)?;
                        (ok, "a version 1 rule".to_string())
                    }
                };
                Say { tenant: id.clone(), slug: tenant.slug.clone(), allowed: ok, because, policy: None }
            }
        };
        says.push(say);
    }
    let named = |s: &Say| {
        // Name the tenant only to its own members, and only when more than one decides.
        if says.len() > 1 && fleet.tenant(&s.tenant).is_some_and(|t| t.is_member(fleet, who)) {
            format!("{} in {}", s.because, s.slug)
        } else {
            s.because.clone()
        }
    };
    if let Some(no) = says.iter().find(|s| !s.allowed) {
        // Recorded where it was refused; the other tenants never saw it go ahead.
        for s in says.iter().filter(|s| !s.allowed) {
            record(s.tenant.clone(), entry("denied", s.because.clone())).await?;
        }
        if let Some(h) = home {
            crate::route::denied(app, h, who, target, verb).await;
        }
        let because = named(no);
        return Ok(Outcome {
            allowed: false,
            because: vec![because.clone()],
            refusal: Some(RpcError::refused(format!(
                "{who} may not {verb} {target} (access control: {because}). If this stops you doing your job, report \
                 it to your person"
            ))),
        });
    }
    // Allowed by every tenant's rules: then any tenant's `justify:` and `ask:`.
    let mut no_reason: Option<String> = None;
    for s in &says {
        if no_reason.is_none()
            && let (Some(p), Some(t)) = (&s.policy, fleet.tenant(&s.tenant))
            && p.needs_justification_in(Some(Scope { fleet, tenant: t }), who, verb, target)
        {
            no_reason = match why {
                None => Some("its access rules need a reason: give one with --why \"…\" (one line)".into()),
                Some(w) => crate::policy::check_justification(w).err(),
            };
        }
    }
    let mut waiting: Vec<(String, String)> = vec![];
    if no_reason.is_none() {
        for s in &says {
            let Some(p) = &s.policy else { continue };
            let Some(t) = fleet.tenant(&s.tenant) else { continue };
            let Some(hold) = p.ask_for_in(Some(Scope { fleet, tenant: t }), who, verb, target) else { continue };
            let held = crate::policy::approval_target(target);
            match crate::approvals::check(app, &s.tenant, who, verb, &held, hold).await.map_err(RpcError::internal)? {
                crate::approvals::Status::Approved => {}
                crate::approvals::Status::Pending(id) => {
                    waiting.push((s.tenant.clone(), format!("waiting for the person to approve request {id}")))
                }
                crate::approvals::Status::Denied(id) => {
                    waiting.push((s.tenant.clone(), format!("the person refused request {id}")))
                }
            }
        }
    }
    for s in &says {
        let wait = waiting.iter().find(|(t, _)| *t == s.tenant).map(|(_, w)| w.clone());
        let (decision, reason) = match (&no_reason, wait, why) {
            (Some(r), _, _) => ("denied", r.clone()),
            (None, Some(w), _) => ("asked", w),
            (None, None, Some(w)) => ("allowed", format!("{}; why: {w}", s.because)),
            (None, None, None) => ("allowed", s.because.clone()),
        };
        record(s.tenant.clone(), entry(decision, reason)).await?;
    }
    let because: Vec<String> = says.iter().map(named).collect();
    if let Some(r) = no_reason {
        return Ok(Outcome {
            allowed: false,
            because,
            refusal: Some(RpcError::refused(format!("{who} may {verb} {target} only with a reason: {r}"))),
        });
    }
    if let Some((_, w)) = waiting.first() {
        return Ok(Outcome {
            allowed: false,
            because,
            refusal: Some(RpcError::refused(format!(
                "{who} needs its person's approval to {verb} {target}: {w}. Try again once they've approved it"
            ))),
        });
    }
    Ok(Outcome { allowed: true, because, refusal: None })
}

async fn audit(app: &App, tenant: &str, who: &str, verb: &str, target: &str, rules: &[String]) {
    let e = fleetlayer_protocol::ext::Event {
        channel: "access".into(),
        kind: "access.audit".into(),
        subject: Some(format!("{who} -> {target}")),
        at: jiff::Timestamp::now(),
        dedupe_key: None,
        summary: format!("an audit rule would refuse {who} {verb} {target}: {}", rules.join(", ")),
        body: serde_json::json!({"from": who, "to": target, "verb": verb, "rules": rules}),
    };
    let _ = crate::route::publish(app, tenant, e, "fleetlayer access control").await;
}

/// The version 2 policies of every tenant `who` is a member of.
async fn policies_of(app: &App, who: &str) -> anyhow::Result<Vec<Arc<Compiled>>> {
    let fleet = app.authz.fleet(&app.db).await?;
    let mut out = vec![];
    for t in fleet.tenants_of(who) {
        if let Some(p) = app.authz.policy(&app.db, &t.id).await? {
            out.push(p);
        }
    }
    Ok(out)
}

/// Whether `who` sending to `to` now stays within its rate limits (the tightest of its tenants'); records the message
/// when it does. `home` is where a refusal is published.
pub async fn within_limits(app: &App, home: &str, who: &str, to: &str) -> Result<(), RpcError> {
    let (mut per_hour, mut distinct): (Option<u32>, Option<u32>) = (None, None);
    for p in policies_of(app, who).await.map_err(RpcError::internal)? {
        let (h, d) = p.limits_for(who);
        per_hour = h.map_or(per_hour, |h| Some(per_hour.map_or(h, |p| p.min(h))));
        distinct = d.map_or(distinct, |d| Some(distinct.map_or(d, |p| p.min(d))));
    }
    if per_hour.is_none() && distinct.is_none() {
        return Ok(());
    }
    let mut sent = app.authz.sent.lock().await;
    let log = sent.entry(who.to_string()).or_default();
    let now = Instant::now();
    while log.front().is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(3600)) {
        log.pop_front();
    }
    if let Some(n) = per_hour
        && log.len() >= n as usize
    {
        drop(sent);
        crate::route::denied(app, home, who, to, "message (rate limit)").await;
        return Err(RpcError::refused(format!("{who} has sent {n} messages in the last hour, its limit")));
    }
    if let Some(n) = distinct {
        let mut seen: Vec<&str> = log.iter().map(|(_, t)| t.as_str()).collect();
        seen.sort();
        seen.dedup();
        if !seen.contains(&to) && seen.len() >= n as usize {
            drop(sent);
            crate::route::denied(app, home, who, to, "message (recipient limit)").await;
            return Err(RpcError::refused(format!(
                "{who} has messaged {n} different sessions in the last hour, its limit"
            )));
        }
    }
    log.push_back((now, to.to_string()));
    Ok(())
}

/// Whether messages from `who` are delivered as outside data: any of its tenants says so.
pub async fn untrusted(app: &App, _home: &str, who: &str) -> bool {
    match policies_of(app, who).await {
        Ok(ps) => ps.iter().any(|p| p.untrusted(who)),
        // Not knowing is treated as untrusted: the message still arrives, marked as outside data.
        Err(_) => true,
    }
}

/// Whether `who` sees session `target` in the fleet's list: version 2 tenants' `read` rules, and in a tenant without
/// one, the sessions it may message (version 1).
pub async fn may_see(app: &App, _home: &str, who: &str, target: &str) -> bool {
    let Ok(fleet) = app.authz.fleet(&app.db).await else { return false };
    let deciders = fleet.deciders(who, target);
    if deciders.is_empty() {
        return false;
    }
    for tenant in deciders {
        let scope = Scope { fleet: &fleet, tenant };
        let allowed = match app.authz.policy(&app.db, &tenant.id).await {
            Ok(Some(p)) => p.decide_in(Some(scope), who, "read", target, false).is_ok_and(|d| d.allowed),
            Ok(None) => {
                builtin().refused_up_front(scope, who, target).is_none()
                    && crate::acl::allowed(&app.db, &tenant.id, who, target).await.unwrap_or(false)
            }
            Err(_) => false,
        };
        if !allowed {
            return false;
        }
    }
    true
}

/// Whether `who` may read `target`, for filtering lists (no audit entry or event per filtered item): every tenant
/// that decides it must allow it.
pub async fn may_read(app: &App, _home: &str, who: &str, target: &str) -> bool {
    let Ok(fleet) = app.authz.fleet(&app.db).await else { return false };
    let deciders = fleet.deciders(who, target);
    if deciders.is_empty() {
        return false;
    }
    for tenant in deciders {
        let scope = Scope { fleet: &fleet, tenant };
        let allowed = match app.authz.policy(&app.db, &tenant.id).await {
            Ok(Some(p)) => p.decide_in(Some(scope), who, "read", target, false).is_ok_and(|d| d.allowed),
            Ok(None) => builtin().refused_up_front(scope, who, target).is_none(),
            Err(_) => false,
        };
        if !allowed {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::people::{self, NewInvite, Owner, Redeemer};

    async fn app() -> App {
        App {
            db: crate::db::open_memory().await.unwrap(),
            links: Default::default(),
            hook_limits: Default::default(),
            admin_token_hash: None,
            streams: Default::default(),
            authz: Default::default(),
            audit: crate::audit::Audit::new(None),
        }
    }

    async fn join(db: &Db, tenant: &str, handle: &str, person: Option<&str>) -> String {
        let i = NewInvite { handle, role: "admin", teams: &[], person };
        let code = people::invite(db, tenant, i, "admin").await.unwrap();
        let who = match person {
            Some(p) => Redeemer::Existing { person: p },
            None => Redeemer::New { display_name: handle },
        };
        people::invite_redeem(db, &code, who).await.unwrap().person
    }

    /// Entries in a tenant's audit log, as `decision subject verb target`.
    async fn log(db: &Db, tenant: &str) -> Vec<String> {
        sqlx::query("SELECT decision, subject, verb, target FROM audit_log WHERE tenant = ? ORDER BY seq")
            .bind(tenant)
            .fetch_all(db)
            .await
            .unwrap()
            .iter()
            .map(|r| {
                let f = |i| r.get::<String, _>(i);
                format!("{} {} {} {}", f(0), f(1), f(2), f(3))
            })
            .collect()
    }

    /// Home (the `default` tenant) and Acme, with A in both and A's laptop's agents admitted per tenant.
    async fn example() -> (App, String) {
        let app = app().await;
        let db = &app.db;
        let acme = people::tenant_add(db, "acme", "Acme", "admin").await.unwrap().id;
        let a = join(db, "default", "a", None).await;
        join(db, &acme, "a", Some(&a)).await;
        crate::db::enrol(db, "default", "laptop").await.unwrap();
        people::node_owner(db, "laptop", Some(&Owner::Person(a)), "admin", "default").await.unwrap();
        people::agent_admit(db, "default", "recipes@laptop", "admin").await.unwrap();
        people::agent_admit(db, &acme, "billing@laptop", "admin").await.unwrap();
        crate::db::enrol(db, "default", "web").await.unwrap();
        people::node_owner(db, "web", Some(&Owner::Tenant(acme.clone())), "admin", &acme).await.unwrap();
        (app, acme)
    }

    #[tokio::test]
    async fn between_agents_every_tenant_of_either_decides_and_records() {
        let (app, acme) = example().await;
        check(&app, "default", "recipes@laptop", "message", "billing@laptop").await.unwrap();
        assert_eq!(log(&app.db, "default").await, ["allowed recipes@laptop message billing@laptop"]);
        assert_eq!(log(&app.db, &acme).await, ["allowed recipes@laptop message billing@laptop"]);
        // Acme lets its agents hear only from its own members.
        store(&app.db, &acme, "version: 2\ndefault: allow\ndeny:\n  - { who: outside, can: message }\n").await.unwrap();
        let e = check(&app, "default", "recipes@laptop", "message", "billing@laptop").await.unwrap_err();
        assert!(e.message.contains("deny[0]"), "{}", e.message);
        assert!(!e.message.contains("acme"), "a tenant isn't named to someone outside it: {}", e.message);
        assert_eq!(log(&app.db, &acme).await.len(), 2);
        assert_eq!(log(&app.db, "default").await.len(), 1, "refused in Acme, so recorded only there");
        // An agent no tenant admitted is refused before any rule.
        let e = check(&app, "default", "x@stranger", "message", "y@stranger").await.unwrap_err();
        assert!(e.message.contains("is a member of any tenant"), "{}", e.message);
    }

    #[tokio::test]
    async fn a_fleet_with_no_policy_asks_before_sensitive_actions() {
        let (app, _) = example().await;
        let e = check(&app, "default", "recipes@laptop", "use_tool", "tool:browser@laptop").await.unwrap_err();
        assert!(e.message.contains("needs its person's approval"), "{}", e.message);
        assert_eq!(log(&app.db, "default").await, ["asked recipes@laptop use_tool tool:browser@laptop"]);
        check(&app, "default", "recipes@laptop", "message", "recipes@laptop").await.unwrap();
        // A deployment with version 1 rules keeps them, as before: they never asked.
        crate::acl::set_default(&app.db, "default", crate::acl::Action::Allow).await.unwrap();
        check(&app, "default", "recipes@laptop", "use_tool", "tool:shell@laptop").await.unwrap();
        assert_eq!(effective(&app.db, "default").await.unwrap(), None);
        // And one with a stored policy decides by it.
        store(&app.db, "default", "version: 2\ndefault: allow\nask: []\n").await.unwrap();
        check(&app, "default", "recipes@laptop", "use_tool", "tool:db@laptop").await.unwrap();
    }

    #[tokio::test]
    async fn a_service_asks_about_people_in_a_tenant_that_admitted_it() {
        let (app, acme) = example().await;
        let ask = |who: &str, can: &str, to: &str| fleetlayer_protocol::link::AccessCheck {
            tenant: "acme".into(),
            who: who.into(),
            can: can.into(),
            to: to.into(),
        };
        // `web` is Acme's own node, so it may ask; A's laptop isn't (only billing@laptop is admitted).
        let e = access_check(&app, "laptop", &ask("person:a", "sign_in", "service:pages")).await.unwrap_err();
        assert!(e.message.contains("isn't a member"), "{}", e.message);
        // Without a policy, people have their own things only.
        let d = access_check(&app, "web", &ask("person:a", "sign_in", "service:pages")).await.unwrap();
        assert!(!d.allowed);
        assert!(
            access_check(&app, "web", &ask("person:a", "read", "service:quietdeck/a/inbox")).await.unwrap().allowed
        );
        store(
            &app.db,
            &acme,
            "version: 2\ndefault: allow\nallow:\n  - { who: role:admin, can: sign_in, to: \"service:*\" }\n",
        )
        .await
        .unwrap();
        let d = access_check(&app, "web", &ask("person:a", "sign_in", "service:pages")).await.unwrap();
        assert!(d.allowed, "{d:?}");
        assert_eq!(d.rules, ["allow[0]"]);
        // By ID (the sign-in's `sub`) too; someone who isn't a member is refused.
        let id = people::resolve_person(&app.db, &acme, "a").await.unwrap();
        assert!(access_check(&app, "web", &ask(&id, "sign_in", "service:pages")).await.unwrap().allowed);
        let d = access_check(&app, "web", &ask("person:b", "sign_in", "service:pages")).await.unwrap();
        assert!(!d.allowed && d.reason.contains("isn't a member of acme"), "{d:?}");
        // Only about a tenant's own things.
        assert!(access_check(&app, "web", &ask("person:a", "read", "billing@laptop")).await.is_err());
        assert!(log(&app.db, &acme).await.iter().any(|l| l == "allowed person:a sign_in service:pages"));
    }

    /// Every method a node can call is listed, with the verb it needs or None on purpose.
    #[test]
    fn every_link_method_is_classified() {
        let listed: Vec<&str> = LINK_METHODS.iter().map(|(m, _)| *m).collect();
        let node_to_control = [
            m::HELLO,
            m::SESSIONS,
            m::SESSION,
            m::SESSION_GONE,
            m::SEND,
            m::PUBLISH,
            m::SUBSCRIBE,
            m::UNSUBSCRIBE,
            m::SUBSCRIPTIONS,
            m::FLEET,
            m::SUMMARY,
            m::SUMMARIES_SEARCH,
            m::TOOL_OPEN,
            m::TERMINAL_OPEN,
            m::SYNC_PUBLISH,
            m::READINGS,
            m::READING_SUBSCRIBE,
            m::READING_LATEST,
            m::ACCESS_CHECK,
            m::PAGES_SPACES,
            m::PAGES_OPEN,
            m::SESSION_START,
        ];
        for method in node_to_control {
            assert!(listed.contains(&method), "{method} isn't in LINK_METHODS");
        }
        for (method, verb) in LINK_METHODS {
            if let Some(v) = verb {
                assert!(crate::policy::VERBS.contains(v), "{method}: unknown verb {v}");
            }
        }
    }
}
