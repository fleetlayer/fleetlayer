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
    // Only the asking node's own sessions, and only events already sent to them.
    (m::EVENTS, None),
    (m::FLEET, Some("read")),
    (m::SUMMARIES_SEARCH, Some("read")),
    (m::TOOL_OPEN, Some("use_tool")),
    (m::TOOL_CALL, Some("call")),
    (m::COMMAND_OPEN, Some("run")),
    (m::COMMAND_CALL, Some("run")),
    (m::TERMINAL_OPEN, Some("read")),
    // Checked as `read` on the session, under every name it goes by (`route::read_transcript`).
    (m::TRANSCRIPT_READ, Some("read")),
    // Hits only in sessions the caller may read without asking (`route::search_transcripts`).
    (m::TRANSCRIPT_SEARCH, Some("read")),
    (m::SYNC_PUBLISH, Some("sync")),
    (m::READINGS, None),
    (m::READING_SUBSCRIBE, Some("read")),
    (m::READING_LATEST, Some("read")),
    // Decided as the person or agent it names, in the tenant it names, once the asking node is a member there.
    (m::ACCESS_CHECK, None),
    (m::PAGES_SPACES, None),
    (m::PAGES_OPEN, Some("write")),
    (m::SESSION_START, Some("start")),
    // `start` on the new session, and `read` on the old one when someone else hands it off (`handoffs::start`).
    (m::SESSION_HANDOFF, Some("start")),
    // The session the work went to, or `read` on the one it came from (`handoffs::get`).
    (m::HANDOFF_GET, Some("read")),
    (m::SESSION_CLOSE, Some("close")),
    (m::SESSION_ANSWER, Some("answer")),
    (m::TOOLS, None),
    // About the caller's own session only, decided item by item without asking (`me.rs`).
    (m::ME, None),
    // Role windows (`windows.rs`): who may ask or close is decided there, by the role's `who` and `asked_by:`, and
    // opening one always goes through the person's approval.
    (m::ROLE_ACTIVATE, None),
    (m::ROLE_DEACTIVATE, None),
    (m::ROLE_WINDOWS, None),
    (m::WINDOW_CLOSED, None),
];

#[derive(Default)]
pub struct Authz {
    /// Each tenant's policy, by tenant ID, with the text it was compiled from.
    policy: RwLock<HashMap<String, (String, Arc<Compiled>)>>,
    /// Who belongs where, with the change marker it was loaded at.
    fleet: RwLock<Option<(i64, Arc<Fleet>)>>,
    /// Recent messages per sender, for rate limits: (when, recipient).
    sent: Mutex<HashMap<String, VecDeque<(Instant, String)>>>,
    /// Policy files (`serve --policy-file`) that couldn't be applied, by path: why. Shown in `/healthz`.
    pub file_errors: std::sync::Mutex<std::collections::BTreeMap<String, String>>,
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
    /// The approval request it waits for, when that's why it isn't allowed.
    pub pending: Option<String>,
    /// Approvals `for: once` it relies on, left unspent for the caller to spend (`approvals::spend_all`) once the
    /// whole request has passed: a tool call through the MCP gate, whose other targets may still be refused.
    pub to_spend: Vec<String>,
}

/// A tool call through the MCP gate: what an approval `for: once` is bound to, and what the person sees when asked.
#[derive(Clone, Copy)]
pub struct CallContext<'a> {
    pub args_hash: &'a str,
    pub detail: Option<&'a str>,
    /// What the audit entry keeps besides the decision: a shared command's run (its program and typed arguments,
    /// files by name, size and hash, never contents). A tool call's arguments stay out of it (None).
    pub audit: Option<&'a str>,
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
    let reply = crate::acl::replying(&app.db, home, who, target).await.map_err(RpcError::internal)?;
    // A session goes by its tab name and its ID: rules written for either hold whichever it's called by. Every other
    // name's rules are checked first (a refusal of any refuses); then the request is decided, approved and audited
    // once, under its canonical name.
    let names = session_names(app, target).await;
    let canonical = names.first().map(String::as_str).unwrap_or(target);
    // Membership is recorded under a session's tab name, so its other names are decided in the canonical name's
    // tenants as well as their own.
    let mut alias_deciders: Vec<String> = fleet.deciders(who, canonical).iter().map(|t| t.id.clone()).collect();
    for alias in names.iter().skip(1) {
        for t in fleet.deciders(who, alias) {
            if !alias_deciders.contains(&t.id) {
                alias_deciders.push(t.id.clone());
            }
        }
    }
    for alias in names.iter().skip(1) {
        for tenant in alias_deciders.clone() {
            let Some((say, _)) = say_in(app, &fleet, &tenant, (who, verb, alias), reply).await? else { continue };
            if !say.allowed {
                let e = crate::audit::Entry {
                    decision: "denied".into(),
                    subject: who.into(),
                    verb: verb.into(),
                    target: canonical.into(),
                    reason: format!("as {alias}: {}", say.because),
                };
                app.audit.record(&app.db, &tenant, e).await.map_err(|err| {
                    tracing::error!("the audit log can't be written, so refusing {who} {verb} {canonical}: {err}");
                    RpcError::refused("the control plane's audit log can't be written now, so nothing is allowed")
                })?;
                crate::route::denied(app, home, who, canonical, verb).await;
                return Err(RpcError::refused(format!(
                    "{who} may not {verb} {canonical} (access control, as {alias}: {}). If this stops you doing your \
                     job, report it to your person",
                    say.because
                )));
            }
        }
    }
    let deciders: Vec<String> = fleet.deciders(who, canonical).iter().map(|t| t.id.clone()).collect();
    let out = decide_in(app, &fleet, &deciders, Some(home), (who, verb, canonical), why, reply, None).await?;
    match out.refusal {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Every name the session `target` (`<name>@<node>`) goes by, its canonical one first: `<tab>@<node>` when it has a
/// tab, then `<session id>@<node>` (for a tab name, every session that ran under it). Anything that isn't one of a
/// node's sessions (`-@<node>`, `tool:…`, a service, a person, a session the node hasn't reported) is just itself.
pub async fn session_names(app: &App, target: &str) -> Vec<String> {
    let only = || vec![target.to_string()];
    let Some((name, node)) = target.rsplit_once('@') else { return only() };
    if name.is_empty() || name == "-" || name.contains(':') {
        return only();
    }
    let Ok(sessions) = crate::db::node_sessions(&app.db, node).await else { return only() };
    let mut tabs: Vec<String> = vec![];
    let mut ids: Vec<String> = vec![];
    for s in &sessions {
        let id = s["sessionId"].as_str().unwrap_or_default();
        let tab = s["meta"]["tab"].as_str().filter(|t| !t.is_empty());
        if id == name || tab == Some(name) {
            if let Some(t) = tab
                && !tabs.iter().any(|x| x == t)
            {
                tabs.push(t.to_string());
            }
            if !id.is_empty() && !ids.iter().any(|x| x == id) {
                ids.push(id.to_string());
            }
        }
    }
    if tabs.is_empty() && ids.is_empty() {
        return only();
    }
    let mut out: Vec<String> = tabs.iter().chain(ids.iter()).map(|n| format!("{n}@{node}")).collect();
    // The name asked for is checked too, even when it's neither (it can't be, but cheaply sure).
    if !out.iter().any(|n| n == target) {
        out.push(target.to_string());
    }
    out
}

/// One tool call through the MCP gate (`link/tool_call`, verb `call`) or one run of a shared command
/// (`link/command_call`, verb `run`) on `target`, decided, audited and filed for approval like any request, with the
/// arguments hash and detail for an `ask:` rule.
pub async fn check_call(
    app: &App,
    home: &str,
    who: &str,
    verb: &str,
    target: &str,
    call: CallContext<'_>,
    why: Option<&str>,
) -> Result<Outcome, RpcError> {
    let fleet = app.authz.fleet(&app.db).await.map_err(RpcError::internal)?;
    let deciders: Vec<String> = fleet.deciders(who, target).iter().map(|t| t.id.clone()).collect();
    decide_in(app, &fleet, &deciders, Some(home), (who, verb, target), why, false, Some(call)).await
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
    // A node files requests only in its own name: about an agent on another node it may only look.
    if crate::directory::is_agent_side(&req.who) && req.who.rsplit_once('@').map(|(_, n)| n) != Some(node) && !req.peek
    {
        return Err(RpcError::refused(format!(
            "node {node} may ask about {} only with peek: approval requests are filed only by the node they're for",
            req.who
        )));
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
    // `connect` is only for internet hosts, and hosts take only `connect`.
    let host = req.to.strip_prefix("host:");
    if (req.can == "connect") != host.is_some() {
        return Err(RpcError::invalid_params(format!(
            "{} {}: connect goes with host:<name>[:<port>]",
            req.can, req.to
        )));
    }
    if let Some(h) = host
        && !crate::policy::valid_host_target(h)
    {
        return Err(RpcError::invalid_params(format!("{} isn't host:<name>[:<port>]", req.to)));
    }
    if let Some(w) = &req.why {
        crate::policy::check_justification(w).map_err(RpcError::invalid_params)?;
    }
    // A person may be named by their ID (the sign-in's `sub`) or as `person:<handle>`.
    let who = match tenant.handles.get(&req.who) {
        Some(h) => format!("person:{h}"),
        None if req.who.starts_with("p_") => "person:?".into(),
        None => req.who.clone(),
    };
    if req.peek {
        return Ok(peek(app, &tenant.id, &who, &req.can, &req.to).await);
    }
    let tenants = [tenant.id.clone()];
    let out = decide_in(app, fleet, &tenants, None, (&who, &req.can, &req.to), req.why.as_deref(), false, None).await?;
    let reason = match &out.refusal {
        Some(e) => e.message.clone(),
        None => out.because.join(", "),
    };
    Ok(fleetlayer_protocol::link::AccessDecision {
        allowed: out.allowed,
        rules: out.because,
        reason,
        pending: out.pending,
        ..Default::default()
    })
}

/// A source node asking about a bundle it would send (`sync:<bundle>@<target>`, verb `sync`) before it does the work,
/// so the person approves the sensitive part first with a reason they can read: the cookies plugin asks about
/// `sync:cookies-from-browser/<set>@<target>` before reading the browser. Only in the node's own name (`-@<node>`).
/// Decided, approved and audited exactly as the bundle's delivery would be (`check_why`); with `peek`, only looked
/// at, and an approval `for: once` is left for the request that uses it.
pub async fn sync_check(
    app: &App,
    home: &str,
    node: &str,
    req: &fleetlayer_protocol::link::AccessCheck,
) -> Result<fleetlayer_protocol::link::AccessDecision, RpcError> {
    let bundle_ok = req.to.strip_prefix("sync:").and_then(|t| t.rsplit_once('@')).is_some_and(|(b, n)| {
        !b.is_empty() && !n.is_empty() && b.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
    });
    if req.can != "sync" || !bundle_ok {
        return Err(RpcError::invalid_params(format!("{} {}: sync goes with sync:<bundle>@<node>", req.can, req.to)));
    }
    if req.who != format!("-@{node}") {
        return Err(RpcError::refused(format!(
            "node {node} may ask about sending a bundle only in its own name (-@{node}), not as {}",
            req.who
        )));
    }
    if let Some(w) = &req.why {
        crate::policy::check_justification(w).map_err(RpcError::invalid_params)?;
    }
    if req.peek {
        return Ok(peek(app, home, &req.who, "sync", &req.to).await);
    }
    Ok(match check_why(app, home, &req.who, "sync", &req.to, req.why.as_deref()).await {
        Ok(()) => {
            fleetlayer_protocol::link::AccessDecision { allowed: true, reason: "allowed".into(), ..Default::default() }
        }
        Err(e) => fleetlayer_protocol::link::AccessDecision { reason: e.message, ..Default::default() },
    })
}

/// An access check that only looks: the decision without an audit entry, an event or a new approval request. An
/// approval in use is renewed, as a real use would (the egress proxy checking a host it lets through).
async fn peek(
    app: &App,
    tenant: &str,
    who: &str,
    verb: &str,
    target: &str,
) -> fleetlayer_protocol::link::AccessDecision {
    let no = |reason: String| fleetlayer_protocol::link::AccessDecision { reason, ..Default::default() };
    match dry(app, tenant, who, verb, target).await {
        Dry::Allowed { .. } => {
            let held = crate::policy::approval_target(target);
            if let Ok(crate::approvals::Peek::Approved(_)) =
                crate::approvals::peek(&app.db, tenant, who, verb, &held).await
            {
                // Approved: this check is a use, so it renews (it never files: there's an approval). It never spends
                // an approval `for: once`: looking isn't using, and the request that follows spends it.
                let _ = crate::approvals::check_spending(app, tenant, who, verb, &held, 0, None, false).await;
            }
            fleetlayer_protocol::link::AccessDecision { allowed: true, reason: "allowed".into(), ..Default::default() }
        }
        Dry::Ask { pending: Some(id), .. } => fleetlayer_protocol::link::AccessDecision {
            pending: Some(id.clone()),
            ..no(format!("waiting for the person to approve request {id}"))
        },
        Dry::Ask { pending: None, .. } => fleetlayer_protocol::link::AccessDecision {
            not_asked: true,
            ..no(format!("not asked yet: {who}'s own node files the request"))
        },
        Dry::Refused { request, .. } => fleetlayer_protocol::link::AccessDecision {
            refused: Some(request.clone()),
            ..no(format!("the person refused request {request}"))
        },
        Dry::Denied => no("refused by the access policy".into()),
    }
}

/// Decide a request in `tenants`: allowed only if every one allows it, each recording its own say in its audit log.
/// `home` is where a refusal is published as `access.denied` (None: not published, for a service's checks).
#[allow(clippy::too_many_arguments)]
async fn decide_in(
    app: &App,
    fleet: &Fleet,
    tenants: &[String],
    home: Option<&str>,
    (who, verb, target): (&str, &str, &str),
    why: Option<&str>,
    reply: bool,
    call: Option<CallContext<'_>>,
) -> Result<Outcome, RpcError> {
    let audited = call.and_then(|c| c.audit);
    let entry = |decision: &str, reason: String| crate::audit::Entry {
        decision: decision.into(),
        subject: who.into(),
        verb: verb.into(),
        target: target.into(),
        reason: match audited {
            Some(d) => format!("{reason}; {d}"),
            None => reason,
        },
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
            pending: None,
            to_spend: vec![],
        });
    }
    // Each tenant's say, from its rules alone.
    let mut says = vec![];
    for id in tenants {
        let Some((say, would_deny)) = say_in(app, fleet, id, (who, verb, target), reply).await? else { continue };
        if say.allowed && !would_deny.is_empty() {
            audit(app, home.unwrap_or(id), who, verb, target, &would_deny).await;
        }
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
            pending: None,
            to_spend: vec![],
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
    let mut pending: Option<String> = None;
    let mut to_spend: Vec<String> = vec![];
    // A tool call too long to show the person whole can't be put to them: they'd approve what they didn't see.
    let mut unshowable = false;
    if no_reason.is_none() {
        for s in &says {
            let Some(p) = &s.policy else { continue };
            let Some(t) = fleet.tenant(&s.tenant) else { continue };
            let Some(hold) = p.ask_for_in(Some(Scope { fleet, tenant: t }), who, verb, target) else { continue };
            if in_window(app, &s.tenant, p, who).await? {
                continue;
            }
            let mut held = crate::policy::approval_target(target);
            // `for: once` on a tool call: approved for exactly these arguments.
            if let (crate::policy::ONCE, Some(c)) = (hold, call) {
                held = format!("{held}#{}", c.args_hash);
            }
            // What the person sees: the tool call, or the agent's reason. Both are written by the agent.
            let (status, spend) = if call.is_some_and(|c| c.detail.is_none()) {
                unshowable = true;
                continue;
            } else {
                let detail = call.and_then(|c| c.detail).or(why);
                // A tool call spends its approval only once all its targets pass (`link::tool_call`).
                crate::approvals::check_spending(app, &s.tenant, who, verb, &held, hold, detail, call.is_none())
                    .await
                    .map_err(RpcError::internal)?
            };
            to_spend.extend(spend);
            match status {
                crate::approvals::Status::Approved => {}
                crate::approvals::Status::Pending(id) => {
                    pending.get_or_insert(id.clone());
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
            (None, _, _) if unshowable => ("denied", "too long to show the person for approval".to_string()),
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
            pending: None,
            to_spend: vec![],
        });
    }
    if unshowable {
        return Ok(Outcome {
            allowed: false,
            because,
            refusal: Some(RpcError::refused(format!(
                "{who} needs its person's approval to {verb} {target}, and this call is too long to show them whole, \
                 so it wasn't put to them. Make smaller calls (fewer at once, shorter arguments)"
            ))),
            pending: None,
            to_spend: vec![],
        });
    }
    if let Some((_, w)) = waiting.first() {
        return Ok(Outcome {
            allowed: false,
            because,
            refusal: Some(RpcError::refused(format!(
                "{who} needs its person's approval to {verb} {target}: {w}. Try again once they've approved it"
            ))),
            pending,
            to_spend: vec![],
        });
    }
    Ok(Outcome { allowed: true, because, refusal: None, pending: None, to_spend })
}

/// One tenant's say on a request, from its rules alone, with the audit rules that would have refused it. No side
/// effects: [`decide_in`] records and publishes, [`dry`] doesn't.
async fn say_in(
    app: &App,
    fleet: &Fleet,
    id: &str,
    (who, verb, target): (&str, &str, &str),
    reply: bool,
) -> Result<Option<(Say, Vec<String>)>, RpcError> {
    let Some(tenant) = fleet.tenant(id) else { return Ok(None) };
    let scope = Scope { fleet, tenant };
    let policy = app.authz.policy(&app.db, id).await.map_err(RpcError::internal)?;
    let people_or_tenant_thing = who.starts_with("person:") || crate::directory::is_tenant_thing(target);
    let compiled = match policy {
        Some(p) => Some(p),
        None if people_or_tenant_thing => Some(builtin()),
        None => None,
    };
    if let Some(p) = compiled {
        if let Some(because) = window_off(app, id, &p, who, target).await? {
            let say =
                Say { tenant: id.to_string(), slug: tenant.slug.clone(), allowed: false, because, policy: Some(p) };
            return Ok(Some((say, vec![])));
        }
        let d = p.decide_in(Some(scope), who, verb, target, reply).map_err(RpcError::internal)?;
        let mut because = d.because.join(", ");
        // Inside a window, every decision names it, so the audit log shows what the window was used for.
        for (role, wid, ends) in crate::windows::open_for(&app.db, id, &p, who).await.map_err(RpcError::internal)? {
            because.push_str(&format!("; in {role}'s window {wid} (until {})", crate::windows::when(ends)));
        }
        let say =
            Say { tenant: id.to_string(), slug: tenant.slug.clone(), allowed: d.allowed, because, policy: Some(p) };
        return Ok(Some((say, d.audit_would_deny)));
    }
    // Version 1: membership, then rules on messages, tools (tool:…) and sync (sync:…); else allowed.
    let (ok, because) = match builtin().refused_up_front(scope, who, target) {
        Some(r) => (false, r),
        None => {
            let v1 = matches!(verb, "message" | "use_tool" | "sync");
            let ok = !v1 || reply || crate::acl::allowed(&app.db, id, who, target).await.map_err(RpcError::internal)?;
            (ok, "a version 1 rule".to_string())
        }
    };
    Ok(Some((Say { tenant: id.to_string(), slug: tenant.slug.clone(), allowed: ok, because, policy: None }, vec![])))
}

/// What the person would decide on a request, without asking: nothing recorded, published or filed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dry {
    /// Refused by a rule (or no tenant decides it).
    Denied,
    Allowed {
        why: bool,
        until: Option<i64>,
    },
    /// Allowed once the person approves; `pending` names a request already waiting.
    Ask {
        why: bool,
        pending: Option<String>,
    },
    /// The person refused it within the hour.
    Refused {
        why: bool,
        request: String,
    },
}

/// [`decide_in`]'s answer for `who` doing `verb` to `target`, for `fleetlayer me`: no audit entry, no event, no
/// approval request, and no renewal of one.
pub async fn dry(app: &App, home: &str, who: &str, verb: &str, target: &str) -> Dry {
    let Ok(fleet) = app.authz.fleet(&app.db).await else { return Dry::Denied };
    let tenants: Vec<String> = fleet.deciders(who, target).iter().map(|t| t.id.clone()).collect();
    if tenants.is_empty() {
        return Dry::Denied;
    }
    let reply = crate::acl::replying(&app.db, home, who, target).await.unwrap_or(false);
    let mut says = vec![];
    for id in &tenants {
        match say_in(app, &fleet, id, (who, verb, target), reply).await {
            Ok(Some((say, _))) if say.allowed => says.push(say),
            Ok(None) => {}
            _ => return Dry::Denied,
        }
    }
    let why = says.iter().any(|s| {
        let (Some(p), Some(t)) = (&s.policy, fleet.tenant(&s.tenant)) else { return false };
        p.needs_justification_in(Some(Scope { fleet: &fleet, tenant: t }), who, verb, target)
    });
    let mut until: Option<i64> = None;
    for s in &says {
        let (Some(p), Some(t)) = (&s.policy, fleet.tenant(&s.tenant)) else { continue };
        if p.ask_for_in(Some(Scope { fleet: &fleet, tenant: t }), who, verb, target).is_none() {
            continue;
        }
        if in_window(app, &s.tenant, p, who).await.unwrap_or(false) {
            continue;
        }
        let held = crate::policy::approval_target(target);
        match crate::approvals::peek(&app.db, &s.tenant, who, verb, &held).await {
            Ok(crate::approvals::Peek::Approved(e)) => until = Some(until.map_or(e, |u| u.min(e))),
            Ok(crate::approvals::Peek::Pending(id)) => return Dry::Ask { why, pending: Some(id) },
            Ok(crate::approvals::Peek::Denied(id)) => return Dry::Refused { why, request: id },
            Ok(crate::approvals::Peek::None) | Err(_) => return Dry::Ask { why, pending: None },
        }
    }
    Dry::Allowed { why, until }
}

/// Why a request is refused before any rule: `who` or `target` is one of the agents of a role with `activation:
/// window` whose window isn't open (`windows.rs`). Every decision checks it, the filtered reads too.
async fn window_off(
    app: &App,
    tenant: &str,
    p: &Compiled,
    who: &str,
    target: &str,
) -> Result<Option<String>, RpcError> {
    for (name, side) in [(who, "by"), (target, "to")] {
        let off = crate::windows::closed_roles(&app.db, tenant, p, name).await.map_err(RpcError::internal)?;
        if let Some(role) = off.first() {
            return Ok(Some(format!(
                "role {role} is off until its window is open (nothing {side} its agents until then): one of its \
                 agents asks with `fleetlayer role activate {role} --for 1h`, and the person approves"
            )));
        }
    }
    Ok(None)
}

/// Whether `who` is one of the agents of a role whose window is open: the window's approval covers its `ask:` rules.
async fn in_window(app: &App, tenant: &str, p: &Compiled, who: &str) -> Result<bool, RpcError> {
    Ok(!crate::windows::open_for(&app.db, tenant, p, who).await.map_err(RpcError::internal)?.is_empty())
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
            Ok(Some(p)) => {
                matches!(window_off(app, &tenant.id, &p, who, target).await, Ok(None))
                    && p.decide_in(Some(scope), who, "read", target, false).is_ok_and(|d| d.allowed)
            }
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
            Ok(Some(p)) => {
                matches!(window_off(app, &tenant.id, &p, who, target).await, Ok(None))
                    && p.decide_in(Some(scope), who, "read", target, false).is_ok_and(|d| d.allowed)
            }
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

    /// A rule written for a session's tab name holds when the session is named by its ID, and one written for its ID
    /// holds when it's named by its tab: both names are checked, and a refusal of either refuses.
    #[tokio::test]
    async fn a_session_named_by_its_id_gets_no_way_past_rules_for_its_tab() {
        let (app, _) = example().await;
        let id = "0b5e6f3c-1d2e-4f5a-9b8c-7d6e5f4a3b2c";
        crate::db::replace_sessions(
            &app.db,
            "default",
            "laptop",
            &[serde_json::json!({
            "sessionId": id, "manager": "herdr", "meta": {"tab": "recipes", "running": true}})],
        )
        .await
        .unwrap();
        store(
            &app.db,
            "default",
            "version: 2\ndefault: allow\nask: []\ndeny:\n  - { who: \"*\", can: read, to: recipes@laptop }\n",
        )
        .await
        .unwrap();
        for target in ["recipes@laptop", &format!("{id}@laptop")] {
            let e = check(&app, "default", "billing@laptop", "read", target).await;
            let e = e.err().unwrap_or_else(|| panic!("reading {target} was allowed"));
            assert!(e.message.contains("deny[0]"), "{target}: {}", e.message);
        }
        check(&app, "default", "billing@laptop", "message", &format!("{id}@laptop")).await.unwrap();
        // A rule for the ID holds for the tab name too.
        store(
            &app.db,
            "default",
            &format!(
                "version: 2\ndefault: allow\nask: []\ndeny:\n  - {{ who: \"*\", can: attach, to: \"{id}@laptop\" }}\n"
            ),
        )
        .await
        .unwrap();
        let e = check(&app, "default", "billing@laptop", "attach", "recipes@laptop").await.unwrap_err();
        assert!(e.message.contains("deny[0]"), "{}", e.message);
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

    /// The MCP gate: each call is decided as `call` on its targets for the stream's caller, audited, and an approval
    /// `for: once` holds for one call with exactly the arguments the person saw.
    #[tokio::test]
    async fn gate_calls_are_decided_per_target_and_once_approvals_bind_to_arguments() {
        use fleetlayer_protocol::link::ToolCall;
        let (app, _) = example().await;
        store(
            &app.db,
            "default",
            "version: 2\ndefault: allow\nask:\n  - { who: \"*@laptop\", can: call, to: \"tool:executor/call/tools.*.send*@*\", for: once }\n\
             deny:\n  - { who: \"*\", can: call, to: \"tool:executor/execute@*\" }\n",
        )
        .await
        .unwrap();
        let stream =
            app.streams.create(&app.db, "default", "executor", "laptop", "laptop", "recipes@laptop").await.unwrap();
        let call = |targets: &[&str], hash: &str| ToolCall {
            stream_id: stream.clone(),
            targets: targets.iter().map(|t| t.to_string()).collect(),
            args_hash: hash.into(),
            detail: Some("call {\"to\": \"sam@example.com\"}".into()),
            justification: None,
            audit: None,
            peek: None,
        };
        let ask = |c: ToolCall| crate::link::tool_call(&app, "default", "laptop", c);
        assert_eq!(ask(call(&["search"], "h0")).await.unwrap().decision, "allowed");
        assert_eq!(ask(call(&["call/tools.google.gmail.search"], "h1")).await.unwrap().decision, "allowed");
        let d = ask(call(&["execute"], "h2")).await.unwrap();
        assert_eq!(d.decision, "denied", "{}", d.reason);
        // One target needing approval holds up the whole call.
        let d = ask(call(&["call/tools.google.gmail.search", "call/tools.google.gmail.send"], "h3")).await.unwrap();
        assert_eq!(d.decision, "pending", "{}", d.reason);
        let id = d.approval.unwrap();
        let listed = crate::approvals::list(&app.db, "default", false).await.unwrap();
        assert_eq!(
            (listed[0]["detail"].as_str(), listed[0]["once"].as_bool()),
            (Some("call {\"to\": \"sam@example.com\"}"), Some(true))
        );
        assert!(listed[0]["to"].as_str().unwrap().ends_with("@laptop#h3"), "{}", listed[0]["to"]);
        // Asking again while it waits files nothing new.
        assert_eq!(
            ask(call(&["call/tools.google.gmail.send"], "h3")).await.unwrap().approval.as_deref(),
            Some(id.as_str())
        );
        // A peek decides and records nothing.
        let before = log(&app.db, "default").await.len();
        let peek = |id: &str| ToolCall { peek: Some(id.into()), ..call(&["call/tools.google.gmail.send"], "h3") };
        assert_eq!(ask(peek(&id)).await.unwrap().decision, "pending");
        crate::approvals::decide(&app.db, "default", &id, true, None, "admin").await.unwrap();
        assert_eq!(ask(peek(&id)).await.unwrap().decision, "ready");
        assert_eq!(log(&app.db, "default").await.len(), before);
        assert!(ask(peek("ap_other")).await.is_err());
        // Other arguments aren't approved.
        assert_eq!(ask(call(&["call/tools.google.gmail.send"], "other")).await.unwrap().decision, "pending");
        // These arguments are, once.
        let mixed = ["call/tools.google.gmail.search", "call/tools.google.gmail.send"];
        assert_eq!(ask(call(&mixed, "h3")).await.unwrap().decision, "allowed");
        assert_eq!(ask(call(&mixed, "h3")).await.unwrap().decision, "pending", "spent");
        // Only the node serving the stream may ask, and the caller is the stream's.
        let e = crate::link::tool_call(&app, "default", "web", call(&["search"], "h")).await.unwrap_err();
        assert!(e.message.contains("no open tool stream"), "{}", e.message);
        assert!(ask(call(&["x@y"], "h")).await.is_err(), "an @ in a target");
        let logged = log(&app.db, "default").await;
        assert!(logged.contains(&"allowed recipes@laptop call tool:executor/search@laptop".to_string()), "{logged:?}");
        assert!(logged.contains(&"denied recipes@laptop call tool:executor/execute@laptop".to_string()), "{logged:?}");
    }

    /// Review of the MCP gate: an approval `for: once` is spent by exactly one call, however many identical calls
    /// arrive at once, and only when the whole call goes ahead.
    #[tokio::test]
    async fn a_once_approval_is_spent_by_one_call_and_only_when_it_goes_ahead() {
        use fleetlayer_protocol::link::ToolCall;
        let (app, _) = example().await;
        store(
            &app.db,
            "default",
            "version: 2\ndefault: allow\nask:\n  - { who: \"*\", can: call, to: \"tool:x/send@*\", for: once }\n\
             deny:\n  - { who: \"*\", can: call, to: \"tool:x/drop@*\" }\n",
        )
        .await
        .unwrap();
        let stream = app.streams.create(&app.db, "default", "x", "laptop", "laptop", "recipes@laptop").await.unwrap();
        let call = |targets: &[&str]| ToolCall {
            stream_id: stream.clone(),
            targets: targets.iter().map(|t| t.to_string()).collect(),
            args_hash: "same".into(),
            detail: Some("send".into()),
            justification: None,
            audit: None,
            peek: None,
        };
        let ask = |c: ToolCall| crate::link::tool_call(&app, "default", "laptop", c);
        let id = ask(call(&["send"])).await.unwrap().approval.unwrap();
        crate::approvals::decide(&app.db, "default", &id, true, None, "admin").await.unwrap();
        // A call whose other target is refused doesn't spend it.
        assert_eq!(ask(call(&["send", "drop"])).await.unwrap().decision, "denied");
        // Ten identical calls at once: one goes ahead.
        let all = futures::future::join_all((0..10).map(|_| ask(call(&["send"])))).await;
        let allowed = all.iter().filter(|d| d.as_ref().is_ok_and(|d| d.decision == "allowed")).count();
        assert_eq!(allowed, 1, "{:?}", all.iter().map(|d| d.as_ref().map(|d| d.decision.clone())).collect::<Vec<_>>());
    }

    /// Shared commands: a run is decided as `run` on `command:<name>@<node>` for the stream's caller, with its detail
    /// shown to the person and kept in the audit log, and the built-in default asks before each run.
    #[tokio::test]
    async fn command_runs_are_decided_as_run_and_audited_with_their_detail() {
        use fleetlayer_protocol::link::ToolCall;
        let (app, _) = example().await;
        let stream = app
            .streams
            .create(&app.db, "default", "command:deploy", "laptop", "laptop", "recipes@laptop")
            .await
            .unwrap();
        let call = |hash: &str| ToolCall {
            stream_id: stream.clone(),
            targets: vec![],
            args_hash: hash.into(),
            detail: Some("rack-gateway deploy --app web --script fix.rb\n  | puts 1".into()),
            justification: None,
            audit: Some("rack-gateway deploy --app web --script fix.rb (7 bytes, sha256 ab)".into()),
            peek: None,
        };
        let ask = |c: ToolCall| crate::link::command_call(&app, "default", "laptop", c);
        // No policy: the built-in default asks before every run, once.
        let d = ask(call("h1")).await.unwrap();
        assert_eq!(d.decision, "pending", "{}", d.reason);
        let id = d.approval.unwrap();
        let listed = crate::approvals::list(&app.db, "default", false).await.unwrap();
        // The person sees the script whole.
        assert_eq!(listed[0]["detail"].as_str(), Some("rack-gateway deploy --app web --script fix.rb\n  | puts 1"));
        crate::approvals::decide(&app.db, "default", &id, true, None, "admin").await.unwrap();
        assert_eq!(ask(call("h2")).await.unwrap().decision, "pending", "other arguments aren't approved");
        assert_eq!(ask(call("h1")).await.unwrap().decision, "allowed");
        assert_eq!(ask(call("h1")).await.unwrap().decision, "pending", "spent");
        // A run whose files can't be shown whole (no detail) isn't put to the person.
        let d = ask(ToolCall { detail: None, ..call("h9") }).await.unwrap();
        assert_eq!(d.decision, "denied", "{}", d.reason);
        assert!(d.reason.contains("too long to show"), "{}", d.reason);
        // A tool stream isn't a command stream, a run names no targets and always carries its audit text, and only the
        // node serving the stream may ask.
        let tool = app.streams.create(&app.db, "default", "x", "laptop", "laptop", "recipes@laptop").await.unwrap();
        assert!(ask(ToolCall { stream_id: tool, ..call("h") }).await.is_err());
        assert!(ask(ToolCall { targets: vec!["x".into()], ..call("h") }).await.is_err());
        assert!(ask(ToolCall { audit: None, ..call("h") }).await.is_err());
        assert!(crate::link::command_call(&app, "default", "web", call("h")).await.is_err());
        store(
            &app.db,
            "default",
            "version: 2\ndefault: allow\nask: []\ndeny:\n  - { who: \"*\", can: run, to: \"command:drop@*\" }\n",
        )
        .await
        .unwrap();
        assert_eq!(ask(call("h3")).await.unwrap().decision, "allowed");
        // Allowed without asking: it runs even when it couldn't have been shown.
        assert_eq!(ask(ToolCall { detail: None, ..call("h4") }).await.unwrap().decision, "allowed");
        let drop =
            app.streams.create(&app.db, "default", "command:drop", "laptop", "laptop", "recipes@laptop").await.unwrap();
        assert_eq!(ask(ToolCall { stream_id: drop, ..call("h") }).await.unwrap().decision, "denied");
        let logged = log(&app.db, "default").await;
        assert!(logged.contains(&"allowed recipes@laptop run command:deploy@laptop".to_string()), "{logged:?}");
        assert!(logged.contains(&"denied recipes@laptop run command:drop@laptop".to_string()), "{logged:?}");
        let reasons: Vec<String> =
            sqlx::query_scalar("SELECT reason FROM audit_log WHERE verb = 'run'").fetch_all(&app.db).await.unwrap();
        // The audit log keeps the run with its files by hash, never their contents.
        assert!(reasons.iter().all(|r| r.ends_with("--script fix.rb (7 bytes, sha256 ab)")), "{reasons:?}");
        assert!(!reasons.iter().any(|r| r.contains("puts 1")), "{reasons:?}");
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
    async fn a_windowed_role_is_off_until_its_window_opens_and_then_doesnt_ask() {
        let (app, _) = example().await;
        people::agent_admit(&app.db, "default", "ops@web", "admin").await.unwrap();
        let text = "version: 2\ndefault: allow\nroles:\n  admin: { who: \"*@laptop\", activation: window }\n";
        store(&app.db, "default", text).await.unwrap();
        let e = check(&app, "default", "recipes@laptop", "message", "ops@web").await.unwrap_err();
        assert!(e.message.contains("role admin is off until its window is open"), "{}", e.message);
        assert!(e.message.contains("fleetlayer role activate admin"), "it says how to ask: {}", e.message);
        let e = check(&app, "default", "ops@web", "message", "recipes@laptop").await.unwrap_err();
        assert!(e.message.contains("role admin is off"), "nothing to it either: {}", e.message);
        let p = app.authz.policy(&app.db, "default").await.unwrap().unwrap();
        crate::windows::open_now(&app.db, "default", &p, "admin", "1h", "person").await.unwrap();
        check(&app, "default", "recipes@laptop", "message", "ops@web").await.unwrap();
        // The default asks before a first tool use; inside the window it doesn't.
        check(&app, "default", "recipes@laptop", "use_tool", "tool:shell@laptop").await.unwrap();
        let last = log(&app.db, "default").await;
        assert!(last.last().unwrap().starts_with("allowed recipes@laptop use_tool"), "{last:?}");
        crate::windows::deactivate_db(&app.db, "default", "admin", "person").await.unwrap();
        assert!(check(&app, "default", "recipes@laptop", "message", "ops@web").await.is_err(), "off again at once");
    }

    #[tokio::test]
    async fn looking_at_a_once_approval_never_spends_it() {
        let (app, acme) = example().await;
        store(
            &app.db,
            &acme,
            "version: 2\ndefault: allow\nask:\n  - { who: \"*\", can: connect, to: \"host:*\", for: once }\n",
        )
        .await
        .unwrap();
        let req = |peek: bool| fleetlayer_protocol::link::AccessCheck {
            tenant: "acme".into(),
            who: "-@web".into(),
            can: "connect".into(),
            to: "host:example.com".into(),
            why: Some("the API docs for the billing integration".into()),
            peek,
        };
        assert!(!access_check(&app, "web", &req(false)).await.unwrap().allowed);
        let id = crate::approvals::list(&app.db, &acme, false).await.unwrap()[0]["id"].as_str().unwrap().to_string();
        crate::approvals::decide(&app.db, &acme, &id, true, None, "admin").await.unwrap().unwrap();
        // Looking, however often, leaves the one use for the request itself.
        for _ in 0..3 {
            assert!(access_check(&app, "web", &req(true)).await.unwrap().allowed);
        }
        assert!(access_check(&app, "web", &req(false)).await.unwrap().allowed, "the request spends it");
        assert!(!access_check(&app, "web", &req(true)).await.unwrap().allowed, "and then it's gone");
    }

    #[tokio::test]
    async fn a_source_node_asks_before_each_read_of_a_browser_for_cookies() {
        let (app, acme) = example().await;
        let req = |who: &str, to: &str, peek: bool| fleetlayer_protocol::link::AccessCheck {
            tenant: String::new(),
            who: who.into(),
            can: "sync".into(),
            to: to.into(),
            why: Some("read auth_token, ct0 for x.com from Chrome on web for server".into()),
            peek,
        };
        let to = "sync:cookies-from-browser/x@server";
        // Only in its own name, and only about a bundle.
        let e = sync_check(&app, &acme, "web", &req("-@other", to, false)).await.unwrap_err();
        assert!(e.message.contains("only in its own name"), "{}", e.message);
        assert!(sync_check(&app, &acme, "web", &req("-@web", "sync:nonode", false)).await.is_err());
        // Fleetlayer's default asks each time, and the person sees what would be read.
        let d = sync_check(&app, &acme, "web", &req("-@web", to, false)).await.unwrap();
        assert!(!d.allowed && d.reason.contains("approv"), "{d:?}");
        let pending = crate::approvals::list(&app.db, &acme, false).await.unwrap();
        assert_eq!((pending[0]["to"].as_str(), pending[0]["once"].as_bool()), (Some(to), Some(true)));
        assert!(pending[0]["detail"].as_str().unwrap().contains("auth_token, ct0 for x.com"));
        let id = pending[0]["id"].as_str().unwrap().to_string();
        crate::approvals::decide(&app.db, &acme, &id, true, None, "admin").await.unwrap().unwrap();
        assert!(sync_check(&app, &acme, "web", &req("-@web", to, true)).await.unwrap().allowed);
        assert!(sync_check(&app, &acme, "web", &req("-@web", to, true)).await.unwrap().allowed, "looking keeps it");
        assert!(sync_check(&app, &acme, "web", &req("-@web", to, false)).await.unwrap().allowed, "one read");
        assert!(!sync_check(&app, &acme, "web", &req("-@web", to, false)).await.unwrap().allowed, "the next asks");
        // The delivery itself is an ordinary sync, held 30 days.
        assert_eq!(
            crate::policy::Compiled::parse(crate::policy::BUILTIN).unwrap().ask_for(
                "-@web",
                "sync",
                "sync:cookies/x@server"
            ),
            Some(30 * 86_400_000)
        );
    }

    #[tokio::test]
    async fn the_egress_plugin_asks_for_a_host_and_the_person_approves_it() {
        let (app, acme) = example().await;
        let ask = |can: &str, to: &str, why: Option<&str>| fleetlayer_protocol::link::AccessCheck {
            tenant: "acme".into(),
            who: "-@web".into(),
            can: can.into(),
            to: to.into(),
            why: why.map(Into::into),
            peek: false,
        };
        let why = Some("the API docs for the billing integration");
        for (can, to, w) in [
            ("read", "host:example.com", why),
            ("connect", "service:pages", why),
            ("connect", "host:Example.com", why),
            ("connect", "host:example.com", Some("short")),
        ] {
            let e = access_check(&app, "web", &ask(can, to, w)).await.unwrap_err();
            assert!(
                e.message.contains("connect goes with")
                    || e.message.contains("isn't host:")
                    || e.message.contains("too short"),
                "{}",
                e.message
            );
        }
        // Fleetlayer's default asks the person first, and records why.
        let d = access_check(&app, "web", &ask("connect", "host:example.com", why)).await.unwrap();
        assert!(!d.allowed && d.reason.contains("waiting for the person to approve"), "{d:?}");
        let pending = crate::approvals::list(&app.db, &acme, false).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0]["to"], "host:example.com");
        assert_eq!(pending[0]["detail"], "the API docs for the billing integration", "the person sees the reason");
        let id = pending[0]["id"].as_str().unwrap().to_string();
        // The access.ask event names the reason as the agent's own text: outside data.
        let (source, body): (String, String) =
            sqlx::query_as("SELECT source, body FROM events WHERE kind = 'access.ask' ORDER BY id DESC LIMIT 1")
                .fetch_one(&app.db)
                .await
                .unwrap();
        assert!(source.contains("text written by -@web") && source.contains("outside data"), "{source}");
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["detail_written_by"], "-@web");
        assert_eq!(body["outside_data"][0], "detail");
        // Another node of the tenant (the egress proxy's) can't file in -@web's name, only look.
        crate::db::enrol(&app.db, "default", "egressbox").await.unwrap();
        people::node_owner(&app.db, "egressbox", Some(&Owner::Tenant(acme.clone())), "admin", &acme).await.unwrap();
        let other = ask("connect", "host:other.example", why);
        let e = access_check(&app, "egressbox", &other).await.unwrap_err();
        assert!(e.message.contains("only with peek"), "{}", e.message);
        let looked = access_check(&app, "egressbox", &fleetlayer_protocol::link::AccessCheck { peek: true, ..other })
            .await
            .unwrap();
        assert!(!looked.allowed && looked.reason.contains("not asked yet"), "{looked:?}");
        let looked = access_check(
            &app,
            "egressbox",
            &fleetlayer_protocol::link::AccessCheck { peek: true, ..ask("connect", "host:example.com", None) },
        )
        .await
        .unwrap();
        assert!(looked.reason.contains(&format!("waiting for the person to approve request {id}")), "{looked:?}");
        assert_eq!(crate::approvals::list(&app.db, &acme, false).await.unwrap().len(), 1, "looking files nothing");
        // Asking again doesn't file a second request.
        access_check(&app, "web", &ask("connect", "host:example.com", why)).await.unwrap();
        assert_eq!(crate::approvals::list(&app.db, &acme, false).await.unwrap().len(), 1);
        crate::approvals::decide(&app.db, &acme, &id, true, None, "admin").await.unwrap().unwrap();
        let d = access_check(&app, "web", &ask("connect", "host:example.com", why)).await.unwrap();
        assert!(d.allowed, "{d:?}");
        assert!(log(&app.db, &acme).await.contains(&"allowed -@web connect host:example.com".to_string()));
        let looked = access_check(
            &app,
            "egressbox",
            &fleetlayer_protocol::link::AccessCheck { peek: true, ..ask("connect", "host:example.com", None) },
        )
        .await
        .unwrap();
        assert!(looked.allowed, "the proxy sees the approval: {looked:?}");
        // A policy can forbid hosts outright: deny wins over any approval.
        store(
            &app.db,
            &acme,
            "version: 2\ndefault: allow\ndeny:\n  - { who: \"*\", can: connect, to: \"host:example.*\" }\n",
        )
        .await
        .unwrap();
        assert!(!access_check(&app, "web", &ask("connect", "host:example.com", why)).await.unwrap().allowed);
    }

    #[tokio::test]
    async fn an_off_role_reads_nothing_and_nobody_reads_it() {
        let (app, _) = example().await;
        people::agent_admit(&app.db, "default", "ops@web", "admin").await.unwrap();
        let text = "version: 2\ndefault: allow\nroles:\n  admin: { who: \"*@laptop\", activation: window }\n";
        store(&app.db, "default", text).await.unwrap();
        assert!(!may_read(&app, "default", "recipes@laptop", "ops@web").await, "the fleet view, summaries, readings");
        assert!(!may_see(&app, "default", "recipes@laptop", "ops@web").await);
        assert!(!may_see(&app, "default", "ops@web", "recipes@laptop").await, "others don't see its sessions");
        let p = app.authz.policy(&app.db, "default").await.unwrap().unwrap();
        crate::windows::open_now(&app.db, "default", &p, "admin", "1h", "person").await.unwrap();
        assert!(may_read(&app, "default", "recipes@laptop", "ops@web").await);
        assert!(may_see(&app, "default", "ops@web", "recipes@laptop").await);
    }

    #[tokio::test]
    async fn a_service_asks_about_people_in_a_tenant_that_admitted_it() {
        let (app, acme) = example().await;
        let ask = |who: &str, can: &str, to: &str| fleetlayer_protocol::link::AccessCheck {
            tenant: "acme".into(),
            who: who.into(),
            can: can.into(),
            to: to.into(),
            ..Default::default()
        };
        // `web` is Acme's own node, so it may ask; A's laptop isn't (only billing@laptop is admitted).
        let e = access_check(&app, "laptop", &ask("person:a", "sign_in", "service:pages")).await.unwrap_err();
        assert!(e.message.contains("isn't a member"), "{}", e.message);
        // Without a policy, people have their own things, and admins the tenant's services.
        let d = access_check(&app, "web", &ask("person:a", "sign_in", "service:pages")).await.unwrap();
        assert!(d.allowed, "{d:?}");
        assert!(d.rules[0].starts_with("default_admins"), "{d:?}");
        assert!(
            access_check(&app, "web", &ask("person:a", "read", "service:quietdeck/a/inbox")).await.unwrap().allowed
        );
        // A policy can keep a service from its admins too.
        store(
            &app.db,
            &acme,
            "version: 2\ndefault: allow\ndeny:\n  - { who: role:admin, can: sign_in, to: service:pages }\n",
        )
        .await
        .unwrap();
        let d = access_check(&app, "web", &ask("person:a", "sign_in", "service:pages")).await.unwrap();
        assert!(!d.allowed, "{d:?}");
        assert_eq!(d.rules, ["deny[0]"]);
        store(&app.db, &acme, "version: 2\ndefault: allow\n").await.unwrap();
        // By ID (the sign-in's `sub`) too; someone who isn't a member is refused.
        let id = people::resolve_person(&app.db, &acme, "a").await.unwrap();
        assert!(access_check(&app, "web", &ask(&id, "sign_in", "service:pages")).await.unwrap().allowed);
        let d = access_check(&app, "web", &ask("person:b", "sign_in", "service:pages")).await.unwrap();
        assert!(!d.allowed && d.reason.contains("isn't a member of acme"), "{d:?}");
        // Only about a tenant's own things.
        assert!(access_check(&app, "web", &ask("person:a", "read", "billing@laptop")).await.is_err());
        assert!(log(&app.db, &acme).await.iter().any(|l| l == "allowed person:a sign_in service:pages"));
    }

    /// Every method the control plane's link handler answers is in LINK_METHODS, read from the handler's own source,
    /// so a new method can't be added to the match and forgotten here (transcript reads were, from v0.3.13 to v0.3.14:
    /// the fail-closed check refused them as unknown).
    #[test]
    fn every_method_the_link_handler_answers_is_listed() {
        let src = include_str!("link.rs");
        let proto = include_str!("../../fleetlayer-protocol/src/link.rs");
        let listed: Vec<&str> = LINK_METHODS.iter().map(|(m, _)| *m).collect();
        let start = src.find("impl Handler for FromNode").expect("the node handler");
        let body = &src[start..];
        let end = body.find("_ => Err(RpcError::method_not_found(method))").expect("the handler's last arm");
        let mut missing = vec![];
        for line in body[..end].lines() {
            let t = line.trim_start();
            if let Some(rest) = t.strip_prefix("l::method::") {
                let name: String = rest.chars().take_while(|c| c.is_ascii_uppercase() || *c == '_').collect();
                if !rest[name.len()..].trim_start().starts_with("=>")
                    && !rest[name.len()..].trim_start().starts_with('|')
                {
                    continue;
                }
                let decl = format!("pub const {name}: &str = \"");
                let value = proto.find(&decl).map(|i| {
                    let rest = &proto[i + decl.len()..];
                    &rest[..rest.find('"').unwrap_or(0)]
                });
                match value {
                    Some(v) if listed.contains(&v) => {}
                    _ => missing.push(name),
                }
            }
        }
        assert!(missing.is_empty(), "answered by the link handler but not in LINK_METHODS: {missing:?}");
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
            m::EVENTS,
            m::FLEET,
            m::SUMMARY,
            m::SUMMARIES_SEARCH,
            m::TOOL_OPEN,
            m::TOOL_CALL,
            m::COMMAND_OPEN,
            m::COMMAND_CALL,
            m::TERMINAL_OPEN,
            m::TRANSCRIPT_READ,
            m::TRANSCRIPT_SEARCH,
            m::SYNC_PUBLISH,
            m::READINGS,
            m::READING_SUBSCRIBE,
            m::READING_LATEST,
            m::ACCESS_CHECK,
            m::PAGES_SPACES,
            m::PAGES_OPEN,
            m::SESSION_START,
            m::SESSION_HANDOFF,
            m::HANDOFF_GET,
            m::SESSION_CLOSE,
            m::SESSION_ANSWER,
            m::TOOLS,
            m::ME,
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
