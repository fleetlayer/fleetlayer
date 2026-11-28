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
    let by_itself =
        caller == from || from_session_id.as_deref().is_some_and(|id| caller == format!("{id}@{from_node}"));
    if !by_itself {
        crate::authz::check_why(app, tenant, &caller, "read", &from, h.start.justification.as_deref()).await?;
    }
    let to = format!("{to_tab}@{}", h.start.node);
    let inserted = sqlx::query(
        "INSERT INTO handoffs (tenant, id, from_address, from_session_id, to_address, note, by_address, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(tenant)
    .bind(&h.id)
    .bind(&from)
    .bind(&from_session_id)
    .bind(&to)
    .bind(&h.note)
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
    let r = sqlx::query("SELECT * FROM handoffs WHERE tenant = ? AND id = ?")
        .bind(tenant)
        .bind(id)
        .fetch_optional(db)
        .await?;
    Ok(r.map(row))
}

/// The newest handoffs first, without their notes (they can be long; `by_id` has them).
pub async fn recent(db: &Db, tenant: &str, limit: i64) -> anyhow::Result<Vec<l::Handoff>> {
    let rows = sqlx::query("SELECT * FROM handoffs WHERE tenant = ? ORDER BY created_at DESC, id DESC LIMIT ?")
        .bind(tenant)
        .bind(limit)
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().map(row).map(|h| l::Handoff { note: None, ..h }).collect())
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
