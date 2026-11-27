//! Windows of roles with `activation: window` (the admin window, or "sudo mode"; `docs/access-control.md`). Such a
//! role has no rights at all until the person approves a window: every request by or to its agents is refused. Asking
//! (`fleetlayer role activate <role> --for 1h`) files an ordinary approval (verb `activate`, target `window:<role>`),
//! so it reaches the person the way every other approval does; the window is open while that approval holds, capped at
//! the role's `longest:`. While open, the role's rules decide, and its `ask:` rules don't ask again: the window's
//! approval covers them.
//!
//! A loop opens and closes windows ([`run`]): it records each window, writes it to the audit log, publishes
//! `access.window.opened` and `access.window.closed` on the `access` channel, and tells the role's nodes, which close
//! the role's sessions, remove what was delivered to them and run their hook at the end, then confirm. Each node also
//! knows when the window ends and closes it by itself then, linked or not. A window whose nodes haven't confirmed within
//! five minutes of its end shows in `/healthz` until they do.

use std::sync::Arc;
use std::time::Duration;

use fleetlayer_protocol::ext::Event;
use fleetlayer_protocol::link::{self as l, RoleActivated, WindowClosed, WindowInfo, WindowNotice};
use fleetlayer_protocol::rpc::RpcError;
use sqlx::Row;

use crate::App;
use crate::db::{Db, now_ms};
use crate::policy::Compiled;

/// How often the loop looks for windows to open and close.
const EVERY: Duration = Duration::from_secs(5);
/// How long a role's nodes have to confirm a closed window before it shows as a problem.
const CONFIRM_WITHIN_MS: i64 = 5 * 60_000;
/// Closed windows are told again to nodes that haven't confirmed, this often.
const RETELL_MS: i64 = 60_000;
/// Nodes that only watch a role hear of windows closed this recently when they link.
const WATCHERS_HEAR_MS: i64 = 86_400_000;

pub fn target(role: &str) -> String {
    format!("window:{role}")
}

/// The open window of `role`: (its ID, when it ends). Read from the approval itself, so a window opens the moment the
/// person approves it (from any process) and closes the moment it's closed early; the end is capped at `longest_ms`
/// after the approval.
pub async fn active(db: &Db, tenant: &str, role: &str, longest_ms: i64) -> anyhow::Result<Option<(String, i64)>> {
    let now = now_ms();
    let row = sqlx::query(
        "SELECT a.id, MIN(a.expires_at, a.decided_at + ?) AS ends FROM approvals a
         WHERE a.tenant = ? AND a.verb = 'activate' AND a.target = ? AND a.status = 'approved'
         AND NOT EXISTS (SELECT 1 FROM role_windows w WHERE w.id = a.id AND w.closed_at IS NOT NULL)
         AND MIN(a.expires_at, a.decided_at + ?) > ?
         ORDER BY ends DESC LIMIT 1",
    )
    .bind(longest_ms)
    .bind(tenant)
    .bind(target(role))
    .bind(longest_ms)
    .bind(now)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|r| (r.get(0), r.get(1))))
}

/// A pending request for `role`'s window, if any.
async fn pending(db: &Db, tenant: &str, role: &str) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM approvals WHERE tenant = ? AND verb = 'activate' AND target = ? AND status = 'pending'
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(tenant)
    .bind(target(role))
    .fetch_optional(db)
    .await?)
}

/// The windowed roles `name` is one of the agents of whose window isn't open: why each refuses. Empty when none does.
pub async fn closed_roles(db: &Db, tenant: &str, p: &Compiled, name: &str) -> anyhow::Result<Vec<String>> {
    let mut out = vec![];
    for role in p.window_roles(name) {
        let longest = p.file.roles[role].longest_ms();
        if active(db, tenant, role, longest).await?.is_none() {
            out.push(role.to_string());
        }
    }
    Ok(out)
}

/// The open windows `name` is in: (role, window ID, ends at).
pub async fn open_for(db: &Db, tenant: &str, p: &Compiled, name: &str) -> anyhow::Result<Vec<(String, String, i64)>> {
    let mut out = vec![];
    for role in p.window_roles(name) {
        if let Some((id, ends)) = active(db, tenant, role, p.file.roles[role].longest_ms()).await? {
            out.push((role.to_string(), id, ends));
        }
    }
    Ok(out)
}

pub fn when(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms).map(|t| t.strftime("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default()
}

fn refused(msg: String) -> RpcError {
    RpcError::refused(msg)
}

/// Ask for `role`'s window for `hold` (`1h`), as `who`.
pub async fn request(
    app: &App,
    tenant: &str,
    p: &Compiled,
    who: &str,
    role: &str,
    hold: &str,
    why: Option<&str>,
) -> Result<RoleActivated, RpcError> {
    let def = p
        .file
        .roles
        .get(role)
        .filter(|r| r.windowed())
        .ok_or_else(|| refused(format!("there's no role {role} with `activation: window` in the access policy")))?;
    if !p.may_ask_window(role, who) {
        return Err(refused(format!(
            "{who} may not ask for {role}'s window: only its own agents and its `asked_by:` may"
        )));
    }
    let hold_ms = crate::policy::parse_duration_ms(hold).map_err(|e| RpcError::invalid_params(e.to_string()))?;
    let longest = def.longest_ms();
    if hold_ms <= 0 || hold_ms > longest {
        return Err(refused(format!(
            "{role}'s window can be at most {} long (its `longest:`), and {hold} isn't",
            human(longest)
        )));
    }
    if let Some(w) = why {
        crate::policy::check_justification(w).map_err(refused)?;
    }
    let db = &app.db;
    if let Some((id, ends)) = active(db, tenant, role, longest).await.map_err(RpcError::internal)? {
        return Ok(RoleActivated {
            role: role.into(),
            state: "open".into(),
            request: Some(id),
            ends_at: Some(ends),
            message: format!("{role}'s window is open already, until {}", when(ends)),
        });
    }
    let status = crate::approvals::check(app, tenant, who, "activate", &target(role), hold_ms)
        .await
        .map_err(RpcError::internal)?;
    let reason = match why {
        Some(w) => format!("asked for {role}'s window for {hold}; why: {w}"),
        None => format!("asked for {role}'s window for {hold}"),
    };
    record(app, tenant, "asked", who, role, reason).await?;
    Ok(match status {
        crate::approvals::Status::Pending(id) => RoleActivated {
            role: role.into(),
            state: "pending".into(),
            request: Some(id.clone()),
            ends_at: None,
            message: format!(
                "asked the person to open {role}'s window for {hold} (request {id}); it opens when they approve, and \
                 `fleetlayer role list` shows it"
            ),
        },
        crate::approvals::Status::Denied(id) => {
            return Err(refused(format!(
                "the person refused {role}'s window (request {id}) within the last hour: ask again later, or ask them"
            )));
        }
        crate::approvals::Status::Approved => RoleActivated {
            role: role.into(),
            state: "open".into(),
            request: None,
            ends_at: None,
            message: format!("{role}'s window is approved and opens within seconds"),
        },
    })
}

/// Close `role`'s window now, as `who` (`fleetlayer role deactivate`, or the person). Ok(false) when none was open.
pub async fn deactivate(app: &App, tenant: &str, role: &str, who: &str) -> anyhow::Result<bool> {
    let closed = deactivate_db(&app.db, tenant, role, who).await?;
    if closed {
        tick(app).await?;
    }
    Ok(closed)
}

/// [`deactivate`] in the database alone (`fleetlayer-control role deactivate`, another process): the server's loop
/// closes the window within seconds.
pub async fn deactivate_db(db: &Db, tenant: &str, role: &str, who: &str) -> anyhow::Result<bool> {
    let now = now_ms();
    let r = sqlx::query(
        "UPDATE approvals SET expires_at = ? WHERE tenant = ? AND verb = 'activate' AND target = ?
         AND status = 'approved' AND expires_at > ?",
    )
    .bind(now)
    .bind(tenant)
    .bind(target(role))
    .bind(now)
    .execute(db)
    .await?;
    // A pending request is withdrawn too: closing means "not now".
    sqlx::query(
        "UPDATE approvals SET status = 'denied', decided_at = ?, expires_at = ? WHERE tenant = ? AND verb = 'activate'
         AND target = ? AND status = 'pending'",
    )
    .bind(now)
    .bind(now)
    .bind(tenant)
    .bind(target(role))
    .execute(db)
    .await?;
    sqlx::query(
        "UPDATE role_windows SET ends_at = MIN(ends_at, ?), closed_by = ? WHERE tenant = ? AND role = ? AND closed_at IS NULL",
    )
    .bind(now)
    .bind(who)
    .bind(tenant)
    .bind(role)
    .execute(db)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// `1h`, `12h`, `1d`, `90m` from milliseconds.
pub fn human(ms: i64) -> String {
    match ms {
        m if m % 86_400_000 == 0 => format!("{}d", m / 86_400_000),
        m if m % 3_600_000 == 0 => format!("{}h", m / 3_600_000),
        m => format!("{}m", m / 60_000),
    }
}

/// The windowed roles `who` is one of the agents of or may ask for, and their state (`fleetlayer role list`, `me`).
pub async fn list_for(db: &Db, tenant: &str, p: &Compiled, who: Option<&str>) -> anyhow::Result<Vec<WindowInfo>> {
    let mut out = vec![];
    for (name, def) in p.file.roles.iter().filter(|(_, r)| r.windowed()) {
        let member = who.is_some_and(|w| def.who.items().iter().any(|p| crate::acl::glob(p, w)));
        if let Some(w) = who
            && !member
            && !p.may_ask_window(name, w)
        {
            continue;
        }
        let longest_ms = def.longest_ms();
        let info = match active(db, tenant, name, longest_ms).await? {
            Some((id, ends)) => WindowInfo {
                role: name.clone(),
                state: "open".into(),
                ends_at: Some(ends),
                id: Some(id),
                longest_ms,
                member,
            },
            None => match pending(db, tenant, name).await? {
                Some(id) => WindowInfo {
                    role: name.clone(),
                    state: "pending".into(),
                    id: Some(id),
                    longest_ms,
                    member,
                    ..Default::default()
                },
                None => {
                    WindowInfo { role: name.clone(), state: "off".into(), longest_ms, member, ..Default::default() }
                }
            },
        };
        out.push(info);
    }
    Ok(out)
}

async fn record(
    app: &App,
    tenant: &str,
    decision: &str,
    who: &str,
    role: &str,
    reason: String,
) -> Result<(), RpcError> {
    let e = crate::audit::Entry {
        decision: decision.into(),
        subject: who.into(),
        verb: "activate".into(),
        target: target(role),
        reason,
    };
    app.audit.record(&app.db, tenant, e).await.map_err(|err| {
        tracing::error!("the audit log can't be written, so refusing a window for {role}: {err}");
        RpcError::refused("the control plane's audit log can't be written now, so nothing is allowed")
    })
}

async fn publish(app: &App, tenant: &str, kind: &str, role: &str, id: &str, summary: String, ends_at: i64) {
    let e = Event {
        channel: "access".into(),
        kind: kind.into(),
        subject: Some(target(role)),
        at: jiff::Timestamp::now(),
        dedupe_key: Some(format!("{kind}:{id}")),
        summary,
        body: serde_json::json!({"id": id, "role": role, "endsAt": ends_at}),
    };
    let _ = crate::route::publish(app, tenant, e, "fleetlayer access control").await;
}

/// The nodes whose agents are the role's: (home tenant, node), members of `tenant`.
async fn role_nodes(app: &App, tenant: &str, p: &Compiled, role: &str) -> anyhow::Result<Vec<(String, String)>> {
    let Some(def) = p.file.roles.get(role) else { return Ok(vec![]) };
    let fleet = app.authz.fleet(&app.db).await?;
    let rows = sqlx::query("SELECT tenant, name FROM nodes ORDER BY name").fetch_all(&app.db).await?;
    let pats = def.who.items();
    Ok(rows
        .into_iter()
        .map(|r| (r.get::<String, _>(0), r.get::<String, _>(1)))
        .filter(|(_, n)| {
            let me = format!("-@{n}");
            pats.iter().any(|w| crate::acl::glob(w, &me))
                && fleet.tenant(tenant).is_none_or(|t| t.is_member(&fleet, &me))
        })
        .collect())
}

/// Tell `node` (in its home tenant) about a window, if it's linked.
async fn tell(app: &App, home: &str, node: &str, n: &WindowNotice) -> bool {
    match app.links.peer(home, node).await {
        Some(peer) => {
            peer.notify(l::method::WINDOW, n);
            true
        }
        None => false,
    }
}

/// Tell the role's nodes and the nodes watching it.
async fn tell_all(app: &App, tenant: &str, members: &[(String, String)], n: &WindowNotice) {
    for (home, node) in members {
        tell(app, home, node, &WindowNotice { member: true, ..n.clone() }).await;
    }
    let watchers: Vec<(String, String)> = app
        .links
        .watchers
        .lock()
        .await
        .iter()
        .filter(|(_, roles)| roles.contains(&n.role))
        .map(|(k, _)| k.clone())
        .collect();
    let fleet = app.authz.fleet(&app.db).await.ok();
    for (home, node) in watchers {
        if members.iter().any(|(h, m)| *h == home && *m == node) {
            continue;
        }
        let member_of_tenant = home == tenant
            || fleet
                .as_ref()
                .and_then(|f| f.tenant(tenant).map(|t| t.is_member(f, &format!("-@{node}"))))
                .unwrap_or(false);
        if member_of_tenant {
            tell(app, &home, &node, &WindowNotice { member: false, ..n.clone() }).await;
        }
    }
}

/// Open windows the person approved, close the ones that ended, and tell nodes that haven't confirmed again.
pub async fn tick(app: &App) -> anyhow::Result<()> {
    let db = &app.db;
    let now = now_ms();
    // Opened: approved, and not recorded yet.
    let rows = sqlx::query(
        "SELECT a.tenant, a.id, a.target, a.subject, a.decided_at, a.expires_at FROM approvals a
         WHERE a.verb = 'activate' AND a.status = 'approved' AND a.expires_at > ?
         AND NOT EXISTS (SELECT 1 FROM role_windows w WHERE w.id = a.id)",
    )
    .bind(now)
    .fetch_all(db)
    .await?;
    for r in rows {
        let (tenant, id, tgt, by, decided, expires): (String, String, String, String, i64, i64) =
            (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5));
        let Some(role) = tgt.strip_prefix("window:") else { continue };
        let Some(p) = app.authz.policy(db, &tenant).await? else { continue };
        let Some(def) = p.file.roles.get(role).filter(|d| d.windowed()) else { continue };
        let ends = expires.min(decided + def.longest_ms());
        if ends <= now {
            continue;
        }
        // A tick from `deactivate` can run beside the loop's: only the one that records the window goes on.
        let inserted = sqlx::query(
            "INSERT OR IGNORE INTO role_windows (tenant, id, role, requested_by, opened_at, ends_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&tenant)
        .bind(&id)
        .bind(role)
        .bind(&by)
        .bind(decided)
        .bind(ends)
        .execute(db)
        .await?
        .rows_affected();
        if inserted == 0 {
            continue;
        }
        let summary = format!("{role}'s window is open until {} (asked by {by}, window {id})", when(ends));
        tracing::info!(role, id, ends, "a window opened");
        let _ = record(app, &tenant, "allowed", &by, role, format!("window {id} opened until {}", when(ends))).await;
        publish(app, &tenant, "access.window.opened", role, &id, summary, ends).await;
        let members = role_nodes(app, &tenant, &p, role).await?;
        let n = WindowNotice { id: id.clone(), role: role.into(), open: true, ends_at: ends, member: true };
        tell_all(app, &tenant, &members, &n).await;
        kick_sync(app, &members).await;
    }
    // Ended: recorded, not closed, and past its end (or its approval was cut short).
    let rows = sqlx::query(
        "SELECT w.tenant, w.id, w.role, w.requested_by, MIN(w.ends_at, COALESCE(a.expires_at, w.ends_at)), w.closed_by
         FROM role_windows w LEFT JOIN approvals a ON a.id = w.id
         WHERE w.closed_at IS NULL AND MIN(w.ends_at, COALESCE(a.expires_at, w.ends_at)) <= ?",
    )
    .bind(now)
    .fetch_all(db)
    .await?;
    for r in rows {
        let (tenant, id, role, by, ends, closed_by): (String, String, String, String, i64, Option<String>) =
            (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5));
        let members = match app.authz.policy(db, &tenant).await? {
            Some(p) => role_nodes(app, &tenant, &p, &role).await?,
            None => vec![],
        };
        let pending: Vec<String> = members.iter().map(|(_, n)| n.clone()).collect();
        let closed = sqlx::query(
            "UPDATE role_windows SET closed_at = ?, ends_at = ?, pending_nodes = ? WHERE id = ? AND closed_at IS NULL",
        )
        .bind(now)
        .bind(ends)
        .bind(serde_json::to_string(&pending)?)
        .bind(&id)
        .execute(db)
        .await?
        .rows_affected();
        if closed == 0 {
            continue;
        }
        let how = match &closed_by {
            Some(c) => format!("closed early by {c}"),
            None => "its time ran out".into(),
        };
        tracing::info!(role, id, how, "a window closed");
        let _ = record(
            app,
            &tenant,
            "closed",
            closed_by.as_deref().unwrap_or(&by),
            &role,
            format!("window {id} closed: {how}"),
        )
        .await;
        publish(
            app,
            &tenant,
            "access.window.closed",
            &role,
            &id,
            format!("{role}'s window closed ({how}, window {id})"),
            ends,
        )
        .await;
        let n = WindowNotice { id: id.clone(), role: role.clone(), open: false, ends_at: ends, member: true };
        tell_all(app, &tenant, &members, &n).await;
    }
    // Closed, but some of the role's nodes haven't confirmed: tell them again, once a minute.
    let rows = sqlx::query(
        "SELECT tenant, id, role, ends_at, closed_at, pending_nodes FROM role_windows
         WHERE closed_at IS NOT NULL AND pending_nodes != '[]' AND closed_at > ?",
    )
    .bind(now - 7 * 86_400_000)
    .fetch_all(db)
    .await?;
    for r in rows {
        let (tenant, id, role, ends, closed, pending): (String, String, String, i64, i64, String) =
            (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5));
        if now - closed < RETELL_MS || (now - closed) % RETELL_MS >= EVERY.as_millis() as i64 {
            continue;
        }
        let pending: Vec<String> = serde_json::from_str(&pending).unwrap_or_default();
        let n = WindowNotice { id, role, open: false, ends_at: ends, member: true };
        for node in pending {
            tell(app, &home_of(db, &node).await.unwrap_or_else(|| tenant.clone()), &node, &n).await;
        }
    }
    Ok(())
}

async fn home_of(db: &Db, node: &str) -> Option<String> {
    sqlx::query_scalar("SELECT tenant FROM nodes WHERE name = ? LIMIT 1").bind(node).fetch_optional(db).await.ok()?
}

/// Bundles owed to the role's nodes are held back while its window is off (the sync is refused, and the source
/// retries with a backoff); ask the sources to send them now.
async fn kick_sync(app: &App, members: &[(String, String)]) {
    for (home, node) in members {
        crate::link::send_owed(app, home, node).await;
    }
}

pub async fn run(app: Arc<App>) {
    loop {
        if let Err(e) = tick(&app).await {
            tracing::warn!("opening and closing role windows: {e}");
        }
        tokio::time::sleep(EVERY).await;
    }
}

/// A role's node confirms it closed a window.
pub async fn confirmed(app: &App, node: &str, c: &WindowClosed) -> Result<(), RpcError> {
    let db = &app.db;
    let row = sqlx::query("SELECT tenant, role, pending_nodes FROM role_windows WHERE id = ?")
        .bind(&c.id)
        .fetch_optional(db)
        .await
        .map_err(RpcError::internal)?
        .ok_or_else(|| RpcError::invalid_params(format!("no window {}", c.id)))?;
    let (tenant, role, pending): (String, String, String) = (row.get(0), row.get(1), row.get(2));
    let mut pending: Vec<String> = serde_json::from_str(&pending).unwrap_or_default();
    let reason = match &c.problem {
        None => {
            pending.retain(|n| n != node);
            format!("window {} closed on {node}: sessions closed, deliveries removed, hook run", c.id)
        }
        Some(p) => format!("window {} didn't close cleanly on {node}: {p}", c.id),
    };
    sqlx::query("UPDATE role_windows SET pending_nodes = ?, problem = ? WHERE id = ?")
        .bind(serde_json::to_string(&pending).map_err(|e| RpcError::internal(e.to_string()))?)
        .bind(c.problem.as_deref().map(|p| format!("{node}: {p}")))
        .bind(&c.id)
        .execute(db)
        .await
        .map_err(RpcError::internal)?;
    record(app, &tenant, if c.problem.is_some() { "problem" } else { "closed" }, &format!("-@{node}"), &role, reason)
        .await
}

/// When `node` links: tell it about open windows of roles it's in or watches, and closed ones it hasn't confirmed.
pub async fn on_link(app: &App, home: &str, node: &str, watched: &[String]) -> anyhow::Result<()> {
    let db = &app.db;
    app.links.watchers.lock().await.insert((home.to_string(), node.to_string()), watched.to_vec());
    let now = now_ms();
    let rows = sqlx::query(
        "SELECT w.tenant, w.id, w.role, w.ends_at, w.closed_at, w.pending_nodes FROM role_windows w
         WHERE w.closed_at IS NULL OR w.closed_at > ?",
    )
    .bind(now - WATCHERS_HEAR_MS)
    .fetch_all(db)
    .await?;
    for r in rows {
        let (tenant, id, role, ends, closed, pending): (String, String, String, i64, Option<i64>, String) =
            (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5));
        let Some(p) = app.authz.policy(db, &tenant).await? else { continue };
        let member = role_nodes(app, &tenant, &p, &role).await?.iter().any(|(h, n)| h == home && n == node);
        let pending: Vec<String> = serde_json::from_str(&pending).unwrap_or_default();
        let open = closed.is_none()
            && active(db, &tenant, &role, p.file.roles.get(&role).map_or(0, |d| d.longest_ms())).await?.is_some();
        let tell_it = if member { open || pending.iter().any(|n| n == node) } else { watched.contains(&role) };
        if tell_it {
            tell(app, home, node, &WindowNotice { id, role, open, ends_at: ends, member }).await;
        }
    }
    Ok(())
}

/// Problems for `/healthz`: windows that ended without closing, or whose nodes haven't confirmed in time.
pub async fn problems(db: &Db) -> Vec<String> {
    let now = now_ms();
    let mut out = vec![];
    let rows = sqlx::query(
        "SELECT role, id, ends_at, closed_at, pending_nodes, problem FROM role_windows
         WHERE (closed_at IS NULL AND ends_at < ?) OR (closed_at IS NOT NULL AND closed_at < ? AND pending_nodes != '[]')",
    )
    .bind(now - 2 * 60_000)
    .bind(now - CONFIRM_WITHIN_MS)
    .fetch_all(db)
    .await;
    for r in rows.into_iter().flatten() {
        let (role, id, ends, closed, pending, problem): (String, String, i64, Option<i64>, String, Option<String>) =
            (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4), r.get(5));
        out.push(match closed {
            None => format!(
                "{role}'s window {id} ended at {} but hasn't been closed (is the server's loop running?)",
                when(ends)
            ),
            Some(_) => {
                let nodes: Vec<String> = serde_json::from_str(&pending).unwrap_or_default();
                let mut s = format!(
                    "{role}'s window {id} ended at {}, but {} {} confirmed closing it",
                    when(ends),
                    nodes.join(", "),
                    if nodes.len() == 1 { "hasn't" } else { "haven't" }
                );
                if let Some(p) = problem {
                    s.push_str(&format!(" ({p})"));
                }
                s
            }
        });
    }
    out
}

/// Every window, newest first, for `role list` and `GET /v1/windows`.
pub async fn history(db: &Db, tenant: &str, limit: i64) -> anyhow::Result<Vec<serde_json::Value>> {
    let rows = sqlx::query(
        "SELECT id, role, requested_by, opened_at, ends_at, closed_at, closed_by, pending_nodes, problem FROM role_windows
         WHERE tenant = ? ORDER BY opened_at DESC LIMIT ?",
    )
    .bind(tenant)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            serde_json::json!({"id": r.get::<String, _>(0), "role": r.get::<String, _>(1), "requestedBy": r.get::<String, _>(2),
                "openedAt": r.get::<i64, _>(3), "endsAt": r.get::<i64, _>(4), "closedAt": r.get::<Option<i64>, _>(5),
                "closedBy": r.get::<Option<String>, _>(6),
                "unconfirmed": serde_json::from_str::<serde_json::Value>(&r.get::<String, _>(7)).unwrap_or_default(),
                "problem": r.get::<Option<String>, _>(8)})
        })
        .collect())
}

/// Open a window as the person (`fleetlayer-control role activate`): filed and approved at once.
pub async fn open_now(db: &Db, tenant: &str, p: &Compiled, role: &str, hold: &str, by: &str) -> anyhow::Result<i64> {
    let def = p
        .file
        .roles
        .get(role)
        .filter(|r| r.windowed())
        .ok_or_else(|| anyhow::anyhow!("there's no role {role} with `activation: window` in the access policy"))?;
    let hold_ms = crate::policy::parse_duration_ms(hold)?;
    anyhow::ensure!(
        hold_ms > 0 && hold_ms <= def.longest_ms(),
        "{role}'s window can be at most {} long (its `longest:`)",
        human(def.longest_ms())
    );
    if let Some((_, ends)) = active(db, tenant, role, def.longest_ms()).await? {
        anyhow::bail!("{role}'s window is open already, until {}", when(ends));
    }
    let id = format!("ap_{}", crate::db::new_id());
    let now = now_ms();
    sqlx::query(
        "INSERT INTO approvals (tenant, id, subject, verb, target, status, hold_ms, created_at, decided_at, expires_at)
         VALUES (?, ?, ?, 'activate', ?, 'approved', ?, ?, ?, ?)",
    )
    .bind(tenant)
    .bind(&id)
    .bind(by)
    .bind(target(role))
    .bind(hold_ms)
    .bind(now)
    .bind(now)
    .bind(now + hold_ms)
    .execute(db)
    .await?;
    Ok(now + hold_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: &str = "version: 2\ndefault: allow\nask: []\nroles:\n  admin: { who: \"*@admin\", activation: window, longest: 12h }\n";

    async fn app() -> Arc<App> {
        let db = crate::db::open_memory().await.unwrap();
        crate::authz::store(&db, crate::db::DEFAULT_TENANT, POLICY).await.unwrap();
        Arc::new(App {
            db,
            links: Default::default(),
            hook_limits: Default::default(),
            admin_token_hash: None,
            streams: Default::default(),
            authz: Default::default(),
            audit: crate::audit::Audit::new(None),
        })
    }

    async fn approve(app: &App, id: &str, hold: Option<i64>) {
        crate::approvals::decide(&app.db, crate::db::DEFAULT_TENANT, id, true, hold).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_window_opens_when_approved_is_capped_and_closes_early() {
        let app = app().await;
        let t = crate::db::DEFAULT_TENANT;
        let p = app.authz.policy(&app.db, t).await.unwrap().unwrap();
        assert_eq!(closed_roles(&app.db, t, &p, "ops@admin").await.unwrap(), ["admin"]);
        // Only the role's agents may ask, for no longer than its longest.
        assert!(request(&app, t, &p, "x@laptop", "admin", "1h", None).await.is_err());
        assert!(request(&app, t, &p, "ops@admin", "admin", "1d", None).await.unwrap_err().message.contains("12h"));
        assert!(request(&app, t, &p, "ops@admin", "nope", "1h", None).await.is_err());
        let r = request(&app, t, &p, "ops@admin", "admin", "1h", Some("rotating the database password")).await.unwrap();
        assert_eq!(r.state, "pending");
        let id = r.request.unwrap();
        assert_eq!(list_for(&app.db, t, &p, Some("ops@admin")).await.unwrap()[0].state, "pending");
        // The person grants a year: the window still ends within the role's longest.
        approve(&app, &id, Some(365 * 86_400_000)).await;
        let (open_id, ends) = active(&app.db, t, "admin", p.file.roles["admin"].longest_ms()).await.unwrap().unwrap();
        assert_eq!(open_id, id);
        assert!(ends <= now_ms() + 12 * 3_600_000);
        assert!(closed_roles(&app.db, t, &p, "ops@admin").await.unwrap().is_empty());
        tick(&app).await.unwrap();
        let again = request(&app, t, &p, "ops@admin", "admin", "1h", None).await.unwrap();
        assert_eq!(again.state, "open");
        // Closed early: off at once, recorded as closed, and the role's nodes owe a confirmation.
        sqlx::query("INSERT INTO nodes (tenant, name, token_hash, created_at) VALUES (?, 'admin', 'h-admin', 0)")
            .bind(t)
            .execute(&app.db)
            .await
            .unwrap();
        assert!(deactivate(&app, t, "admin", "person").await.unwrap());
        assert_eq!(closed_roles(&app.db, t, &p, "ops@admin").await.unwrap(), ["admin"]);
        let h = history(&app.db, t, 10).await.unwrap();
        assert!(h[0]["closedAt"].is_i64(), "{h:?}");
        assert_eq!(h[0]["closedBy"], "person");
        assert!(!deactivate(&app, t, "admin", "person").await.unwrap(), "nothing open now");
    }

    #[tokio::test]
    async fn an_unconfirmed_close_is_a_problem_until_the_node_confirms() {
        let app = app().await;
        let t = crate::db::DEFAULT_TENANT;
        let past = now_ms() - 10 * 60_000;
        sqlx::query(
            "INSERT INTO role_windows (tenant, id, role, requested_by, opened_at, ends_at, closed_at, pending_nodes)
             VALUES (?, 'ap_w', 'admin', 'ops@admin', ?, ?, ?, '[\"admin\"]')",
        )
        .bind(t)
        .bind(past - 3_600_000)
        .bind(past)
        .bind(past)
        .execute(&app.db)
        .await
        .unwrap();
        let p = problems(&app.db).await;
        assert_eq!(p.len(), 1);
        assert!(p[0].contains("admin hasn't confirmed"), "{p:?}");
        let bad = WindowClosed { id: "ap_w".into(), problem: Some("the end hook exited with 1".into()) };
        confirmed(&app, "admin", &bad).await.unwrap();
        assert!(problems(&app.db).await[0].contains("the end hook exited with 1"));
        confirmed(&app, "admin", &WindowClosed { id: "ap_w".into(), problem: None }).await.unwrap();
        assert!(problems(&app.db).await.is_empty());
        // A window past its end that nobody closed is a problem too.
        sqlx::query(
            "INSERT INTO role_windows (tenant, id, role, requested_by, opened_at, ends_at) VALUES (?, 'ap_x', 'admin', 'ops@admin', ?, ?)",
        )
        .bind(t)
        .bind(past - 3_600_000)
        .bind(past)
        .execute(&app.db)
        .await
        .unwrap();
        assert!(problems(&app.db).await[0].contains("hasn't been closed"));
        tick(&app).await.unwrap();
        assert!(problems(&app.db).await.is_empty(), "the loop closes it, and no node of the role is known");
    }
}
