//! Approving from a deck, with a passkey for role windows (`docs/access-control.md`, "Windows in a deck or an inbox").
//!
//! A deck holds an API token with the `approve` scope: it reads approvals and windows, approves and refuses requests and
//! closes windows, and nothing else. The token has an owner, a person of its tenant, recorded as the approver of what
//! it decides; the owner must be one of the tenant's admins when it's used.
//!
//! Opening a role window (approving a request with verb `activate` on `window:<role>`) with such a token also needs a
//! step-up: the owner confirmed that one approval with their passkey at the sign-in service within the last few minutes.
//! The sign-in service reports it over its own token (`POST /v1/signin/stepup`), the channel the control plane already
//! trusts to say who someone is; each step-up is for one approval, used once. The admin token needs no step-up: it's
//! the operator's.

use sqlx::Row;

use crate::db::{Db, now_ms};

/// How long a step-up holds.
pub const STEPUP_MS: i64 = 5 * 60_000;

/// One approval as the sign-in page shows it, or None when there's none by that ID in the tenant.
pub async fn view(db: &Db, tenant: &str, id: &str) -> anyhow::Result<Option<serde_json::Value>> {
    let row = sqlx::query(
        "SELECT id, subject, verb, target, status, hold_ms, created_at, detail FROM approvals WHERE tenant = ? AND id = ?",
    )
    .bind(tenant)
    .bind(id)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|r| {
        serde_json::json!({"id": r.get::<String, _>(0), "from": r.get::<String, _>(1), "verb": r.get::<String, _>(2),
            "to": r.get::<String, _>(3), "status": r.get::<String, _>(4), "holdMs": r.get::<i64, _>(5),
            "createdAt": r.get::<i64, _>(6), "detail": r.get::<Option<String>, _>(7)})
    }))
}

/// Whether an approval's request opens a role window, so approving it with an approve token needs a step-up.
pub fn needs_stepup(verb: &str, target: &str) -> bool {
    verb == "activate" && target.starts_with("window:")
}

/// Why a step-up for `id` by `person` (`person:<handle>`, of `role`) can't be recorded, or None when it can.
pub async fn refusal(db: &Db, tenant: &str, id: &str, role: &str) -> anyhow::Result<Option<String>> {
    if role != "admin" {
        return Ok(Some("only the tenant's admins approve role windows".into()));
    }
    let row = sqlx::query("SELECT status, verb, target FROM approvals WHERE tenant = ? AND id = ?")
        .bind(tenant)
        .bind(id)
        .fetch_optional(db)
        .await?;
    let Some(r) = row else { return Ok(Some(format!("there's no request {id}"))) };
    let (status, verb, target): (String, String, String) = (r.get(0), r.get(1), r.get(2));
    if !needs_stepup(&verb, &target) {
        return Ok(Some(format!("request {id} doesn't open a role window, so it needs no passkey")));
    }
    if status != "pending" {
        return Ok(Some(format!("request {id} isn't waiting any more (it's {status})")));
    }
    Ok(None)
}

/// Record that `person` just confirmed approval `id` with their passkey; returns when it stops holding.
pub async fn record(db: &Db, tenant: &str, id: &str, person: &str, signin: &str) -> anyhow::Result<i64> {
    let now = now_ms();
    sqlx::query(
        "INSERT INTO approval_stepups (tenant, approval_id, person, at, expires_at, signin) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(tenant)
    .bind(id)
    .bind(person)
    .bind(now)
    .bind(now + STEPUP_MS)
    .bind(signin)
    .execute(db)
    .await?;
    Ok(now + STEPUP_MS)
}

/// Use `person`'s step-up for approval `id`, if there's one that still holds. In one statement, so two approves can't
/// share one.
pub async fn consume(db: &Db, tenant: &str, id: &str, person: &str) -> anyhow::Result<bool> {
    let now = now_ms();
    let r = sqlx::query(
        "UPDATE approval_stepups SET used_at = ? WHERE rowid = (SELECT rowid FROM approval_stepups
         WHERE tenant = ? AND approval_id = ? AND person = ? AND used_at IS NULL AND expires_at > ? ORDER BY at DESC LIMIT 1)",
    )
    .bind(now)
    .bind(tenant)
    .bind(id)
    .bind(person)
    .bind(now)
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// An approve token's owner, checked: a person of the tenant who is one of its admins now. Err says why not.
pub fn owner_may_approve(fleet: &crate::directory::Fleet, tenant: &str, owner: Option<&str>) -> Result<String, String> {
    let owner = owner.ok_or("this approve token has no owner: mint it again with --owner")?;
    let handle = owner.strip_prefix("person:").unwrap_or(owner);
    let t = fleet.tenant(tenant).ok_or("no such tenant")?;
    match t.people.get(handle) {
        Some(p) if p.role == "admin" => Ok(format!("person:{handle}")),
        Some(_) => Err(format!("this approve token's owner, person:{handle}, isn't one of the tenant's admins")),
        None => Err(format!("this approve token's owner, person:{handle}, isn't a member of the tenant")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn db_with(status: &str, verb: &str, target: &str) -> Db {
        let db = crate::db::open_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO approvals (tenant, id, subject, verb, target, status, hold_ms, created_at)
             VALUES ('default', 'ap_1', 'ops@admin', ?, ?, ?, 3600000, 0)",
        )
        .bind(verb)
        .bind(target)
        .bind(status)
        .execute(&db)
        .await
        .unwrap();
        db
    }

    #[tokio::test]
    async fn a_stepup_is_for_one_pending_window_request_by_an_admin_and_is_used_once() {
        let db = db_with("pending", "activate", "window:admin").await;
        assert!(refusal(&db, "default", "ap_1", "member").await.unwrap().unwrap().contains("admins"));
        assert!(refusal(&db, "default", "ap_2", "admin").await.unwrap().unwrap().contains("no request"));
        assert!(refusal(&db, "default", "ap_1", "admin").await.unwrap().is_none());
        assert!(refusal(&db, "other", "ap_1", "admin").await.unwrap().is_some(), "another tenant's request");
        assert!(!consume(&db, "default", "ap_1", "person:sam").await.unwrap(), "none yet");
        record(&db, "default", "ap_1", "person:sam", "signin").await.unwrap();
        assert!(!consume(&db, "default", "ap_1", "person:kim").await.unwrap(), "someone else's");
        assert!(consume(&db, "default", "ap_1", "person:sam").await.unwrap());
        assert!(!consume(&db, "default", "ap_1", "person:sam").await.unwrap(), "used once");
        record(&db, "default", "ap_1", "person:sam", "signin").await.unwrap();
        sqlx::query("UPDATE approval_stepups SET expires_at = 1").execute(&db).await.unwrap();
        assert!(!consume(&db, "default", "ap_1", "person:sam").await.unwrap(), "expired");
        let other = db_with("pending", "use_tool", "tool:browser@laptop").await;
        assert!(refusal(&other, "default", "ap_1", "admin").await.unwrap().unwrap().contains("needs no passkey"));
        let done = db_with("approved", "activate", "window:admin").await;
        assert!(refusal(&done, "default", "ap_1", "admin").await.unwrap().unwrap().contains("isn't waiting"));
    }
}
