//! Watches: "tell me when this happens", once. A watch matches events like a subscription (a channel, and maybe one
//! subject on it), optionally only some kinds of event (`session.idle`, `session.blocked`). The first event that
//! matches is delivered to its target like a subscription's, saying it was the watch, and the watch is removed. A watch
//! that sees nothing by its deadline ends with a message saying so, so "tell me when CI finishes, or in two hours if
//! it hasn't" is one watch.
//!
//! A watch on `session.status` for one session whose status already matches fires at once, from the session as its
//! node last pushed it, instead of waiting for a change that may never come.

use std::sync::Arc;
use std::time::Duration;

use fleetlayer_protocol::ext::{DeliveryKind, Event, From};
use fleetlayer_protocol::link::{DeliverRequest, WatchRequest};
use fleetlayer_protocol::rpc::RpcError;
use serde_json::{Value, json};
use sqlx::Row;

use crate::App;
use crate::db::{self, Db};

/// How long a watch lasts when it doesn't say.
pub const DEFAULT_FOR_SECS: u64 = 24 * 3600;
/// The longest a watch may last.
pub const MAX_FOR_SECS: u64 = 30 * 24 * 3600;
/// The most watches one target may have at once.
pub const MAX_PER_TARGET: i64 = 100;
/// The most watches one node's sessions may have at once (a node can name any target of its own).
pub const MAX_PER_NODE: i64 = 1000;

/// A watch that matched an event: removed already.
#[derive(Debug, Clone, PartialEq)]
pub struct Fired {
    pub id: String,
    pub target: String,
    pub note: Option<String>,
}

fn check(w: &WatchRequest) -> Result<u64, RpcError> {
    let name_ok =
        |s: &str| !s.is_empty() && s.len() <= 100 && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c));
    if !name_ok(&w.channel) {
        return Err(RpcError::invalid_params("channel: 1 to 100 letters, digits and - _ . :"));
    }
    if w.kinds.len() > 20 || w.kinds.iter().any(|k| !name_ok(k)) {
        return Err(RpcError::invalid_params("kinds: at most 20, each 1 to 100 letters, digits and - _ . :"));
    }
    if w.subject.as_deref().is_some_and(|s| s.is_empty() || s.len() > 500) {
        return Err(RpcError::invalid_params("subject: 1 to 500 bytes"));
    }
    if w.note.as_deref().is_some_and(|n| n.len() > 500) {
        return Err(RpcError::invalid_params("note: at most 500 bytes"));
    }
    let secs = w.for_secs.unwrap_or(DEFAULT_FOR_SECS);
    if !(60..=MAX_FOR_SECS).contains(&secs) {
        return Err(RpcError::invalid_params("a watch lasts from a minute to 30 days"));
    }
    Ok(secs)
}

/// Add a watch for `w.target` (already allowed to subscribe to the channel). Answers `{id, expiresAt}`, or
/// `{fired: true, event}` when a session's status already matches.
pub async fn add(app: &App, tenant: &str, w: &WatchRequest) -> Result<Value, RpcError> {
    let secs = check(w)?;
    // A watch on a session's status needs `read` on that session, under every name it goes by; checked again when it
    // fires.
    if w.channel == crate::status::CHANNEL
        && let Some(subject) = w.subject.as_deref()
    {
        let (id, node) = match crate::route::split_target(subject) {
            Some((name, node)) => {
                let sessions = db::node_sessions(&app.db, tenant, node).await.unwrap_or_default();
                let id = sessions
                    .iter()
                    .find(|s| s["meta"]["tab"].as_str() == Some(name) || s["sessionId"].as_str() == Some(name))
                    .and_then(|s| s["sessionId"].as_str().map(str::to_string));
                (id, node.to_string())
            }
            None => (None, String::new()),
        };
        if !crate::route::may_read_session(app, tenant, &w.target, subject, id.as_deref(), &node).await {
            return Err(RpcError::refused(format!("{} may not read {subject}, so it can't watch it", w.target)));
        }
    }
    if let Some(now) = already(app, tenant, w).await {
        return Ok(json!({"fired": true, "summary": now.summary, "event": now}));
    }
    let id = format!("w_{}", db::new_id());
    let expires = db::now_ms() + (secs as i64) * 1000;
    let node = crate::route::split_target(&w.target).map(|(_, n)| format!("@{n}")).unwrap_or_default();
    // Counted and added in one statement, so two at once can't both pass the limits.
    let added = sqlx::query(
        "INSERT INTO watches (tenant, id, channel, subject, kinds, target, note, expires_at, created_at)
         SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?
         WHERE (SELECT COUNT(*) FROM watches WHERE tenant = ? AND target = ?) < ?
           AND (SELECT COUNT(*) FROM watches WHERE tenant = ? AND substr(target, -length(?)) = ?) < ?",
    )
    .bind(tenant)
    .bind(&id)
    .bind(&w.channel)
    .bind(&w.subject)
    .bind(serde_json::to_string(&w.kinds).unwrap_or_else(|_| "[]".into()))
    .bind(&w.target)
    .bind(&w.note)
    .bind(expires)
    .bind(db::now_ms())
    .bind(tenant)
    .bind(&w.target)
    .bind(MAX_PER_TARGET)
    .bind(tenant)
    .bind(&node)
    .bind(&node)
    .bind(MAX_PER_NODE)
    .execute(&app.db)
    .await
    .map_err(|e| RpcError::internal(e.to_string()))?;
    if added.rows_affected() == 0 {
        return Err(RpcError::refused(format!(
            "{} has {MAX_PER_TARGET} watches already, or its node {MAX_PER_NODE}: remove some (fleetlayer watches, \
             fleetlayer unwatch <id>)",
            w.target
        )));
    }
    Ok(json!({"id": id, "expiresAt": jiff::Timestamp::from_millisecond(expires).ok()}))
}

/// The status event a `session.status` watch on one session would fire on now, if its status already matches.
async fn already(app: &App, tenant: &str, w: &WatchRequest) -> Option<Event> {
    if w.channel != crate::status::CHANNEL {
        return None;
    }
    let subject = w.subject.as_deref()?;
    let (name, node) = crate::route::split_target(subject)?;
    let sessions = db::node_sessions(&app.db, tenant, node).await.ok()?;
    let s =
        sessions.iter().find(|s| s["meta"]["tab"].as_str() == Some(name) || s["sessionId"].as_str() == Some(name))?;
    let status = crate::status::status_of(s);
    let kind = format!("session.{status}");
    if !w.kinds.is_empty() && !w.kinds.contains(&kind) {
        return None;
    }
    // The watcher must be allowed to read the session, as for its events.
    if !crate::route::may_read_session(app, tenant, &w.target, subject, s["sessionId"].as_str(), node).await {
        return None;
    }
    Some(Event {
        channel: w.channel.clone(),
        kind,
        subject: Some(subject.to_string()),
        at: jiff::Timestamp::now(),
        dedupe_key: None,
        summary: format!("{subject} is {status} already"),
        body: json!({"node": node, "sessionId": s["sessionId"], "tab": s["meta"]["tab"], "status": status,
            "statusChangedAt": s["meta"]["statusChangedAt"], "prompt": s["meta"]["prompt"],
            "outside_data": ["prompt", "tab"], "written_by": subject}),
    })
}

/// Remove watch `id`, if it's one of `node`'s targets'. True when it was there.
pub async fn remove(db: &Db, tenant: &str, id: &str, node: &str) -> anyhow::Result<bool> {
    let r = sqlx::query("DELETE FROM watches WHERE tenant = ? AND id = ? AND substr(target, -length(?)) = ?")
        .bind(tenant)
        .bind(id)
        .bind(format!("@{node}"))
        .bind(format!("@{node}"))
        .execute(db)
        .await?;
    Ok(r.rows_affected() > 0)
}

/// The watches whose targets are on `node` (every watch with None), soonest deadline first.
pub async fn list(db: &Db, tenant: &str, node: Option<&str>) -> anyhow::Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT id, channel, subject, kinds, target, note, expires_at, created_at FROM watches
         WHERE tenant = ? AND (? IS NULL OR substr(target, -length(?)) = ?) ORDER BY expires_at",
    )
    .bind(tenant)
    .bind(node)
    .bind(node.map(|n| format!("@{n}")))
    .bind(node.map(|n| format!("@{n}")))
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let kinds: String = r.get(3);
            json!({
                "id": r.get::<String, _>(0), "channel": r.get::<String, _>(1),
                "subject": r.get::<Option<String>, _>(2),
                "kinds": serde_json::from_str::<Value>(&kinds).unwrap_or(json!([])),
                "target": r.get::<String, _>(4), "note": r.get::<Option<String>, _>(5),
                "expiresAt": jiff::Timestamp::from_millisecond(r.get(6)).ok(),
                "createdAt": jiff::Timestamp::from_millisecond(r.get(7)).ok(),
            })
        })
        .collect())
}

/// The watches event `e` matches, not yet taken.
pub async fn matching(db: &Db, tenant: &str, e: &Event) -> anyhow::Result<Vec<Fired>> {
    let rows = sqlx::query(
        "SELECT id, target, note FROM watches WHERE tenant = ? AND channel = ? AND (subject IS NULL OR subject = ?)
           AND (json_array_length(kinds) = 0 OR ? IN (SELECT value FROM json_each(kinds)))
           AND expires_at > ?",
    )
    .bind(tenant)
    .bind(&e.channel)
    .bind(&e.subject)
    .bind(&e.kind)
    .bind(db::now_ms())
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|r| Fired { id: r.get(0), target: r.get(1), note: r.get(2) }).collect())
}

/// Take (remove) the watches `ids`: only those still there are answered, so each fires once even when two events race.
pub async fn take(db: &Db, tenant: &str, ids: &[String]) -> anyhow::Result<Vec<Fired>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let rows = sqlx::query(
        "DELETE FROM watches WHERE tenant = ? AND id IN (SELECT value FROM json_each(?)) RETURNING id, target, note",
    )
    .bind(tenant)
    .bind(serde_json::to_string(ids)?)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|r| Fired { id: r.get(0), target: r.get(1), note: r.get(2) }).collect())
}

/// The line added to a fired watch's delivery.
pub fn fired_line(f: &Fired) -> String {
    match &f.note {
        Some(n) => format!("This was your watch {} (\"{n}\"): it has fired and ended.", f.id),
        None => format!("This was your watch {}: it has fired and ended.", f.id),
    }
}

/// Every 30 seconds, end the watches past their deadline, telling each session target so.
pub async fn run(app: Arc<App>) {
    loop {
        if let Err(e) = expire(&app).await {
            tracing::warn!("ending watches past their deadline: {e}");
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

/// End the watches past their deadline now. Answers how many ended.
pub async fn expire(app: &App) -> anyhow::Result<usize> {
    let rows = sqlx::query(
        "DELETE FROM watches WHERE expires_at <= ? RETURNING tenant, id, channel, subject, kinds, target, note",
    )
    .bind(db::now_ms())
    .fetch_all(&app.db)
    .await?;
    for r in &rows {
        let (tenant, id, channel, target): (String, String, String, String) = (r.get(0), r.get(1), r.get(2), r.get(5));
        let subject: Option<String> = r.get(3);
        let kinds: String = r.get(4);
        let note: Option<String> = r.get(6);
        // A plugin's watch just ends: it has no text to read.
        if target.starts_with("plugin:") {
            continue;
        }
        let what = match (&subject, kinds.as_str()) {
            (Some(s), "[]") => format!("{channel} about {s}"),
            (Some(s), k) => format!("{channel} about {s} ({k})"),
            (None, "[]") => channel.clone(),
            (None, k) => format!("{channel} ({k})"),
        };
        let note = note.map(|n| format!(" (\"{n}\")")).unwrap_or_default();
        let d = DeliverRequest {
            name: String::new(),
            delivery_id: format!("watch-end-{id}"),
            kind: DeliveryKind::Message,
            subkind: Some("watch.expired".into()),
            untrusted: false,
            from: From { label: "Fleetlayer watches".into(), ..Default::default() },
            text: format!("Your watch {id}{note} on {what} reached its deadline with nothing seen, and has ended."),
        };
        if let Err(e) = crate::route::deliver(app, &tenant, &target, d).await {
            tracing::info!(%target, watch = %id, "a watch's end couldn't be told: {}", e.message);
        }
    }
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn app() -> Arc<App> {
        let app = Arc::new(App {
            db: db::open_memory().await.unwrap(),
            links: Default::default(),
            hook_limits: Default::default(),
            admin_token_hash: None,
            streams: Default::default(),
            authz: Default::default(),
            audit: crate::audit::Audit::new(None),
        });
        db::enrol(&app.db, db::DEFAULT_TENANT, "n").await.unwrap();
        crate::people::agent_admit(&app.db, db::DEFAULT_TENANT, "*@n", "admin").await.unwrap();
        app
    }

    fn ev(channel: &str, kind: &str, subject: &str) -> Event {
        Event {
            channel: channel.into(),
            kind: kind.into(),
            subject: Some(subject.into()),
            at: jiff::Timestamp::now(),
            dedupe_key: None,
            summary: format!("{kind} on {subject}"),
            body: Value::Null,
        }
    }

    fn watch(channel: &str, subject: Option<&str>, kinds: &[&str]) -> WatchRequest {
        WatchRequest {
            channel: channel.into(),
            subject: subject.map(str::to_string),
            kinds: kinds.iter().map(|k| k.to_string()).collect(),
            target: "me@n".into(),
            note: Some("the deploy".into()),
            for_secs: None,
        }
    }

    fn targets(r: &fleetlayer_protocol::link::PublishResult) -> Vec<String> {
        r.deliveries.iter().map(|d| d.target.clone()).collect()
    }

    #[tokio::test]
    async fn a_watch_fires_once_on_the_first_matching_event() {
        let app = app().await;
        let t = db::DEFAULT_TENANT;
        let v = add(&app, t, &watch("ci", Some("repo@main"), &["ci.done"])).await.unwrap();
        assert!(v["id"].as_str().unwrap().starts_with("w_"));
        assert_eq!(list(&app.db, t, Some("n")).await.unwrap().len(), 1);
        assert_eq!(list(&app.db, t, Some("other")).await.unwrap().len(), 0);
        // Another kind, another subject: not yet.
        let r = crate::route::publish(&app, t, ev("ci", "ci.started", "repo@main"), "test").await.unwrap();
        assert!(targets(&r).is_empty());
        let r = crate::route::publish(&app, t, ev("ci", "ci.done", "repo@fix"), "test").await.unwrap();
        assert!(targets(&r).is_empty());
        // The one it waits for (delivery fails: no node is linked, but it went to the watcher), then never again.
        let r = crate::route::publish(&app, t, ev("ci", "ci.done", "repo@main"), "test").await.unwrap();
        assert_eq!(targets(&r), vec!["me@n"]);
        assert!(list(&app.db, t, None).await.unwrap().is_empty(), "fired and removed");
        let r = crate::route::publish(&app, t, ev("ci", "ci.done", "repo@main"), "test").await.unwrap();
        assert!(targets(&r).is_empty());
        assert_eq!(
            fired_line(&Fired { id: "w_1".into(), target: "me@n".into(), note: Some("the deploy".into()) }),
            "This was your watch w_1 (\"the deploy\"): it has fired and ended."
        );
    }

    #[tokio::test]
    async fn a_subscriber_that_watches_too_gets_the_event_once() {
        let app = app().await;
        let t = db::DEFAULT_TENANT;
        let s = db::Subscription { channel: "ci".into(), subject: None, target: "me@n".into() };
        db::subscribe(&app.db, t, &s).await.unwrap();
        add(&app, t, &watch("ci", None, &[])).await.unwrap();
        let r = crate::route::publish(&app, t, ev("ci", "ci.done", "x"), "test").await.unwrap();
        assert_eq!(targets(&r), vec!["me@n"]);
        assert!(list(&app.db, t, None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_session_already_in_the_status_fires_at_once() {
        let app = app().await;
        let t = db::DEFAULT_TENANT;
        let s = json!({"sessionId": "id-a", "meta": {"tab": "a", "status": "idle", "running": true}, "ended": null});
        db::replace_sessions(&app.db, t, "n", &[s]).await.unwrap();
        let v = add(&app, t, &watch("session.status", Some("a@n"), &["session.idle", "session.done"])).await.unwrap();
        assert_eq!(v["fired"], json!(true));
        assert_eq!(v["summary"], json!("a@n is idle already"));
        assert!(list(&app.db, t, None).await.unwrap().is_empty());
        // Working: it waits.
        let v = add(&app, t, &watch("session.status", Some("a@n"), &["session.working"])).await.unwrap();
        assert!(v["id"].is_string());
    }

    #[tokio::test]
    async fn a_watch_past_its_deadline_ends_and_bad_ones_are_refused() {
        let app = app().await;
        let t = db::DEFAULT_TENANT;
        add(&app, t, &watch("ci", None, &[])).await.unwrap();
        assert_eq!(expire(&app).await.unwrap(), 0);
        sqlx::query("UPDATE watches SET expires_at = 0").execute(&app.db).await.unwrap();
        assert_eq!(expire(&app).await.unwrap(), 1);
        assert!(list(&app.db, t, None).await.unwrap().is_empty());
        let mut w = watch("ci", None, &[]);
        w.for_secs = Some(31 * 24 * 3600);
        assert!(add(&app, t, &w).await.is_err(), "longer than 30 days");
        let w = watch("bad channel", None, &[]);
        assert!(add(&app, t, &w).await.is_err());
        // Only the node's own sessions' watches can be removed.
        let id = add(&app, t, &watch("ci", None, &[])).await.unwrap()["id"].as_str().unwrap().to_string();
        assert!(!remove(&app.db, t, &id, "other").await.unwrap());
        assert!(!remove(&app.db, t, &id, "_").await.unwrap(), "a node name is matched exactly, never as a pattern");
        assert!(remove(&app.db, t, &id, "n").await.unwrap());
    }

    #[tokio::test]
    async fn a_watch_whose_session_may_not_read_the_session_is_refused_and_never_consumed() {
        let app = app().await;
        let t = db::DEFAULT_TENANT;
        let s = json!({"sessionId": "id-p", "meta": {"tab": "private", "status": "working", "running": true}, "ended": null});
        db::replace_sessions(&app.db, t, "n", &[s]).await.unwrap();
        let policy = "version: 2\ndefault: allow\nask: []\ndeny:\n  - { who: \"me@n\", can: read, to: [\"id-p@n\"] }\n";
        crate::authz::store(&app.db, t, policy).await.unwrap();
        // Refused when set, under the session's other name too.
        let e = add(&app, t, &watch("session.status", Some("private@n"), &["session.idle"])).await.unwrap_err();
        assert!(e.message.contains("may not read"), "{}", e.message);
        // A watch set before the session was known isn't taken by an event it may not read.
        sqlx::query(
            "INSERT INTO watches (tenant, id, channel, subject, kinds, target, expires_at, created_at)
             VALUES ('default', 'w_old', 'session.status', 'private@n', '[]', 'me@n', ?, 0)",
        )
        .bind(db::now_ms() + 60_000)
        .execute(&app.db)
        .await
        .unwrap();
        let mut idle = ev("session.status", "session.idle", "private@n");
        idle.body = json!({"node": "n", "sessionId": "id-p", "tab": "private", "status": "idle"});
        let r = crate::route::publish(&app, t, idle, "test").await.unwrap();
        assert!(targets(&r).is_empty());
        assert_eq!(list(&app.db, t, None).await.unwrap().len(), 1, "still there");
    }

    #[tokio::test]
    async fn a_watch_never_takes_the_place_of_the_fallback() {
        let app = app().await;
        let t = db::DEFAULT_TENANT;
        sqlx::query("INSERT INTO channels (tenant, name, fallback) VALUES ('default', 'alerts', 'oncall@n')")
            .execute(&app.db)
            .await
            .unwrap();
        add(&app, t, &watch("alerts", None, &[])).await.unwrap();
        let r = crate::route::publish(&app, t, ev("alerts", "alert.firing", "disk"), "test").await.unwrap();
        let mut got = targets(&r);
        got.sort();
        assert_eq!(got, vec!["me@n", "oncall@n"]);
    }

    #[tokio::test]
    async fn another_tenants_node_of_the_same_name_is_never_seen() {
        let app = app().await;
        let t = db::DEFAULT_TENANT;
        let theirs =
            json!({"sessionId": "id-b", "meta": {"tab": "secret", "status": "idle", "running": true}, "ended": null});
        db::replace_sessions(&app.db, "other", "n", &[theirs]).await.unwrap();
        // Our node n pushes its whole list: the other tenant's session on its own n isn't ours, so it isn't "gone".
        let ours = json!({"sessionId": "id-a", "meta": {"tab": "a", "status": "idle", "running": true}, "ended": null});
        let old = db::node_sessions(&app.db, t, "n").await.unwrap();
        assert!(old.is_empty());
        let diff = crate::status::diff(&old, &[ours], true);
        assert!(diff.iter().all(|(_, _, s)| s["sessionId"] == json!("id-a")), "{diff:?}");
        // A watch on its name doesn't fire from its status.
        let v = add(&app, t, &watch("session.status", Some("secret@n"), &["session.idle"])).await.unwrap();
        assert!(v["id"].is_string(), "not fired from another tenant's session: {v}");
        assert_eq!(
            crate::authz::session_names(&app, t, "id-b@n").await,
            vec!["id-b@n".to_string()],
            "no names from the other tenant"
        );
    }

    #[test]
    fn only_the_control_plane_publishes_status_and_access() {
        assert!(crate::route::outside_publish_allowed("session.status").is_err());
        assert!(crate::route::outside_publish_allowed("access").is_err());
        assert!(crate::route::outside_publish_allowed("alerts").is_ok());
    }

    #[tokio::test]
    async fn a_webhook_endpoint_can_t_publish_on_the_control_planes_channels() {
        let app = app().await;
        assert!(db::add_endpoint(&app.db, db::DEFAULT_TENANT, "x", "session.status", None, false).await.is_err());
    }
}
