use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use async_trait::async_trait;
use fleetlayer_protocol::ext::{Event, Reading};
use fleetlayer_protocol::link::AccessDecision;
use fleetlayer_protocol::rpc::RpcError;
use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};

use crate::engine::*;
use crate::runner::{self, Authority, Paths};
use crate::settings::{self, Settings, payload_key};

const SETTINGS: &str = r#"
kinds:
  build-server:
    slots: 2
    ttl: 2h
    idle: 30m
  gmail-filter:
    approval: true
    ttl: 24h
    max_per_holder: 2
    events: leases
    payload:
      required: [account, query]
      fields:
        account: '^[a-z0-9_-]{1,40}$'
        query: 'from:\S+'
"#;

fn s() -> Settings {
    settings::parse(SETTINGS).unwrap()
}

fn t0() -> Timestamp {
    "2026-11-28T12:00:00Z".parse().unwrap()
}

fn mins(m: i64) -> SignedDuration {
    SignedDuration::from_mins(m)
}

fn build(holder: &str, wait: bool) -> Request {
    Request {
        kind: "build-server".into(),
        reason: "cargo test for leases".into(),
        holder: holder.into(),
        wait,
        ..Default::default()
    }
}

fn started(changes: &[Change]) -> Vec<u64> {
    changes.iter().filter_map(|c| if let Change::Started { lease, .. } = c { Some(lease.id) } else { None }).collect()
}

fn ended(changes: &[Change]) -> Vec<(u64, EndReason)> {
    changes
        .iter()
        .filter_map(|c| if let Change::Ended { ended, .. } = c { Some((ended.lease.id, ended.why)) } else { None })
        .collect()
}

fn fleet(sessions: &[(&str, &str, &str, Timestamp)]) -> FleetView {
    let mut f = FleetView::default();
    for (node, tab, status, since) in sessions {
        f.online.insert(node.to_string());
        f.sessions.insert(
            (node.to_string(), tab.to_string()),
            SessionInfo { resting: matches!(*status, "idle" | "done" | "blocked"), since: Some(*since) },
        );
    }
    f
}

#[test]
fn settings_are_checked_all_at_once() {
    let bad = r#"
kinds:
  Bad Name: { slots: 1 }
  zero: { slots: 0 }
  short: { ttl: 10s }
  approve: { approval: true }
  long: { ttl: 2d, max_ttl: 1d }
"#;
    let e = settings::parse(bad).unwrap_err();
    for want in ["'Bad Name'", "'zero'", "'short'", "'approve'", "'long'"] {
        assert!(e.contains(want), "{want} in {e}");
    }
    let s = s();
    assert_eq!(s.kinds["build-server"].slots, Some(2));
    assert_eq!(s.kinds["gmail-filter"].stuck_after, SignedDuration::from_hours(24), "defaults to the ttl");
    assert!(settings::parse("").unwrap().kinds.is_empty());
}

#[test]
fn payloads_are_checked_and_keyed_whatever_their_order() {
    let k = &s().kinds["gmail-filter"];
    let a = k.check_payload(Some(&json!({"account": "work", "query": "from:ci@example.com"}))).unwrap().unwrap();
    let b = k.check_payload(Some(&json!({"query": "from:ci@example.com", "account": "work"}))).unwrap().unwrap();
    assert_eq!(payload_key(&a), payload_key(&b));
    assert_eq!(payload_key(&a).len(), 16);
    for (p, want) in [
        (json!({"account": "work"}), "needs 'query'"),
        (json!({"account": "work", "query": "label:x"}), "doesn't match"),
        (json!({"account": "work", "query": "from:x", "extra": "y"}), "isn't one of"),
        (json!({"account": 3, "query": "from:x"}), "must be a string"),
    ] {
        let e = k.check_payload(Some(&p)).unwrap_err();
        assert!(e.contains(want), "{p}: {e}");
    }
    assert!(k.check_payload(None).unwrap_err().contains("needs a payload"));
    assert!(
        s().kinds["build-server"].check_payload(Some(&json!({"a": "b"}))).unwrap_err().contains("takes no payload")
    );
}

#[test]
fn slots_are_handed_out_first_come_first_served() {
    let s = s();
    let mut e = Engine::new(State::default(), t0());
    let (a, ch) = e.acquire(&s, build("a@laptop", false), t0()).unwrap();
    assert_eq!(started(&ch), vec![a]);
    let (b, _) = e.acquire(&s, build("b@laptop", false), t0()).unwrap();
    assert_eq!(e.get(b).unwrap().phase, Phase::Held);
    // Full: without --wait it's refused, naming who holds.
    match e.acquire(&s, build("c@laptop", false), t0()) {
        Err(Refusal::Busy { holders, waiting }) => {
            assert_eq!(holders.len(), 2);
            assert!(holders[0].contains("a@laptop") && holders[0].contains("cargo test"), "{holders:?}");
            assert_eq!(waiting, 0);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(e.state.leases.len(), 2, "a refused request leaves nothing behind");
    let (c, _) = e.acquire(&s, build("c@laptop", true), t0()).unwrap();
    let (d, _) = e.acquire(&s, build("d@server", true), t0() + mins(1)).unwrap();
    assert_eq!((e.position(c), e.position(d)), (Some(1), Some(2)));
    // b releases: c, the first in line, gets the slot; d moves up.
    let ch = e.release(&s, b, Some("b@laptop"), t0() + mins(5)).unwrap();
    assert_eq!(ended(&ch), vec![(b, EndReason::Released)]);
    assert_eq!(started(&ch), vec![c]);
    assert_eq!(e.position(d), Some(1));
    // Only the holder's node may release it.
    assert!(e.release(&s, a, Some("x@server"), t0()).unwrap_err().contains("only its own node"));
    e.release(&s, a, Some("other-tab@laptop"), t0()).unwrap();
    assert_eq!(e.get(d).unwrap().phase, Phase::Held);
    // Releasing again is fine.
    assert!(e.release(&s, a, Some("a@laptop"), t0()).unwrap().is_empty());
    assert!(e.release(&s, 999, None, t0()).is_err());
}

#[test]
fn a_waiting_session_that_stops_waiting_leaves_the_queue() {
    let s = s();
    let mut e = Engine::new(State::default(), t0());
    e.acquire(&s, build("a@laptop", false), t0()).unwrap();
    e.acquire(&s, build("b@laptop", false), t0()).unwrap();
    let (c, _) = e.acquire(&s, build("c@laptop", true), t0()).unwrap();
    assert!(e.attach(c));
    let ch = e.detach(&s, c, t0() + mins(1));
    assert_eq!(ended(&ch), vec![(c, EndReason::Cancelled)]);
    assert!(e.get(c).is_none());
}

#[test]
fn leases_end_at_their_ttl_and_the_problem_shows_for_a_while() {
    let s = s();
    let mut e = Engine::new(State::default(), t0());
    let mut r = build("a@laptop", false);
    r.ttl = Some(mins(30));
    let (a, _) = e.acquire(&s, r, t0()).unwrap();
    assert_eq!(e.get(a).unwrap().expires_at, Some(t0() + mins(30)));
    let f = fleet(&[("laptop", "a", "working", t0())]);
    assert!(ended(&e.tick(&s, Some(&f), t0() + mins(29))).is_empty());
    assert_eq!(ended(&e.tick(&s, Some(&f), t0() + mins(30))), vec![(a, EndReason::Expired)]);
    assert_eq!(e.recently_expired(t0() + mins(40)).len(), 1);
    assert!(e.recently_expired(t0() + mins(61)).is_empty());
    // Longer than the kind allows is refused.
    let mut r = build("a@laptop", false);
    r.ttl = Some(SignedDuration::from_hours(25));
    assert!(matches!(e.acquire(&s, r, t0()), Err(Refusal::Invalid(m)) if m.contains("longer than")));
}

#[test]
fn a_session_holder_that_dies_or_idles_loses_its_lease() {
    let s = s();
    let mut e = Engine::new(State::default(), t0());
    let (a, _) = e.acquire(&s, build("a@laptop", false), t0()).unwrap();
    let (b, _) = e.acquire(&s, build("b@server", false), t0()).unwrap();
    let (c, _) = e.acquire(&s, build("c@laptop", true), t0()).unwrap();
    // a's session stops running: gone after the grace, and c moves up. b's node goes offline: longer grace.
    let mut f = fleet(&[("laptop", "c", "working", t0())]);
    f.online.insert("laptop".into());
    assert!(ended(&e.tick(&s, Some(&f), t0() + mins(1))).is_empty());
    let ch = e.tick(&s, Some(&f), t0() + mins(3));
    assert_eq!(ended(&ch), vec![(a, EndReason::SessionGone)]);
    assert_eq!(started(&ch), vec![c]);
    let ch = e.tick(&s, Some(&f), t0() + mins(6));
    assert_eq!(ended(&ch), vec![(b, EndReason::NodeGone)]);
    // c goes idle: it keeps the lease for its idle limit, counted from when it went idle.
    let f = fleet(&[("laptop", "c", "idle", t0() + mins(10))]);
    assert!(ended(&e.tick(&s, Some(&f), t0() + mins(39))).is_empty());
    assert_eq!(ended(&e.tick(&s, Some(&f), t0() + mins(40))), vec![(c, EndReason::Idle)]);
    // Without the fleet (not linked), sessions are left alone.
    let (d, _) = e.acquire(&s, build("d@laptop", false), t0() + mins(41)).unwrap();
    assert!(ended(&e.tick(&s, None, t0() + mins(100))).is_empty());
    assert!(e.get(d).is_some());
}

#[test]
fn a_session_seen_again_within_the_grace_keeps_its_lease() {
    let s = s();
    let mut e = Engine::new(State::default(), t0());
    let (a, _) = e.acquire(&s, build("a@laptop", false), t0()).unwrap();
    let mut gone = FleetView::default();
    gone.online.insert("laptop".into());
    e.tick(&s, Some(&gone), t0() + mins(1));
    e.tick(&s, Some(&fleet(&[("laptop", "a", "working", t0())])), t0() + mins(2));
    e.tick(&s, Some(&gone), t0() + mins(3));
    assert!(e.get(a).is_some(), "the grace starts again after it was seen");
}

#[test]
fn a_script_ties_its_lease_to_a_process_kept_by_a_connection() {
    let s = s();
    let mut e = Engine::new(State::default(), t0());
    // A script (no session) must name a process.
    assert!(matches!(e.acquire(&s, build("-@laptop", false), t0()), Err(Refusal::Invalid(m)) if m.contains("--pid")));
    let mut r = build("-@laptop", false);
    r.tie = Some(Tie::Session);
    r.fallback_pid = Some(4242);
    let (a, _) = e.acquire(&s, r, t0()).unwrap();
    assert_eq!(e.get(a).unwrap().tie, Tie::Process { pid: 4242 });
    // Its keeper connects within the grace and holds it for hours, sessions or not.
    assert!(e.attach(a));
    assert!(ended(&e.tick(&s, Some(&FleetView::default()), t0() + mins(90))).is_empty());
    // The keeper goes away: the lease ends after the grace.
    e.detach(&s, a, t0() + mins(91));
    assert!(ended(&e.tick(&s, None, t0() + mins(92))).is_empty());
    assert_eq!(ended(&e.tick(&s, None, t0() + mins(93))), vec![(a, EndReason::ProcessGone)]);
}

#[test]
fn after_a_restart_keepers_have_the_grace_to_come_back() {
    let s = s();
    let mut e = Engine::new(State::default(), t0());
    let mut r = build("-@laptop", false);
    r.fallback_pid = Some(7);
    let (a, _) = e.acquire(&s, r, t0()).unwrap();
    e.attach(a);
    let saved = serde_json::to_string(&e.state).unwrap();
    let mut e = Engine::new(serde_json::from_str(&saved).unwrap(), t0() + mins(60));
    assert_eq!(e.get(a).unwrap().attached, 0);
    assert!(ended(&e.tick(&s, None, t0() + mins(61))).is_empty());
    e.attach(a);
    assert!(ended(&e.tick(&s, None, t0() + mins(70))).is_empty());
}

fn filter(holder: &str, query: &str) -> Request {
    Request {
        kind: "gmail-filter".into(),
        payload: Some(json!({"account": "work", "query": query})),
        reason: "watching CI myself while I fix it".into(),
        holder: holder.into(),
        ..Default::default()
    }
}

#[test]
fn approval_kinds_ask_first_and_share_a_claim() {
    let s = s();
    let mut e = Engine::new(State::default(), t0());
    let (a, ch) = e.acquire(&s, filter("a@laptop", "from:ci@example.com"), t0()).unwrap();
    assert!(ch.is_empty());
    assert_eq!(e.get(a).unwrap().phase, Phase::Asking, "asking doesn't need --wait");
    // Asking again for the same claim gets the same lease.
    assert_eq!(e.acquire(&s, filter("a@laptop", "from:ci@example.com"), t0()).unwrap().0, a);
    e.asked(a, Some("waiting for the person to approve request r1".into()), None, t0());
    let ch = e.approved(&s, a, t0() + mins(5));
    assert!(matches!(&ch[..], [Change::Started { first_of_key: true, .. }]), "{ch:?}");
    // Another session, the same claim: started too, not first.
    let (b, _) = e.acquire(&s, filter("b@server", "from:ci@example.com"), t0()).unwrap();
    let ch = e.approved(&s, b, t0() + mins(6));
    assert!(matches!(&ch[..], [Change::Started { first_of_key: false, .. }]), "{ch:?}");
    // a ends: not the last of its claim. b ends: the last, so the filter goes.
    let ch = e.release(&s, a, Some("a@laptop"), t0() + mins(7)).unwrap();
    assert!(matches!(&ch[..], [Change::Ended { last_of_key: false, was_held: true, .. }]), "{ch:?}");
    let ch = e.release(&s, b, Some("b@server"), t0() + mins(8)).unwrap();
    assert!(matches!(&ch[..], [Change::Ended { last_of_key: true, .. }]), "{ch:?}");
    // A refusal ends it; nobody answering within a day ends it.
    let (c, _) = e.acquire(&s, filter("a@laptop", "from:x@example.com"), t0()).unwrap();
    let ch = e.asked(c, None, Some("the person refused request r2".into()), t0());
    assert!(matches!(&ch[..], [Change::Ended { was_held: false, .. }]));
    assert_eq!(e.ended(c).unwrap().why, EndReason::Refused);
    let (d, _) = e.acquire(&s, filter("a@laptop", "from:y@example.com"), t0()).unwrap();
    let f = fleet(&[("laptop", "a", "idle", t0())]);
    assert!(e.tick(&s, Some(&f), t0() + mins(60)).is_empty(), "asking isn't idle");
    assert_eq!(ended(&e.tick(&s, Some(&f), t0() + mins(24 * 60))), vec![(d, EndReason::NotApproved)]);
    // At most two per holder.
    e.acquire(&s, filter("z@laptop", "from:1@example.com"), t0()).unwrap();
    e.acquire(&s, filter("z@laptop", "from:2@example.com"), t0()).unwrap();
    assert!(
        matches!(e.acquire(&s, filter("z@laptop", "from:3@example.com"), t0()), Err(Refusal::Invalid(m)) if m.contains("the most"))
    );
}

#[test]
fn the_queue_reports_stuck_and_unknown_kinds_are_refused() {
    let s = s();
    let mut e = Engine::new(State::default(), t0());
    e.acquire(&s, build("a@laptop", false), t0()).unwrap();
    e.acquire(&s, build("b@laptop", false), t0()).unwrap();
    e.acquire(&s, build("c@laptop", true), t0()).unwrap();
    assert!(e.stuck(&s, t0() + mins(119)).is_empty());
    assert_eq!(e.stuck(&s, t0() + mins(120)).len(), 1);
    let mut r = build("a@laptop", false);
    r.kind = "nope".into();
    assert!(matches!(e.acquire(&s, r, t0()), Err(Refusal::Invalid(m)) if m.contains("build-server, gmail-filter")));
}

#[test]
fn the_fleet_listing_is_read_for_liveness() {
    let v = json!({
        "nodes": [{"name": "laptop", "online": true}, {"name": "server", "online": false}],
        "sessions": [
            {"node": "laptop", "ended": null, "meta": {"tab": "a", "status": "idle", "statusChangedAt": "2026-11-28T12:00:00Z"}},
            {"node": "laptop", "ended": null, "meta": {"tab": "a", "status": "working"}},
            {"node": "laptop", "ended": "2026-11-28T11:00:00Z", "meta": {"tab": "gone"}},
            {"node": "laptop", "ended": null, "meta": {"tab": "stopped", "running": false}},
        ]
    });
    let f = runner::parse_fleet(&v);
    assert!(f.online.contains("laptop") && !f.online.contains("server"));
    assert!(!f.sessions[&("laptop".into(), "a".into())].resting, "the tab at work counts");
    assert_eq!(f.sessions.len(), 1);
}

// ---------------------------------------------------------------- the authority, through its socket

#[derive(Default)]
struct Fake {
    fleet: StdMutex<Value>,
    decisions: StdMutex<Vec<AccessDecision>>,
    asked: StdMutex<Vec<(String, String, bool)>>,
    events: StdMutex<Vec<Event>>,
    sent: StdMutex<Vec<(String, String)>>,
    statuses: StdMutex<Vec<Option<String>>>,
    readings: StdMutex<Vec<Reading>>,
}

#[async_trait]
impl runner::Node for Fake {
    async fn fleet(&self) -> Result<Value, RpcError> {
        Ok(self.fleet.lock().unwrap().clone())
    }
    async fn access(&self, who: &str, to: &str, _why: &str, peek: bool) -> Result<AccessDecision, RpcError> {
        self.asked.lock().unwrap().push((who.into(), to.into(), peek));
        let mut d = self.decisions.lock().unwrap();
        Ok(if d.len() > 1 { d.remove(0) } else { d[0].clone() })
    }
    async fn publish(&self, event: Event) -> Result<(), RpcError> {
        self.events.lock().unwrap().push(event);
        Ok(())
    }
    async fn send(&self, to: &str, text: &str) -> Result<(), RpcError> {
        self.sent.lock().unwrap().push((to.into(), text.into()));
        Ok(())
    }
    fn readings(&self, readings: Vec<Reading>) {
        self.readings.lock().unwrap().extend(readings);
    }
    fn status(&self, problem: Option<String>, _detail: Value) {
        self.statuses.lock().unwrap().push(problem);
    }
}

fn decision(allowed: bool, reason: &str) -> AccessDecision {
    AccessDecision { allowed, rules: vec![], reason: reason.into() }
}

async fn authority(dir: &std::path::Path, fake: Arc<Fake>) -> Arc<Authority> {
    std::fs::write(dir.join("leases.yaml"), SETTINGS).unwrap();
    let paths = Paths { settings: dir.join("leases.yaml"), state_dir: dir.join("state") };
    let a = Authority::open(paths.clone(), "plugin:leases@server".into(), fake).unwrap();
    std::fs::create_dir_all(&paths.state_dir).unwrap();
    let listener = tokio::net::UnixListener::bind(paths.socket()).unwrap();
    tokio::spawn(a.clone().serve(listener));
    a.round().await;
    a
}

async fn eventually(mut f: impl FnMut() -> bool) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out");
}

#[tokio::test]
async fn callers_wait_in_line_through_the_socket() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    *fake.fleet.lock().unwrap() = json!({"nodes": [{"name": "laptop", "online": true}], "sessions": [
        {"node": "laptop", "ended": null, "meta": {"tab": "a", "status": "working"}},
        {"node": "laptop", "ended": null, "meta": {"tab": "b", "status": "working"}},
        {"node": "laptop", "ended": null, "meta": {"tab": "c", "status": "working"}},
    ]});
    let a = authority(dir.path(), fake.clone()).await;
    let sock = a.paths.socket();
    let args = json!({"kind": "build-server", "reason": "cargo test for leases"});
    let one = crate::serve::ask(&sock, "acquire", Some("a@laptop"), args.clone(), |_| {}).await.unwrap();
    assert_eq!(one["lease"]["phase"], "held");
    crate::serve::ask(&sock, "acquire", Some("b@laptop"), args.clone(), |_| {}).await.unwrap();
    let busy = crate::serve::ask(&sock, "acquire", Some("c@laptop"), args.clone(), |_| {}).await.unwrap_err();
    assert!(busy.to_string().contains("busy") && busy.to_string().contains("a@laptop"), "{busy}");
    // c waits; it hears where it is, and gets the slot when a releases.
    let (sock2, mut wait_args) = (sock.clone(), args.clone());
    wait_args["wait"] = json!(true);
    let progress = Arc::new(StdMutex::new(vec![]));
    let p2 = progress.clone();
    let waiter = tokio::spawn(async move {
        crate::serve::ask(&sock2, "acquire", Some("c@laptop"), wait_args, |p| p2.lock().unwrap().push(p.to_string()))
            .await
    });
    eventually(|| !progress.lock().unwrap().is_empty()).await;
    assert!(progress.lock().unwrap()[0].contains("number 1 in line"), "{:?}", progress.lock().unwrap());
    let first = one["lease"]["id"].as_u64().unwrap();
    // Another node can't release it; its own node can.
    let e = crate::serve::ask(&sock, "release", Some("x@server"), json!({"id": first}), |_| {}).await.unwrap_err();
    assert!(e.to_string().contains("only its own node"));
    crate::serve::ask(&sock, "release", Some("a@laptop"), json!({"id": first}), |_| {}).await.unwrap();
    let got = waiter.await.unwrap().unwrap();
    assert_eq!(got["lease"]["holder"], "c@laptop");
    assert_eq!(got["lease"]["phase"], "held");
    // No events for a kind without a channel; no message to a caller that waited on the call.
    assert!(fake.events.lock().unwrap().is_empty());
    assert!(fake.sent.lock().unwrap().is_empty(), "{:?}", fake.sent.lock().unwrap());
    let list = crate::serve::ask(&sock, "list", None, json!({}), |_| {}).await.unwrap();
    let text = crate::list_text(&list);
    assert!(text.contains("build-server: 2 of 2 slots held"), "{text}");
    assert!(text.contains("c@laptop"), "{text}");
    let r = fake.readings.lock().unwrap().iter().rev().find(|r| r.key == "build-server").cloned().unwrap();
    assert_eq!((r.kind.as_str(), r.data["held"].as_u64()), ("lease.kind", Some(2)));
    // The state survives a restart of the authority.
    let saved: State = serde_json::from_slice(&std::fs::read(a.paths.state_file()).unwrap()).unwrap();
    assert_eq!(saved.leases.len(), 2);
}

#[tokio::test]
async fn a_waiting_caller_that_hangs_up_leaves_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    *fake.fleet.lock().unwrap() = json!({"nodes": [], "sessions": []});
    let a = authority(dir.path(), fake.clone()).await;
    let sock = a.paths.socket();
    let args = json!({"kind": "build-server", "reason": "cargo test for leases"});
    crate::serve::ask(&sock, "acquire", Some("a@laptop"), args.clone(), |_| {}).await.unwrap();
    crate::serve::ask(&sock, "acquire", Some("b@laptop"), args.clone(), |_| {}).await.unwrap();
    let mut w = args.clone();
    w["wait"] = json!(true);
    let sock2 = sock.clone();
    let waiter = tokio::spawn(async move { crate::serve::ask(&sock2, "acquire", Some("c@laptop"), w, |_| {}).await });
    eventually(|| a.engine.try_lock().is_ok_and(|e| e.state.leases.len() == 3)).await;
    waiter.abort();
    let _ = waiter.await;
    eventually(|| a.engine.try_lock().is_ok_and(|e| e.state.leases.len() == 2)).await;
}

#[tokio::test]
async fn an_approved_claim_starts_and_ends_with_events() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    *fake.fleet.lock().unwrap() = json!({"nodes": [{"name": "laptop", "online": true}], "sessions": [
        {"node": "laptop", "ended": null, "meta": {"tab": "a", "status": "working"}},
    ]});
    *fake.decisions.lock().unwrap() = vec![
        decision(
            false,
            "plugin:leases@server needs its person's approval to write ...: waiting for the person to approve request r1. Try again",
        ),
        decision(false, "waiting for the person to approve request r1"),
        decision(true, "allowed"),
    ];
    let a = authority(dir.path(), fake.clone()).await;
    let sock = a.paths.socket();
    let args = json!({"kind": "gmail-filter", "reason": "watching CI myself while I fix it",
        "payload": {"account": "work", "query": "from:ci@example.com"}});
    let r = crate::serve::ask(&sock, "acquire", Some("a@laptop"), args, |_| {}).await.unwrap();
    assert_eq!(r["lease"]["phase"], "asking");
    let id = r["lease"]["id"].as_u64().unwrap();
    let key = r["lease"]["payloadKey"].as_str().unwrap().to_string();
    eventually(|| !fake.asked.lock().unwrap().is_empty()).await;
    eventually(|| a.engine.try_lock().is_ok_and(|e| e.get(id).and_then(|l| l.approval.clone()).is_some())).await;
    // Each round looks again (an ask runs in the background, one at a time per lease): still waiting, then approved.
    for _ in 0..100 {
        if a.engine.lock().await.get(id).is_some_and(|l| l.phase == Phase::Held) {
            break;
        }
        a.round().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(a.engine.lock().await.get(id).unwrap().phase, Phase::Held);
    {
        let asked = fake.asked.lock().unwrap();
        assert_eq!(asked[0], ("plugin:leases@server".to_string(), format!("service:leases/gmail-filter/{key}"), false));
        assert!(asked[1].2 && asked[2].2, "later asks only look: {asked:?}");
        assert!(!asked.last().unwrap().2, "the approved use is recorded");
    }
    eventually(|| fake.events.lock().unwrap().len() == 1).await;
    let ev = fake.events.lock().unwrap()[0].clone();
    assert_eq!((ev.channel.as_str(), ev.kind.as_str()), ("leases", "lease.started"));
    assert_eq!(ev.body["lease"]["payload"]["query"], "from:ci@example.com");
    assert_eq!(ev.body["firstOfKey"], true);
    // Nobody waited on the call, so the session hears it by message.
    eventually(|| !fake.sent.lock().unwrap().is_empty()).await;
    assert!(fake.sent.lock().unwrap()[0].1.contains("has started"));
    crate::serve::ask(&sock, "release", Some("a@laptop"), json!({"id": id}), |_| {}).await.unwrap();
    eventually(|| fake.events.lock().unwrap().len() == 2).await;
    let ev = fake.events.lock().unwrap()[1].clone();
    assert_eq!(ev.kind, "lease.ended");
    assert_eq!(ev.body["lastOfKey"], true);
    assert_eq!(ev.body["lease"]["why"], "released");
}

#[tokio::test]
async fn a_hold_keeps_a_process_lease_until_it_hangs_up() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    *fake.fleet.lock().unwrap() = json!({"nodes": [], "sessions": []});
    let a = authority(dir.path(), fake.clone()).await;
    let sock = a.paths.socket();
    let args = json!({"kind": "build-server", "reason": "release build of the images", "tie": "session", "pid": 4242});
    let r = crate::serve::ask(&sock, "acquire", Some("-@laptop"), args, |_| {}).await.unwrap();
    assert_eq!(r["lease"]["tie"], json!({"kind": "process", "pid": 4242}));
    let id = r["lease"]["id"].as_u64().unwrap();
    let sock2 = sock.clone();
    let hold =
        tokio::spawn(
            async move { crate::serve::ask(&sock2, "hold", Some("-@laptop"), json!({"id": id}), |_| {}).await },
        );
    eventually(|| a.engine.try_lock().is_ok_and(|e| e.get(id).is_some_and(|l| l.attached == 1))).await;
    // Released elsewhere: the hold answers with how it ended.
    crate::serve::ask(&sock, "release", None, json!({"id": id}), |_| {}).await.unwrap();
    let out = hold.await.unwrap().unwrap();
    assert_eq!(out["ended"]["why"], "released");
}

#[tokio::test]
async fn a_lease_past_its_ttl_is_a_problem() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::default());
    *fake.fleet.lock().unwrap() = json!({"nodes": [{"name": "laptop", "online": true}], "sessions": [
        {"node": "laptop", "ended": null, "meta": {"tab": "a", "status": "working"}}]});
    let a = authority(dir.path(), fake.clone()).await;
    let sock = a.paths.socket();
    let args = json!({"kind": "build-server", "reason": "cargo test for leases"});
    let r = crate::serve::ask(&sock, "acquire", Some("a@laptop"), args, |_| {}).await.unwrap();
    let id = r["lease"]["id"].as_u64().unwrap();
    a.engine.lock().await.state.leases.get_mut(&id).unwrap().expires_at = Some(Timestamp::now() - mins(1));
    a.round().await;
    let last = fake.statuses.lock().unwrap().last().cloned().flatten().unwrap();
    assert!(last.contains("ran into its TTL"), "{last}");
    eventually(|| fake.sent.lock().unwrap().iter().any(|(_, t)| t.contains("expired at its TTL"))).await;
}
