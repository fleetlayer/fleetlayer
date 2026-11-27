//! Approvals for `ask:` rules: a request that needs the person is refused with a request ID until they approve it
//! (`fleetlayer-control approvals approve <id>`, or `POST /v1/approvals/<id>`); then the same subject may do the same
//! thing to the same target until the approval expires. Each new request is published as an `access.ask` event on the
//! `access` channel, for whatever brings it to the person (an Inbox card, a phone push).

use sqlx::Row;

use crate::App;
use crate::db::{Db, new_id, now_ms};

pub enum Status {
    Approved,
    /// Waiting; the request's ID.
    Pending(String),
    Denied(String),
}

/// Whether `who` doing `verb` to `target` is approved now; files a request (and announces it) when there's none.
pub async fn check(
    app: &App,
    tenant: &str,
    who: &str,
    verb: &str,
    target: &str,
    hold_ms: i64,
) -> anyhow::Result<Status> {
    let db = &app.db;
    let now = now_ms();
    let rows = sqlx::query(
        "SELECT id, status, expires_at, hold_ms FROM approvals WHERE tenant = ? AND subject = ? AND verb = ? AND target = ?
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(tenant)
    .bind(who)
    .bind(verb)
    .bind(target)
    .fetch_optional(db)
    .await?;
    if let Some(r) = rows {
        let (id, status, expires, hold): (String, String, Option<i64>, i64) = (r.get(0), r.get(1), r.get(2), r.get(3));
        match status.as_str() {
            "approved" if expires.is_some_and(|e| e > now) => {
                if let Some(to) = renewal(verb, expires, hold, now) {
                    sqlx::query("UPDATE approvals SET expires_at = ? WHERE tenant = ? AND id = ?")
                        .bind(to)
                        .bind(tenant)
                        .bind(&id)
                        .execute(db)
                        .await?;
                }
                return Ok(Status::Approved);
            }
            "pending" => return Ok(Status::Pending(id)),
            // A refusal stands for an hour; then the agent may ask again.
            "denied" if expires.is_some_and(|e| e > now) => return Ok(Status::Denied(id)),
            _ => {}
        }
    }
    let id = format!("ap_{}", new_id());
    sqlx::query(
        "INSERT INTO approvals (tenant, id, subject, verb, target, status, hold_ms, created_at) VALUES (?, ?, ?, ?, ?, 'pending', ?, ?)",
    )
    .bind(tenant)
    .bind(&id)
    .bind(who)
    .bind(verb)
    .bind(target)
    .bind(hold_ms)
    .bind(now)
    .execute(db)
    .await?;
    let e = fleetlayer_protocol::ext::Event {
        channel: "access".into(),
        kind: "access.ask".into(),
        subject: Some(format!("{who} -> {target}")),
        at: jiff::Timestamp::now(),
        dedupe_key: Some(id.clone()),
        summary: format!("{who} asks to {verb} {target} (approval {id})"),
        body: serde_json::json!({"id": id, "from": who, "verb": verb, "to": target, "approve_with": format!("fleetlayer-control approvals approve {id}")}),
    };
    let _ = crate::route::publish(app, tenant, e, "fleetlayer access control").await;
    Ok(Status::Pending(id))
}

/// An approval's state, looked up without filing a request or renewing one (`fleetlayer me`).
pub enum Peek {
    /// Approved until then (milliseconds).
    Approved(i64),
    Pending(String),
    Denied(String),
    /// Never asked, or the last answer has lapsed: the next use asks.
    None,
}

pub async fn peek(db: &Db, tenant: &str, who: &str, verb: &str, target: &str) -> anyhow::Result<Peek> {
    let row = sqlx::query(
        "SELECT id, status, expires_at FROM approvals WHERE tenant = ? AND subject = ? AND verb = ? AND target = ?
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(tenant)
    .bind(who)
    .bind(verb)
    .bind(target)
    .fetch_optional(db)
    .await?;
    let Some(r) = row else { return Ok(Peek::None) };
    let (id, status, expires): (String, String, Option<i64>) = (r.get(0), r.get(1), r.get(2));
    let now = now_ms();
    Ok(match (status.as_str(), expires) {
        ("approved", Some(e)) if e > now => Peek::Approved(e),
        ("pending", _) => Peek::Pending(id),
        ("denied", Some(e)) if e > now => Peek::Denied(id),
        _ => Peek::None,
    })
}

/// Approvals for work that runs unattended and repeats (a sync, a tool an agent uses) renew while they're used: each
/// use pushes the expiry to a full hold from now, so one lapses only after a hold's length unused, rather than stopping
/// a sync silently mid-use. The new expiry, when it moves by more than an hour (no write on every use).
fn renewal(verb: &str, expires: Option<i64>, hold: i64, now: i64) -> Option<i64> {
    const RENEWS: &[&str] = &["sync", "use_tool"];
    let to = now + hold;
    (RENEWS.contains(&verb) && expires.is_some_and(|e| to > e + 3_600_000)).then_some(to)
}

/// A decided request.
pub struct Decided {
    pub subject: String,
    pub verb: String,
    pub target: String,
}

/// Approve (for `hold_ms`, or the request's own) or deny a request. None when there's no pending request by that ID.
/// The hold granted is kept, so a renewing approval renews by it.
pub async fn decide(
    db: &Db,
    tenant: &str,
    id: &str,
    approve: bool,
    hold_ms: Option<i64>,
) -> anyhow::Result<Option<Decided>> {
    let now = now_ms();
    let r = sqlx::query(
        "SELECT hold_ms, subject, verb, target FROM approvals WHERE tenant = ? AND id = ? AND status = 'pending'",
    )
    .bind(tenant)
    .bind(id)
    .fetch_optional(db)
    .await?;
    let Some(r) = r else { return Ok(None) };
    let hold = hold_ms.unwrap_or(r.get(0));
    let decided = Decided { subject: r.get(1), verb: r.get(2), target: r.get(3) };
    // A denial is remembered for an hour (expires_at), so the agent can't ask again at once.
    let expires = if approve { now + hold } else { now + 3_600_000 };
    sqlx::query(
        "UPDATE approvals SET status = ?, decided_at = ?, expires_at = ?, hold_ms = ? WHERE tenant = ? AND id = ?",
    )
    .bind(if approve { "approved" } else { "denied" })
    .bind(now)
    .bind(expires)
    .bind(hold)
    .bind(tenant)
    .bind(id)
    .execute(db)
    .await?;
    Ok(Some(decided))
}

pub async fn list(db: &Db, tenant: &str, all: bool) -> anyhow::Result<Vec<serde_json::Value>> {
    let rows = sqlx::query(
        "SELECT id, subject, verb, target, status, created_at, expires_at FROM approvals WHERE tenant = ?
         AND (? = 1 OR status = 'pending') ORDER BY created_at DESC LIMIT 200",
    )
    .bind(tenant)
    .bind(all as i64)
    .fetch_all(db)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            serde_json::json!({"id": r.get::<String, _>(0), "from": r.get::<String, _>(1), "verb": r.get::<String, _>(2),
                "to": r.get::<String, _>(3), "status": r.get::<String, _>(4), "createdAt": r.get::<i64, _>(5),
                "expiresAt": r.get::<Option<i64>, _>(6)})
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400_000;

    #[test]
    fn sync_and_tool_approvals_renew_while_used_and_others_dont() {
        let now = 100 * DAY;
        // Approved 29 days ago for 30: a use pushes it to 30 days from now.
        assert_eq!(renewal("sync", Some(now + DAY), 30 * DAY, now), Some(now + 30 * DAY));
        assert_eq!(renewal("use_tool", Some(now + DAY), 30 * DAY, now), Some(now + 30 * DAY));
        // Not on every use: only when it moves by more than an hour.
        assert_eq!(renewal("sync", Some(now + 30 * DAY - 60_000), 30 * DAY, now), None);
        // Never shortened (a longer hold set by hand stays).
        assert_eq!(renewal("sync", Some(now + 300 * DAY), 30 * DAY, now), None);
        // A one-off approval (start, write) holds for its time and no longer.
        assert_eq!(renewal("start", Some(now + 60_000), 600_000, now), None);
        assert_eq!(renewal("write", Some(now + DAY), 30 * DAY, now), None);
    }

    #[tokio::test]
    async fn a_decision_keeps_the_hold_granted_and_says_what_it_was_for() {
        let db = crate::db::open_memory().await.unwrap();
        let t = crate::db::DEFAULT_TENANT;
        sqlx::query(
            "INSERT INTO approvals (tenant, id, subject, verb, target, status, hold_ms, created_at)
             VALUES (?, 'ap_1', '-@laptop', 'sync', 'sync:files/skills@server', 'pending', ?, 0)",
        )
        .bind(t)
        .bind(30 * DAY)
        .execute(&db)
        .await
        .unwrap();
        let d = decide(&db, t, "ap_1", true, Some(365 * DAY)).await.unwrap().unwrap();
        assert_eq!(
            (d.subject.as_str(), d.verb.as_str(), d.target.as_str()),
            ("-@laptop", "sync", "sync:files/skills@server")
        );
        let hold: i64 =
            sqlx::query_scalar("SELECT hold_ms FROM approvals WHERE id = 'ap_1'").fetch_one(&db).await.unwrap();
        assert_eq!(hold, 365 * DAY, "it renews by what the person granted");
        assert!(decide(&db, t, "ap_1", true, None).await.unwrap().is_none(), "already decided");
    }
}
