//! Authorization for every request the control plane handles. A version 2 policy (`policy.rs`), once applied, decides
//! everything; before one is applied, the version 1 rules (`acl.rs`) decide messages, tools and sync as before. Every
//! refusal is published as `access.denied`, every allowed request an audit rule would refuse as `access.audit`.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fleetlayer_protocol::link::method as m;
use fleetlayer_protocol::rpc::RpcError;
use sqlx::Row;
use tokio::sync::{Mutex, RwLock};

use crate::App;
use crate::db::{Db, now_ms};
use crate::policy::Compiled;

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
];

#[derive(Default)]
pub struct Authz {
    policy: RwLock<Option<(String, Arc<Compiled>)>>,
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
    /// The current version 2 policy, reloaded when the stored text changed (`acl apply` from another process).
    pub async fn policy(&self, db: &Db, tenant: &str) -> anyhow::Result<Option<Arc<Compiled>>> {
        let text = stored(db, tenant).await?;
        {
            let cur = self.policy.read().await;
            match (&*cur, &text) {
                (Some((t, c)), Some(new)) if t == new => return Ok(Some(c.clone())),
                (None, None) => return Ok(None),
                _ => {}
            }
        }
        let mut cur = self.policy.write().await;
        *cur = match text {
            Some(t) => {
                let c = Arc::new(Compiled::parse(&t)?);
                tracing::info!("access policy (version 2) loaded");
                Some((t, c))
            }
            None => None,
        };
        Ok(cur.as_ref().map(|(_, c)| c.clone()))
    }
}

/// Whether `who` may `verb` `target`, publishing refusals and audit matches. Err is the refusal to send back.
pub async fn check(app: &App, tenant: &str, who: &str, verb: &str, target: &str) -> Result<(), RpcError> {
    check_why(app, tenant, who, verb, target, None).await
}

/// [`check`], with the request's justification, which `justify:` rules can require; it goes in the audit log.
pub async fn check_why(
    app: &App,
    tenant: &str,
    who: &str,
    verb: &str,
    target: &str,
    why: Option<&str>,
) -> Result<(), RpcError> {
    let reply = crate::acl::replying(&app.db, tenant, who, target).await.map_err(RpcError::internal)?;
    let policy = app.authz.policy(&app.db, tenant).await.map_err(RpcError::internal)?;
    let (allowed, because) = match &policy {
        Some(p) => {
            let d = p.decide(who, verb, target, reply).map_err(RpcError::internal)?;
            if d.allowed && !d.audit_would_deny.is_empty() {
                audit(app, tenant, who, verb, target, &d.audit_would_deny).await;
            }
            (d.allowed, d.because.join(", "))
        }
        None => {
            // Version 1: rules on messages, tools (tool:…) and sync (sync:…); everything else allowed.
            let v1_target = target.to_string();
            let v1 = matches!(verb, "message" | "use_tool" | "sync");
            let ok = !v1
                || reply
                || crate::acl::allowed(&app.db, tenant, who, &v1_target).await.map_err(RpcError::internal)?;
            (ok, "a version 1 rule".to_string())
        }
    };
    // An allowed request a `justify:` rule covers must say why.
    let mut no_reason: Option<String> = None;
    if allowed
        && let Some(p) = &policy
        && p.needs_justification(who, verb, target)
    {
        no_reason = match why {
            None => Some("its access rules need a reason: give one with --why \"…\" (one line)".into()),
            Some(w) => crate::policy::check_justification(w).err(),
        };
    }
    // An allowed request an `ask:` rule covers needs the person's approval first.
    let mut waiting: Option<String> = None;
    if allowed
        && no_reason.is_none()
        && let Some(p) = &policy
        && let Some(hold) = p.ask_for(who, verb, target)
    {
        match crate::approvals::check(app, tenant, who, verb, target, hold).await.map_err(RpcError::internal)? {
            crate::approvals::Status::Approved => {}
            crate::approvals::Status::Pending(id) => {
                waiting = Some(format!("waiting for the person to approve request {id}"))
            }
            crate::approvals::Status::Denied(id) => waiting = Some(format!("the person refused request {id}")),
        }
    }
    let allowed = allowed && no_reason.is_none() && waiting.is_none();
    let reason = match (&no_reason, &waiting, why) {
        (Some(r), _, _) | (None, Some(r), _) => r.clone(),
        (None, None, Some(w)) => format!("{because}; why: {w}"),
        (None, None, None) => because.clone(),
    };
    // Fail closed: nothing goes ahead without its entry in the audit log.
    let entry = crate::audit::Entry {
        decision: if waiting.is_some() {
            "asked"
        } else if allowed {
            "allowed"
        } else {
            "denied"
        }
        .into(),
        subject: who.into(),
        verb: verb.into(),
        target: target.into(),
        reason,
    };
    if let Err(e) = app.audit.record(&app.db, tenant, entry).await {
        tracing::error!("the audit log can't be written, so refusing {who} {verb} {target}: {e}");
        return Err(RpcError::refused("the control plane's audit log can't be written now, so nothing is allowed"));
    }
    if allowed {
        return Ok(());
    }
    if let Some(w) = waiting {
        return Err(RpcError::refused(format!(
            "{who} needs its person's approval to {verb} {target}: {w}. Try again once they've approved it"
        )));
    }
    if let Some(r) = no_reason {
        return Err(RpcError::refused(format!("{who} may {verb} {target} only with a reason: {r}")));
    }
    crate::route::denied(app, tenant, who, target, verb).await;
    Err(RpcError::refused(format!(
        "{who} may not {verb} {target} (access control: {because}). If this stops you doing your job, report it to \
         your person"
    )))
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

/// Whether `who` sending to `to` now stays within its rate limits; records the message when it does.
pub async fn within_limits(app: &App, tenant: &str, who: &str, to: &str) -> Result<(), RpcError> {
    let Some(policy) = app.authz.policy(&app.db, tenant).await.map_err(RpcError::internal)? else { return Ok(()) };
    let (per_hour, distinct) = policy.limits_for(who);
    let mut sent = app.authz.sent.lock().await;
    let log = sent.entry(format!("{tenant}/{who}")).or_default();
    let now = Instant::now();
    while log.front().is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(3600)) {
        log.pop_front();
    }
    if let Some(n) = per_hour
        && log.len() >= n as usize
    {
        drop(sent);
        crate::route::denied(app, tenant, who, to, "message (rate limit)").await;
        return Err(RpcError::refused(format!("{who} has sent {n} messages in the last hour, its limit")));
    }
    if let Some(n) = distinct {
        let mut seen: Vec<&str> = log.iter().map(|(_, t)| t.as_str()).collect();
        seen.sort();
        seen.dedup();
        if !seen.contains(&to) && seen.len() >= n as usize {
            drop(sent);
            crate::route::denied(app, tenant, who, to, "message (recipient limit)").await;
            return Err(RpcError::refused(format!(
                "{who} has messaged {n} different sessions in the last hour, its limit"
            )));
        }
    }
    log.push_back((now, to.to_string()));
    Ok(())
}

/// Whether messages from `who` are delivered as outside data.
pub async fn untrusted(app: &App, tenant: &str, who: &str) -> bool {
    matches!(app.authz.policy(&app.db, tenant).await, Ok(Some(p)) if p.untrusted(who))
}

/// Whether `who` may read `target`, for filtering lists (no event per filtered item).
pub async fn may_read(app: &App, tenant: &str, who: &str, target: &str) -> bool {
    match app.authz.policy(&app.db, tenant).await {
        Ok(Some(p)) => p.decide(who, "read", target, false).is_ok_and(|d| d.allowed),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
