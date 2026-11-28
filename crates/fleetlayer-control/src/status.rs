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
/// How long the events are kept, at most.
pub const KEEP: Duration = Duration::from_secs(7 * 24 * 3600);
/// How long every channel's events are kept unless `control.yaml` says otherwise (`event_days`).
pub const DEFAULT_EVENT_DAYS: u32 = 30;
static EVENT_DAYS: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
/// The most changes waiting in the queue; more are dropped (and logged) until it drains.
pub const QUEUE_MAX: usize = 10_000;
/// The most events one session gets a minute. Past that its latest change is held and sent when the minute ends (so
/// its last status always arrives), saying how many changes weren't reported (`dropped`).
pub const PER_SESSION_PER_MINUTE: u32 = 30;
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
    /// Changes of this session dropped (over its rate, or a full queue) since its last event.
    dropped: u32,
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
            dropped: 0,
        };
        queue(app, c);
    }
}

/// The queue: its sender, and how many events each session got this minute.
pub struct Queue {
    tx: mpsc::Sender<Change>,
    minute: HashMap<(String, String, String), Minute>,
}

/// One session's current minute.
struct Minute {
    start: tokio::time::Instant,
    sent: u32,
    /// Changes not reported since its last event.
    dropped: u32,
    /// The latest change past its limit, sent when the minute ends, so the last status always arrives.
    held: Option<Change>,
}

fn queue(app: &Arc<App>, c: Change) {
    let mut q = app.links.status_queue.lock().expect("status queue");
    if q.as_ref().is_none_or(|q| q.tx.is_closed()) {
        let (tx, rx) = mpsc::channel(QUEUE_MAX);
        tokio::spawn(run(Arc::downgrade(app), rx));
        *q = Some(Queue { tx, minute: HashMap::new() });
    }
    let Some(q) = q.as_mut() else { return };
    let now = tokio::time::Instant::now();
    if q.minute.len() > 10_000 {
        q.minute.retain(|_, m| now.duration_since(m.start) < Duration::from_secs(60) || m.held.is_some());
    }
    let key = (c.tenant.clone(), c.node.clone(), c.session_id.clone());
    let m = q.minute.entry(key.clone()).or_insert(Minute { start: now, sent: 0, dropped: 0, held: None });
    if now.duration_since(m.start) >= Duration::from_secs(60) && m.held.is_none() {
        (m.start, m.sent) = (now, 0);
    }
    if m.sent >= PER_SESSION_PER_MINUTE {
        // Over its limit: hold the latest, and send it once the minute is over.
        if m.held.replace(c).is_some() {
            m.dropped += 1;
        } else {
            let (app, at) = (Arc::downgrade(app), m.start + Duration::from_secs(60));
            tokio::spawn(async move {
                tokio::time::sleep_until(at).await;
                if let Some(app) = app.upgrade() {
                    release(&app, &key);
                }
            });
        }
        return;
    }
    send(&q.tx, m, c);
}

/// Send one change, with the count of those not reported before it.
fn send(tx: &mpsc::Sender<Change>, m: &mut Minute, mut c: Change) {
    c.dropped = std::mem::take(&mut m.dropped);
    match tx.try_send(c) {
        Ok(()) => m.sent += 1,
        Err(mpsc::error::TrySendError::Full(c)) => {
            m.dropped += 1 + c.dropped;
            tracing::warn!(session = %c.session_id, "the session.status queue is full: a change wasn't reported");
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {}
    }
}

/// A session's minute is over: send the change it held, starting a new minute.
fn release(app: &Arc<App>, key: &(String, String, String)) {
    let mut q = app.links.status_queue.lock().expect("status queue");
    let Some(q) = q.as_mut() else { return };
    let Some(m) = q.minute.get_mut(key) else { return };
    let Some(c) = m.held.take() else { return };
    (m.start, m.sent) = (tokio::time::Instant::now(), 0);
    let mut c = c;
    // Its own wait (for a signal or a prompt) starts now.
    c.ready_at = c.ready_at.max(tokio::time::Instant::now());
    send(&q.tx, m, c);
}

/// How long every channel's events are kept, from `control.yaml`, set once at start.
pub fn keep_events_for(days: u32) {
    let _ = EVENT_DAYS.set(days.max(1));
}

/// Every hour, forget events older than their retention: `event_days` for every channel, and at most [`KEEP`] for
/// `session.status`.
pub async fn prune_events(app: Arc<App>) {
    loop {
        let days = i64::from(*EVENT_DAYS.get().unwrap_or(&DEFAULT_EVENT_DAYS));
        let all = days * 24 * 3600 * 1000;
        let status = all.min(KEEP.as_millis() as i64);
        if let Err(e) = db::prune_events(&app.db, all, CHANNEL, status).await {
            tracing::warn!("pruning old events: {e}");
        }
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

/// The queue's one worker: each change in order, once it's ready. Ends when the control plane does.
async fn run(app: std::sync::Weak<App>, mut rx: mpsc::Receiver<Change>) {
    // The turn signal each session's last `working` event used, so a signal starts one turn at most.
    let mut used: HashMap<(String, String, String), jiff::Timestamp> = HashMap::new();
    while let Some(c) = rx.recv().await {
        tokio::time::sleep_until(c.ready_at).await;
        let Some(app) = app.upgrade() else { return };
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
        // The prompt, title, tab and folder are the session's own words: its source says so.
        let source = format!(
            "fleetlayer session status, with text written by {} (its prompt, title, tab and cwd: outside data)",
            e.subject.as_deref().unwrap_or_default()
        );
        if let Err(err) = crate::route::publish(&app, &c.tenant, e, &source).await {
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
        "outside_data": ["prompt", "title", "tab", "cwd"],
        "written_by": subject,
    });
    if c.dropped > 0 {
        body["dropped"] = json!(c.dropped);
    }
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

    #[tokio::test]
    async fn a_flapping_session_is_rate_limited_and_its_events_are_marked_as_its_words() {
        let app = app().await;
        let mut old = vec![];
        for i in 0..40 {
            let new = vec![s("f", if i % 2 == 0 { "idle" } else { "done" })];
            observe(&app, db::DEFAULT_TENANT, "n", &old, &new, true);
            old = new;
        }
        let e = events(&app, PER_SESSION_PER_MINUTE as usize).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let e2 = db::events_after(&app.db, db::DEFAULT_TENANT, 0, &[CHANNEL.into()], None, 100).await.unwrap();
        assert_eq!(e2.len(), PER_SESSION_PER_MINUTE as usize, "at most {PER_SESSION_PER_MINUTE} a minute");
        assert_eq!(e[0]["body"]["outside_data"], json!(["prompt", "title", "tab", "cwd"]));
        assert_eq!(e[0]["body"]["written_by"], json!("f@n"));
        assert!(e[0]["source"].as_str().unwrap().contains("written by f@n"), "{}", e[0]["source"]);
        {
            let q = app.links.status_queue.lock().unwrap();
            let m = q.as_ref().unwrap().minute.values().next().unwrap();
            assert_eq!(m.dropped, 9, "counted, for the held event to say");
            let held = m.held.as_ref().expect("the latest change is held for the end of the minute");
            assert_eq!(held.status, "done", "the 40th push");
        }
        // The minute ends: the held change goes out, saying how many weren't reported.
        release(&app, &(db::DEFAULT_TENANT.to_string(), "n".to_string(), "f".to_string()));
        let e = events(&app, PER_SESSION_PER_MINUTE as usize + 1).await;
        let last = e.last().unwrap();
        assert_eq!((last["kind"].clone(), last["body"]["dropped"].clone()), (json!("session.done"), json!(9)));
    }

    #[tokio::test]
    async fn old_events_are_pruned_and_session_status_sooner() {
        let app = app().await;
        let day = 24 * 3600 * 1000;
        let old = |channel: &str, days: i64| fleetlayer_protocol::ext::Event {
            channel: channel.into(),
            kind: "k".into(),
            subject: None,
            at: jiff::Timestamp::from_millisecond(db::now_ms() - days * day).unwrap(),
            dedupe_key: None,
            summary: "s".into(),
            body: Value::Null,
        };
        for (c, d) in [("ci", 40), ("ci", 10), (CHANNEL, 10), (CHANNEL, 1)] {
            db::add_event(&app.db, db::DEFAULT_TENANT, &old(c, d), "test").await.unwrap();
        }
        let n = db::prune_events(&app.db, 30 * day, CHANNEL, 7 * day).await.unwrap();
        assert_eq!(n, 2);
        let left = db::events_after(&app.db, db::DEFAULT_TENANT, 0, &[], None, 10).await.unwrap();
        assert_eq!(left.len(), 2);
    }
}
