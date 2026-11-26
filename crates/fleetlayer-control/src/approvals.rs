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
        "SELECT id, status, expires_at FROM approvals WHERE tenant = ? AND subject = ? AND verb = ? AND target = ?
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(tenant)
    .bind(who)
    .bind(verb)
    .bind(target)
    .fetch_optional(db)
    .await?;
    if let Some(r) = rows {
        let (id, status, expires): (String, String, Option<i64>) = (r.get(0), r.get(1), r.get(2));
        match status.as_str() {
            "approved" if expires.is_some_and(|e| e > now) => return Ok(Status::Approved),
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

/// Approve (for `hold_ms`, or the request's own) or deny a request. False when there's no pending request by that ID.
pub async fn decide(db: &Db, tenant: &str, id: &str, approve: bool, hold_ms: Option<i64>) -> anyhow::Result<bool> {
    let now = now_ms();
    let r = sqlx::query("SELECT hold_ms FROM approvals WHERE tenant = ? AND id = ? AND status = 'pending'")
        .bind(tenant)
        .bind(id)
        .fetch_optional(db)
        .await?;
    let Some(r) = r else { return Ok(false) };
    let hold = hold_ms.unwrap_or(r.get(0));
    // A denial is remembered for an hour (expires_at), so the agent can't ask again at once.
    let expires = if approve { now + hold } else { now + 3_600_000 };
    sqlx::query("UPDATE approvals SET status = ?, decided_at = ?, expires_at = ? WHERE tenant = ? AND id = ?")
        .bind(if approve { "approved" } else { "denied" })
        .bind(now)
        .bind(expires)
        .bind(tenant)
        .bind(id)
        .execute(db)
        .await?;
    Ok(true)
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
