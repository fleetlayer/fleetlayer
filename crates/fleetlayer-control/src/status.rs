//! The `session.status` channel: an event each time a session's status changes (working, idle, done, blocked on a
//! prompt, unknown, or gone), found by comparing what each node pushes with what it pushed before, so it works with
//! every node version.
//!
//! A `working` event says what started the turn (`turn.cause`: `person`, `message`, `scheduler` or `unknown`), from
//! the latest turn signal the node or the harness reported (`meta.turn`: a delivery, an act typed in for someone, a
//! prompt submitted in the agent), each signal used for one turn at most. Working after blocked with no signal is the
//! person answering in the tab. A `blocked` event carries the prompt.
//!
//! The signal can reach the control plane a moment after the status does (the harness's hook runs on its own), so
//! `working` and `blocked` events wait [`SETTLE`] and read the session again before they go out. Every event goes
//! through one queue in order, so a session's events never overtake each other.
//!
//! Events are delivered like any other channel's, except that a subscriber gets one only when it may `read` the
//! session it's about. They're kept [`KEEP`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use fleetlayer_protocol::ext::{Event, Turn, TurnCause};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::App;
use crate::db;

pub const CHANNEL: &str = "session.status";
/// How long a `working` or `blocked` event waits for the signal or the prompt that goes with it.
pub const SETTLE: Duration = Duration::from_secs(3);
/// How long the events are kept.
pub const KEEP: Duration = Duration::from_secs(7 * 24 * 3600);
/// A turn signal older than this when the session starts working didn't start it.
const SIGNAL_FRESH_MS: i64 = 10 * 60 * 1000;

/// One status change, waiting in the queue.
#[derive(Debug, Clone)]
pub struct Change {
    tenant: String,
    node: String,
    session_id: String,
    previous: Option<String>,
    status: String,
    /// The session as last pushed (as it was, for a session that's gone).
    session: Value,
    ready_at: tokio::time::Instant,
}

/// A session's status for this channel: `gone` once it has ended or stopped running.
pub fn status_of(s: &Value) -> String {
    if !s["ended"].is_null() || s["meta"]["running"] == json!(false) {
        return "gone".into();
    }
    s["meta"]["status"].as_str().unwrap_or("unknown").to_string()
}

/// What changed between a node's sessions before and the ones it pushed now: (previous status, new status, session).
/// With `full`, `new` is the whole list, so a session missing from it is gone.
pub fn diff(old: &[Value], new: &[Value], full: bool) -> Vec<(Option<String>, String, Value)> {
    let id = |s: &Value| s["sessionId"].as_str().map(str::to_string);
    let before: HashMap<String, &Value> = old.iter().filter_map(|s| Some((id(s)?, s))).collect();
    let mut out = vec![];
    for s in new {
        let Some(sid) = id(s) else { continue };
        let now = status_of(s);
        let was = before.get(&sid).map(|o| status_of(o));
        if was.as_deref() != Some(now.as_str()) && !(was.is_none() && now == "gone") {
            out.push((was, now, s.clone()));
        }
    }
    if full {
        let listed: std::collections::HashSet<String> = new.iter().filter_map(id).collect();
        for (sid, o) in &before {
            let was = status_of(o);
            if !listed.contains(sid) && was != "gone" {
                out.push((Some(was), "gone".into(), (*o).clone()));
            }
        }
    }
    out
}

/// Queue the changes between `old` and `new` for node `node`'s sessions.
pub fn observe(app: &Arc<App>, tenant: &str, node: &str, old: &[Value], new: &[Value], full: bool) {
    for (previous, status, session) in diff(old, new, full) {
        let wait = if matches!(status.as_str(), "working" | "blocked") { SETTLE } else { Duration::ZERO };
        let session_id = session["sessionId"].as_str().unwrap_or_default().to_string();
        let c = Change {
            tenant: tenant.into(),
            node: node.into(),
            session_id,
            previous,
            status,
            session,
            ready_at: tokio::time::Instant::now() + wait,
        };
        queue(app, c);
    }
}

fn queue(app: &Arc<App>, c: Change) {
    let mut q = app.links.status_queue.lock().expect("status queue");
    if q.as_ref().is_none_or(|tx| tx.is_closed()) {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run(Arc::downgrade(app), rx));
        *q = Some(tx);
    }
    if let Some(tx) = q.as_ref() {
        let _ = tx.send(c);
    }
}

/// The queue's one worker: each change in order, once it's ready. Ends when the control plane does.
async fn run(app: std::sync::Weak<App>, mut rx: mpsc::UnboundedReceiver<Change>) {
    // The turn signal each session's last `working` event used, so a signal starts one turn at most.
    let mut used: HashMap<(String, String, String), jiff::Timestamp> = HashMap::new();
    let mut pruned = tokio::time::Instant::now() - Duration::from_secs(3600);
    while let Some(c) = rx.recv().await {
        tokio::time::sleep_until(c.ready_at).await;
        let Some(app) = app.upgrade() else { return };
        if pruned.elapsed() >= Duration::from_secs(3600) {
            pruned = tokio::time::Instant::now();
            if let Err(e) = db::prune_channel(&app.db, CHANNEL, KEEP.as_millis() as i64).await {
                tracing::warn!("pruning old session.status events: {e}");
            }
        }
        let key = (c.tenant.clone(), c.node.clone(), c.session_id.clone());
        // The session as it is now, for the signal or prompt that came after the status.
        let current = if c.status == "gone" {
            None
        } else {
            db::session_data(&app.db, &c.tenant, &c.node, &c.session_id).await.ok().flatten()
        };
        let session = current.as_ref().unwrap_or(&c.session);
        let mut turn = None;
        if c.status == "working" {
            let signal: Option<Turn> = serde_json::from_value(session["meta"]["turn"].clone()).ok();
            let now = jiff::Timestamp::now();
            let fresh = signal.filter(|t| {
                used.get(&key).is_none_or(|u| t.at > *u)
                    && now.duration_since(t.at).as_millis() < SIGNAL_FRESH_MS as i128
            });
            turn = Some(match fresh {
                Some(t) => {
                    used.insert(key.clone(), t.at);
                    t
                }
                None if c.previous.as_deref() == Some("blocked") => {
                    Turn { cause: TurnCause::Person, at: now, from: None, detail: Some("answered".into()) }
                }
                None => Turn { cause: TurnCause::Unknown, at: now, from: None, detail: None },
            });
        }
        if c.status == "gone" {
            used.remove(&key);
        }
        let e = event(&c, session, turn.as_ref());
        if let Err(err) = crate::route::publish(&app, &c.tenant, e, "fleetlayer").await {
            tracing::warn!(session = %c.session_id, "publishing a session.status event: {err}");
        }
    }
}

/// `<tab>@<node>`, or `<session ID>@<node>` for a session with no tab.
pub fn subject(node: &str, session: &Value, session_id: &str) -> String {
    let name = session["meta"]["tab"].as_str().filter(|t| !t.is_empty()).unwrap_or(session_id);
    format!("{name}@{node}")
}

fn event(c: &Change, session: &Value, turn: Option<&Turn>) -> Event {
    let subject = subject(&c.node, session, &c.session_id);
    let cause = turn.map(|t| match t.cause {
        TurnCause::Person => "a person",
        TurnCause::Message => "a message",
        TurnCause::Scheduler => "a scheduler entry",
        TurnCause::Unknown => "nothing seen",
    });
    let summary = match (c.status.as_str(), cause) {
        ("working", Some(cause)) => format!("{subject} is working (started by {cause})"),
        ("blocked", _) => format!("{subject} is blocked on a prompt"),
        ("gone", _) => format!("{subject} has gone"),
        (s, _) => format!("{subject} is {s}"),
    };
    let meta = &session["meta"];
    let mut body = json!({
        "node": c.node,
        "sessionId": c.session_id,
        "tab": meta["tab"],
        "title": session["title"],
        "cwd": session["cwd"],
        "status": c.status,
        "previous": c.previous,
        "statusChangedAt": meta["statusChangedAt"],
    });
    if let Some(t) = turn {
        body["turn"] = json!(t);
    }
    if c.status == "blocked" && !meta["prompt"].is_null() {
        body["prompt"] = meta["prompt"].clone();
    }
    Event {
        channel: CHANNEL.into(),
        kind: format!("session.{}", c.status),
        subject: Some(subject),
        at: jiff::Timestamp::now(),
        dedupe_key: None,
        summary,
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(id: &str, status: &str) -> Value {
        json!({"sessionId": id, "meta": {"tab": id, "status": status, "running": true}, "ended": null})
    }

    #[test]
    fn changes_are_found_between_pushes() {
        let old = vec![s("a", "idle"), s("b", "working"), s("c", "done")];
        let new = vec![s("a", "working"), s("b", "working"), s("d", "idle")];
        let d = diff(&old, &new, true);
        let got: Vec<_> =
            d.iter().map(|(w, n, s)| (s["sessionId"].as_str().unwrap(), w.as_deref(), n.as_str())).collect();
        assert!(got.contains(&("a", Some("idle"), "working")));
        assert!(got.contains(&("d", None, "idle")), "a new session");
        assert!(got.contains(&("c", Some("done"), "gone")), "missing from a whole list");
        assert_eq!(got.len(), 3, "b didn't change: {got:?}");
        // One session's push isn't the whole list.
        assert_eq!(diff(&old, &[s("a", "idle")], false).len(), 0);
        let mut ended = s("a", "done");
        ended["ended"] = json!({"at": "2026-11-28T00:00:00Z"});
        assert_eq!(diff(&old, &[ended.clone()], false)[0].1, "gone");
        assert!(diff(&[], &[ended], false).is_empty(), "never seen running: nothing to say");
    }

    async fn app() -> Arc<App> {
        Arc::new(App {
            db: crate::db::open_memory().await.unwrap(),
            links: Default::default(),
            hook_limits: Default::default(),
            admin_token_hash: None,
            streams: Default::default(),
            authz: Default::default(),
            audit: crate::audit::Audit::new(None),
        })
    }

    /// Push `new` as node n's whole list, as a node does.
    async fn push(app: &Arc<App>, new: Vec<Value>) {
        let old = db::node_sessions(&app.db, "n").await.unwrap();
        db::replace_sessions(&app.db, db::DEFAULT_TENANT, "n", &new).await.unwrap();
        observe(app, db::DEFAULT_TENANT, "n", &old, &new, true);
    }

    async fn events(app: &App, n: usize) -> Vec<Value> {
        for _ in 0..100 {
            let e = db::events_after(&app.db, db::DEFAULT_TENANT, 0, &[CHANNEL.into()], None, 100).await.unwrap();
            if e.len() >= n {
                return e;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("fewer than {n} events");
    }

    #[tokio::test]
    async fn status_changes_become_events_with_what_started_each_turn() {
        let app = app().await;
        push(&app, vec![s("a", "idle")]).await;
        // A message was delivered, then the session started working: the message started it.
        let mut a = s("a", "working");
        a["meta"]["turn"] = json!({"cause": "message", "at": jiff::Timestamp::now(), "from": "ci (laptop)"});
        push(&app, vec![a.clone()]).await;
        let mut blocked = s("a", "blocked");
        blocked["meta"]["turn"] = a["meta"]["turn"].clone();
        blocked["meta"]["prompt"] = json!({"id": "p1", "kind": "question"});
        push(&app, vec![blocked.clone()]).await;
        // Answered in the tab: working again with no new signal.
        let mut again = blocked.clone();
        again["meta"]["status"] = json!("working");
        again["meta"]["prompt"] = Value::Null;
        push(&app, vec![again.clone()]).await;
        let mut idle = again.clone();
        idle["meta"]["status"] = json!("idle");
        push(&app, vec![idle.clone()]).await;
        // Working once more, the old message's signal already used: nothing seen.
        push(&app, vec![again]).await;
        push(&app, vec![]).await;
        let e = events(&app, 7).await;
        let got: Vec<_> = e
            .iter()
            .map(|e| (e["kind"].as_str().unwrap().to_string(), e["body"]["turn"]["cause"].as_str().map(str::to_string)))
            .collect();
        let want = |k: &str, c: Option<&str>| (k.to_string(), c.map(str::to_string));
        assert_eq!(
            got,
            vec![
                want("session.idle", None),
                want("session.working", Some("message")),
                want("session.blocked", None),
                want("session.working", Some("person")),
                want("session.idle", None),
                want("session.working", Some("unknown")),
                want("session.gone", None),
            ][..],
            "{e:#?}"
        );
        assert_eq!(e[1]["subject"], json!("a@n"));
        assert_eq!(e[1]["body"]["turn"]["from"], json!("ci (laptop)"));
        assert_eq!(e[2]["body"]["prompt"]["id"], json!("p1"));
        assert_eq!(e[3]["body"]["turn"]["detail"], json!("answered"));
        assert_eq!(e[3]["body"]["previous"], json!("blocked"));
    }
}
