//! Handoffs: one session's work moved to a new session, usually on another node. The node that asks writes the new
//! session's first prompt (where its note and the old transcript are); the control plane checks the policy (`start` on
//! the new session, `read` on the old one when someone else hands it off), keeps the note, starts the session through
//! the ordinary start path and links old and new in the fleet (`/v1/fleet`: `handoffFrom`, `handoffTo`).

use fleetlayer_protocol::link as l;
use fleetlayer_protocol::rpc::RpcError;
use serde_json::{Value, json};
use sqlx::Row;

use crate::App;
use crate::db::{self, Db};

/// The longest note kept.
pub const NOTE_MAX: usize = 64 * 1024;

/// How long a handoff (and its note) is kept, in days, unless `control.yaml` says otherwise (`handoff_days`).
pub const DEFAULT_DAYS: u32 = 30;
static DAYS: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

/// Set how long handoffs are kept (from `control.yaml`), once at start.
pub fn keep_for(days: u32) {
    let _ = DAYS.set(days.max(1));
}

/// Forget the handoffs older than the retention.
pub async fn prune(db: &Db) -> anyhow::Result<()> {
    let days = i64::from(*DAYS.get().unwrap_or(&DEFAULT_DAYS));
    sqlx::query("DELETE FROM handoffs WHERE created_at < ?")
        .bind(db::now_ms() - days * 24 * 3600 * 1000)
        .execute(db)
        .await?;
    Ok(())
}

/// Well-known secret formats, redacted from a note before it's kept: the node has already redacted the secrets
/// delivered to it, and these catch the ones it never knew (pasted from a vendor's page, an environment variable).
fn known_secrets() -> &'static [(regex::Regex, &'static str)] {
    static R: std::sync::OnceLock<Vec<(regex::Regex, &'static str)>> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        [
            (
                r"-----BEGIN [A-Z ]*PRIVATE KEY( BLOCK)?-----[\s\S]*?(-----END [A-Z ]*PRIVATE KEY( BLOCK)?-----|$)",
                "a private key",
            ),
            (r"\b(ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,}\b", "a GitHub token"),
            (r"\bgithub_pat_[A-Za-z0-9_]{60,}\b", "a GitHub token"),
            (r"\b(AKIA|ASIA)[A-Z0-9]{16}\b", "an AWS access key"),
            (r"\bxox[abposr]-[A-Za-z0-9-]{10,}\b", "a Slack token"),
            (r"\b(sk|rk)_(live|test)_[A-Za-z0-9]{20,}\b", "a Stripe key"),
            (r"\bsk-ant-[A-Za-z0-9_-]{20,}", "an Anthropic key"),
            (r"\bsk-(proj-)?[A-Za-z0-9_-]{32,}", "an OpenAI key"),
            (r"\bAIza[0-9A-Za-z_-]{35}\b", "a Google API key"),
            (r"\bglpat-[A-Za-z0-9_-]{20,}\b", "a GitLab token"),
            (r"\bnpm_[A-Za-z0-9]{36}\b", "an npm token"),
            (r"\bops_[A-Za-z0-9_-]{40,}", "a 1Password service account token"),
            (r"\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}", "a JSON web token"),
        ]
        .into_iter()
        .map(|(p, what)| (regex::Regex::new(p).expect("a secret pattern compiles"), what))
        .collect()
    })
}

/// `note` with every well-known secret format redacted, and how many were.
pub fn redact_note(note: &str) -> (String, usize) {
    let mut out = note.to_string();
    let mut n = 0;
    for (re, what) in known_secrets() {
        let found = re.find_iter(&out).count();
        if found > 0 {
            n += found;
            out = re.replace_all(&out, format!("[REDACTED {what}]").as_str()).into_owned();
        }
    }
    (out, n)
}

/// Start `h`'s new session for a caller on `from_node`, keeping the handoff.
pub async fn start(
    app: &App,
    tenant: &str,
    from_node: &str,
    h: l::SessionHandoff,
) -> Result<l::HandoffStarted, RpcError> {
    let caller = format!("{}@{from_node}", h.start.caller);
    let valid_id = h.id.len() == 17 && h.id.starts_with('h') && h.id[1..].bytes().all(|b| b.is_ascii_hexdigit());
    if !valid_id {
        return Err(RpcError::invalid_params("a handoff ID is h and 16 hex digits"));
    }
    let Some((from_name, from_node_name)) = h.from.rsplit_once('@') else {
        return Err(RpcError::invalid_params("say which session hands off, as <tab>@<node>"));
    };
    if h.note.as_ref().is_some_and(|n| n.len() > NOTE_MAX) {
        return Err(RpcError::invalid_params(format!("a handoff note is at most {} KB", NOTE_MAX / 1024)));
    }
    let to_tab = h.start.spec.tab.clone().ok_or_else(|| RpcError::invalid_params("name the new session's tab"))?;
    // The old session's own work is its to hand off; anyone else needs to be able to read it.
    let from_session_id = session_id(&app.db, tenant, from_node_name, from_name).await.map_err(RpcError::internal)?;
    let from_tab = tab_of(&app.db, tenant, from_node_name, from_name).await.unwrap_or_else(|| from_name.to_string());
    let from = format!("{from_tab}@{from_node_name}");
    // Only a session on the old session's own node can be it: a tab elsewhere named like its ID isn't.
    let by_itself = from_node == from_node_name
        && (caller == from || from_session_id.as_deref().is_some_and(|id| caller == format!("{id}@{from_node}")));
    if !by_itself {
        crate::authz::check_why(app, tenant, &caller, "read", &from, h.start.justification.as_deref()).await?;
    }
    let to = format!("{to_tab}@{}", h.start.node);
    prune(&app.db).await.map_err(RpcError::internal)?;
    let note = h.note.as_deref().map(|n| {
        let (clean, found) = redact_note(n);
        if found > 0 {
            tracing::warn!(handoff = %h.id, found, "redacted secrets from a handoff note before keeping it");
        }
        clean
    });
    let inserted = sqlx::query(
        "INSERT INTO handoffs (tenant, id, from_address, from_session_id, to_address, note, by_address, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(tenant)
    .bind(&h.id)
    .bind(&from)
    .bind(&from_session_id)
    .bind(&to)
    .bind(&note)
    .bind(&caller)
    .bind(db::now_ms())
    .execute(&app.db)
    .await;
    if let Err(e) = inserted {
        return Err(if e.to_string().contains("UNIQUE") {
            RpcError::refused(format!("there's already a handoff {}: try again", h.id))
        } else {
            RpcError::internal(e)
        });
    }
    match crate::route::start_session(app, tenant, from_node, h.start).await {
        Ok(started) => {
            let to = format!("{}@{}", started.tab, started.node);
            sqlx::query("UPDATE handoffs SET to_address = ?, to_session_id = ? WHERE tenant = ? AND id = ?")
                .bind(&to)
                .bind(&started.session_id)
                .bind(tenant)
                .bind(&h.id)
                .execute(&app.db)
                .await
                .map_err(RpcError::internal)?;
            Ok(l::HandoffStarted { handoff: h.id, started })
        }
        Err(e) => {
            let _ = sqlx::query("DELETE FROM handoffs WHERE tenant = ? AND id = ?")
                .bind(tenant)
                .bind(&h.id)
                .execute(&app.db)
                .await;
            Err(e)
        }
    }
}

/// One handoff, for the session it went to or a caller who may read the session it came from.
pub async fn get(app: &App, tenant: &str, from_node: &str, g: l::HandoffGet) -> Result<l::Handoff, RpcError> {
    let caller = format!("{}@{from_node}", g.caller);
    let h = by_id(&app.db, tenant, &g.id)
        .await
        .map_err(RpcError::internal)?
        .ok_or_else(|| RpcError::refused(format!("there's no handoff {}", g.id)))?;
    let to_node = h.to.rsplit_once('@').map(|(_, n)| n).unwrap_or_default();
    let is_new_session =
        caller == h.to || h.to_session_id.as_deref().is_some_and(|id| caller == format!("{id}@{to_node}"));
    if !is_new_session {
        crate::authz::check_why(app, tenant, &caller, "read", &h.from, g.justification.as_deref()).await?;
    }
    Ok(h)
}

fn row(r: sqlx::sqlite::SqliteRow) -> l::Handoff {
    let ms: i64 = r.get("created_at");
    l::Handoff {
        id: r.get("id"),
        from: r.get("from_address"),
        from_session_id: r.get("from_session_id"),
        to: r.get("to_address"),
        to_session_id: r.get("to_session_id"),
        note: r.get("note"),
        by: r.get("by_address"),
        created_at: jiff::Timestamp::from_millisecond(ms).map(|t| t.to_string()).unwrap_or_default(),
    }
}

pub async fn by_id(db: &Db, tenant: &str, id: &str) -> anyhow::Result<Option<l::Handoff>> {
    prune(db).await?;
    let r = sqlx::query("SELECT * FROM handoffs WHERE tenant = ? AND id = ?")
        .bind(tenant)
        .bind(id)
        .fetch_optional(db)
        .await?;
    Ok(r.map(row))
}

/// The newest handoffs first, without their notes (they can be long; `by_id` has them).
pub async fn recent(db: &Db, tenant: &str, limit: i64) -> anyhow::Result<Vec<l::Handoff>> {
    prune(db).await?;
    let rows = sqlx::query("SELECT * FROM handoffs WHERE tenant = ? ORDER BY created_at DESC, id DESC LIMIT ?")
        .bind(tenant)
        .bind(limit)
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().map(row).map(|h| l::Handoff { note: None, ..h }).collect())
}

/// The handoffs written after `cursor` (`<created ms>:<id>`, from an earlier answer's `nextCursor`; empty for the
/// first), oldest first, without their notes, with the cursor to ask from next.
pub async fn feed(db: &Db, tenant: &str, cursor: &str, limit: i64) -> anyhow::Result<(Vec<l::Handoff>, String)> {
    prune(db).await?;
    let (ms, id) = match cursor {
        "" => (i64::MIN, String::new()),
        c => {
            let (ms, id) = c.split_once(':').ok_or_else(|| anyhow::anyhow!("a cursor is <ms>:<id>"))?;
            (ms.parse()?, id.to_string())
        }
    };
    let rows = sqlx::query(
        "SELECT * FROM handoffs WHERE tenant = ? AND (created_at > ? OR (created_at = ? AND id > ?))
         ORDER BY created_at, id LIMIT ?",
    )
    .bind(tenant)
    .bind(ms)
    .bind(ms)
    .bind(&id)
    .bind(limit)
    .fetch_all(db)
    .await?;
    let next = rows
        .last()
        .map(|r| format!("{}:{}", r.get::<i64, _>("created_at"), r.get::<String, _>("id")))
        .unwrap_or_else(|| cursor.to_string());
    Ok((rows.into_iter().map(row).map(|h| l::Handoff { note: None, ..h }).collect(), next))
}

/// The session ID of `name` (a tab or a session ID) on `node`, as the node last pushed its sessions.
async fn session_id(db: &Db, tenant: &str, node: &str, name: &str) -> anyhow::Result<Option<String>> {
    Ok(db::sessions(db, tenant).await?.into_iter().find_map(|(n, s)| {
        let id = s["sessionId"].as_str()?;
        (n == node && (id == name || s["meta"]["tab"].as_str() == Some(name))).then(|| id.to_string())
    }))
}

/// The tab of `name` (a tab or a session ID) on `node`.
async fn tab_of(db: &Db, tenant: &str, node: &str, name: &str) -> Option<String> {
    db::sessions(db, tenant).await.ok()?.into_iter().find_map(|(n, s)| {
        let tab = s["meta"]["tab"].as_str()?;
        (n == node && (s["sessionId"].as_str() == Some(name) || tab == name)).then(|| tab.to_string())
    })
}

/// Add `handoffFrom` (the handoff that started it) and `handoffTo` (the newest that took its work) to each session
/// of the fleet, `{id, from|to, at}`.
pub fn link_sessions(sessions: &mut [Value], handoffs: &[l::Handoff]) {
    // By session ID when it's known: a tab's name outlives its session.
    let matches = |s: &Value, address: &str, id: Option<&str>| match id {
        Some(id) => s["sessionId"].as_str() == Some(id),
        None => s["meta"]["tab"].as_str().zip(s["node"].as_str()).is_some_and(|(t, n)| format!("{t}@{n}") == address),
    };
    for s in sessions.iter_mut() {
        // Newest first, so the first match is the newest.
        if let Some(h) = handoffs.iter().find(|h| matches(s, &h.to, h.to_session_id.as_deref())) {
            s["handoffFrom"] =
                json!({"id": h.id, "from": h.from, "fromSessionId": h.from_session_id, "at": h.created_at});
        }
        if let Some(h) = handoffs.iter().find(|h| matches(s, &h.from, h.from_session_id.as_deref())) {
            s["handoffTo"] = json!({"id": h.id, "to": h.to, "toSessionId": h.to_session_id, "at": h.created_at});
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Secrets the node never knew (pasted from a vendor's page) are redacted from a note before it's kept.
    #[test]
    fn well_known_secret_formats_are_redacted_from_a_note() {
        let note = "Deploy with ghp_0123456789abcdefghijABCDEFGHIJ012345 and AKIAABCDEFGHIJKLMNOP;\n\
                    -----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk\n-----END OPENSSH PRIVATE KEY-----\nThe PR is #42.";
        let (clean, n) = redact_note(note);
        assert_eq!(n, 3, "{clean}");
        assert!(!clean.contains("ghp_0123") && !clean.contains("AKIAABCD") && !clean.contains("b3BlbnNz"), "{clean}");
        assert!(clean.contains("[REDACTED a GitHub token]") && clean.contains("[REDACTED a private key]"), "{clean}");
        assert!(clean.ends_with("The PR is #42."), "{clean}");
        assert_eq!(redact_note("Tests pass; deploy next.").1, 0);
    }

    /// A handoff older than the retention is forgotten, note and all.
    #[tokio::test]
    async fn handoffs_older_than_the_retention_are_forgotten() {
        let db = crate::db::open_memory().await.unwrap();
        let old = db::now_ms() - (i64::from(DEFAULT_DAYS) + 1) * 24 * 3600 * 1000;
        for (id, at) in [("h0000000000000001", old), ("h0000000000000002", db::now_ms())] {
            sqlx::query(
                "INSERT INTO handoffs (tenant, id, from_address, to_address, note, by_address, created_at)
                 VALUES ('default', ?, 'a@x', 'a@y', 'n', 'a@x', ?)",
            )
            .bind(id)
            .bind(at)
            .execute(&db)
            .await
            .unwrap();
        }
        assert!(by_id(&db, "default", "h0000000000000001").await.unwrap().is_none());
        assert!(by_id(&db, "default", "h0000000000000002").await.unwrap().is_some());
        assert_eq!(recent(&db, "default", 10).await.unwrap().len(), 1);
    }
}
