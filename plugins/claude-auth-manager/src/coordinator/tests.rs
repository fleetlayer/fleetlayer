//! The monitor's behaviour, ported from claude-account-rotator's `coordinator/core.spec.ts`: everything beneath it in
//! memory, on a clock the test moves.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::FutureExt;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::core::*;
use super::model::*;

const HOUR: Ms = 3_600_000;
const SLOT2: &str = "/Users/tester/.claude/accounts/2";
const SLOT3: &str = "/Users/tester/.claude/accounts/3";

fn now0() -> Ms {
    parse_iso("2026-11-09T10:00:00Z").expect("a time")
}

fn blob(token: &str, expires_at: Ms) -> Stored {
    Stored::Present(Some(Credential {
        access_token: token.into(),
        refresh_token: Some(format!("refresh-{token}")),
        expires_at,
        refresh_token_expires_at: Some(now0() + 20 * 24 * HOUR),
        plan: Some("max".into()),
    }))
}

fn live(token: &str) -> Stored {
    blob(token, now0() + 6 * HOUR)
}

struct SignInCall {
    slot: String,
    finish: Option<oneshot::Sender<(Option<i32>, String)>>,
    cancelled: Arc<Mutex<bool>>,
}

#[derive(Default)]
struct W {
    now: Ms,
    timers: Vec<(Ms, u64, oneshot::Sender<()>)>,
    next_timer: u64,
    stored: HashMap<String, Stored>,
    /// token -> (5-hour %, or None as since November 2026; week %)
    usage: HashMap<String, (Option<f64>, f64)>,
    owners: HashMap<String, String>,
    tiers: HashMap<String, String>,
    status: HashMap<String, Identity>,
    usage_calls: Vec<String>,
    fable: HashMap<String, f64>,
    failing: HashMap<String, (u16, Option<Ms>)>,
    profile_calls: Vec<String>,
    sign_ins: Vec<SignInCall>,
    sign_outs: Vec<String>,
    alive: HashSet<i64>,
    saved: Option<Persisted>,
    logs: Vec<String>,
    refreshes: Vec<String>,
    refresh_to: HashMap<String, String>,
}

/// Everything beneath the monitor, shared by the test and the monitor.
#[derive(Clone)]
struct World(Arc<Mutex<W>>);

impl World {
    fn new() -> World {
        World(Arc::new(Mutex::new(W { now: now0(), next_timer: 1, ..Default::default() })))
    }

    fn w(&self) -> std::sync::MutexGuard<'_, W> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now(&self) -> Ms {
        self.w().now
    }

    /// Moves the clock on, waking each sleeper as it comes due and letting its work settle.
    async fn advance(&self, ms: Ms) {
        let until = self.now() + ms;
        loop {
            let due = {
                let mut w = self.w();
                w.timers.retain(|t| !t.2.is_closed());
                let Some(i) = (0..w.timers.len())
                    .filter(|&i| w.timers[i].0 <= until)
                    .min_by_key(|&i| (w.timers[i].0, w.timers[i].1))
                else {
                    break;
                };
                let t = w.timers.remove(i);
                w.now = t.0;
                t.2
            };
            let _ = due.send(());
            settle().await;
        }
        self.w().now = until;
        settle().await;
    }

    fn finish_sign_in(&self, i: usize, code: Option<i32>, errors: &str) {
        let tx = self.w().sign_ins[i].finish.take();
        if let Some(tx) = tx {
            let _ = tx.send((code, errors.into()));
        }
    }
}

async fn settle() {
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
}

#[async_trait]
impl Deps for World {
    fn now(&self) -> Ms {
        self.w().now
    }

    async fn sleep(&self, ms: Ms) {
        let rx = {
            let mut w = self.w();
            let (tx, rx) = oneshot::channel();
            let (at, id) = (w.now + ms, w.next_timer);
            w.next_timer += 1;
            w.timers.push((at, id, tx));
            rx
        };
        let _ = rx.await;
    }

    async fn read_stored(&self, slot: &str) -> Stored {
        self.w().stored.get(slot).cloned().unwrap_or(Stored::Absent)
    }

    async fn fetch_usage(&self, token: &str) -> UsageAnswer {
        let mut w = self.w();
        w.usage_calls.push(token.into());
        if let Some((status, retry_after_ms)) = w.failing.get(token) {
            return UsageAnswer::Failed { status: *status, retry_after_ms: *retry_after_ms };
        }
        let Some((five, week)) = w.usage.get(token).copied() else {
            return UsageAnswer::Rejected;
        };
        let mut body = json!({
            "five_hour": five.map(|p| json!({ "utilization": p, "resets_at": "2026-11-09T12:00:00Z" })),
            "seven_day": { "utilization": week, "resets_at": "2026-11-14T03:00:00Z" },
        });
        if let Some(pct) = w.fable.get(token) {
            body["limits"] = json!([{ "kind": "weekly_scoped", "percent": pct, "resets_at": "2026-11-14T03:00:00Z", "scope": { "model": { "display_name": "Fable" }, "surface": null } }]);
        }
        UsageAnswer::Ok(body)
    }

    async fn fetch_profile(&self, token: &str) -> Option<Value> {
        let mut w = self.w();
        w.profile_calls.push(token.into());
        let email = w.owners.get(token)?.clone();
        let tier = w.tiers.get(token).cloned().unwrap_or("default_claude_max_20x".into());
        Some(json!({
            "account": { "uuid": format!("acct-{email}"), "email": email },
            "organization": { "uuid": format!("org-{email}"), "name": "Org", "rate_limit_tier": tier },
        }))
    }

    async fn auth_status(&self, slot: &str) -> Option<Identity> {
        self.w().status.get(slot).cloned()
    }

    async fn refresh_token(&self, slot: &str) -> bool {
        let mut w = self.w();
        w.refreshes.push(slot.into());
        let Some(token) = w.refresh_to.get(slot).cloned() else {
            return false;
        };
        let at = w.now + 8 * HOUR;
        w.stored.insert(slot.into(), blob(&token, at));
        true
    }

    fn sign_in(&self, slot: &str, _email: Option<&str>) -> SignInRun {
        let (tx, rx) = oneshot::channel::<(Option<i32>, String)>();
        let cancelled = Arc::new(Mutex::new(false));
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        let cancel_tx = Arc::new(Mutex::new(Some(cancel_tx)));
        self.w().sign_ins.push(SignInCall { slot: slot.into(), finish: Some(tx), cancelled: cancelled.clone() });
        let done = async move {
            tokio::select! {
                r = rx => r.unwrap_or((None, String::new())),
                _ = cancel_rx => (None, "cancelled".into()),
            }
        }
        .boxed();
        SignInRun {
            done,
            cancel: Arc::new(move || {
                *cancelled.lock().unwrap_or_else(|e| e.into_inner()) = true;
                if let Some(tx) = cancel_tx.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    let _ = tx.send(());
                }
            }),
        }
    }

    async fn sign_out(&self, slot: &str) {
        self.w().sign_outs.push(slot.into());
    }

    async fn make_slot(&self, id: &str) -> Result<String, String> {
        Ok(format!("/Users/tester/.claude/accounts/{id}"))
    }

    fn is_alive(&self, pid: i64) -> bool {
        self.w().alive.contains(&pid)
    }

    fn save(&self, persisted: &Persisted) {
        self.w().saved = Some(persisted.clone());
    }

    fn log(&self, text: &str) {
        self.w().logs.push(text.into());
    }
}

/// Three Ultra logins, signed in, at 20%, 40% and 60% of their 5-hour windows.
fn three_logins(w: &World) -> Value {
    {
        let mut x = w.w();
        x.stored.insert(String::new(), live("t1"));
        x.stored.insert(SLOT2.into(), live("t2"));
        x.stored.insert(SLOT3.into(), live("t3"));
        x.usage = HashMap::from([
            ("t1".into(), (Some(20.0), 30.0)),
            ("t2".into(), (Some(40.0), 50.0)),
            ("t3".into(), (Some(60.0), 70.0)),
        ]);
        x.owners = HashMap::from([
            ("t1".into(), "one@example.com".into()),
            ("t2".into(), "two@example.com".into()),
            ("t3".into(), "three@example.com".into()),
        ]);
    }
    let org = |id: &str, slot: &str, email: &str| json!({ "id": id, "slot": slot, "email": email, "orgId": format!("org-{email}"), "orgName": "Org" });
    json!({
        "accounts": [org("1", "", "one@example.com"), org("2", SLOT2, "two@example.com"), org("3", SLOT3, "three@example.com")],
        "readings": {}, "health": {}, "holders": {},
        "settings": { "rotateTo": "reset", "footer": { "session": "braille", "week": "bars", "sessionPercent": true, "weekPercent": true, "active": "lines", "fill": "left" } },
        "chrome": { "accountIds": [] },
    })
}

async fn started(w: &World, persisted: Option<&Value>) -> Coordinator<World> {
    let c = Coordinator::new(
        w.clone(),
        Info { version: "0.4.0".into(), pid: 1, started_at: now0(), is_managed: false },
        persisted,
    );
    c.start().await;
    settle().await;
    c
}

fn account(c: &Coordinator<World>, id: &str) -> AccountState {
    c.state().accounts.into_iter().find(|a| a.account.id == id).unwrap_or_else(|| panic!("account {id}"))
}

fn five(c: &Coordinator<World>, id: &str) -> Option<f64> {
    account(c, id).reading.and_then(|r| r.five_hour).map(|w| w.pct)
}

fn week(c: &Coordinator<World>, id: &str) -> Option<f64> {
    account(c, id).reading.and_then(|r| r.seven_day).map(|w| w.pct)
}

fn reading(five: Option<f64>, week: Option<f64>, at: Ms) -> Reading {
    let win = |pct: f64, resets: &str| UsageWindow { pct, resets_at: Some(resets.into()) };
    Reading {
        five_hour: five.map(|p| win(p, "2026-11-09T12:00:00Z")),
        seven_day: week.map(|p| win(p, "2026-11-14T03:00:00Z")),
        models: None,
        at,
    }
}

fn req(method: &str, path: &str, query: &[(&str, &str)], body: Option<Value>) -> Request {
    Request {
        method: method.into(),
        path: path.into(),
        query: query.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        body,
    }
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

// ------------------------------------------------------------ polling

#[tokio::test]
async fn reads_each_login_once_at_start_and_polls_once_per_login_however_many_sessions_run() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    assert_eq!(sorted(w.w().usage_calls.clone()), ["t1", "t2", "t3"]);
    for i in 0..30 {
        c.register(&format!("s{i}"), None, if i % 2 == 0 { "" } else { SLOT2 }, None, true);
    }
    w.w().usage_calls.clear();
    w.advance(POLL_MS + TICK_MS).await;
    assert_eq!(sorted(w.w().usage_calls.clone()), ["t1", "t2", "t3"], "one poll per login in a poll period");
    assert_eq!(account(&c, "1").session_count, 15);
    assert_eq!(account(&c, "2").session_count, 15);
}

#[tokio::test]
async fn a_sessions_readings_keep_its_login_fresh_so_it_isnt_polled() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    w.w().usage_calls.clear();
    for minute in 1..=12 {
        w.advance(60_000).await;
        c.report("1", reading(Some(20.0 + minute as f64), None, w.now())).expect("reported");
    }
    let calls = w.w().usage_calls.clone();
    assert!(!calls.contains(&"t1".to_string()), "login 1 was never polled");
    assert!(calls.contains(&"t2".to_string()), "idle logins still are");
    assert_eq!(five(&c, "1"), Some(32.0));
}

#[tokio::test]
async fn a_reading_that_shows_the_same_moves_its_time_on_without_waking_long_polls() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    let before = c.state().version;
    c.report("1", reading(Some(20.2), Some(30.0), w.now() + 1)).expect("reported");
    assert_eq!(c.state().version, before);
    c.report("1", reading(Some(25.0), Some(30.0), w.now() + 2)).expect("reported");
    assert_eq!(c.state().version, before + 1);
}

#[tokio::test]
async fn registering_again_unchanged_wakes_no_long_polls() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    c.register("s1", Some(7), SLOT2, Some("/code"), true);
    let before = c.state().version;
    w.advance(1000).await;
    c.register("s1", Some(7), SLOT2, Some("/code"), true);
    assert_eq!(c.state().version, before);
    c.register("s1", Some(7), SLOT2, Some("/elsewhere"), true);
    assert_eq!(c.state().version, before + 1);
}

#[tokio::test]
async fn an_older_reading_never_replaces_a_newer_one() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    c.report(
        "1",
        Reading {
            five_hour: Some(UsageWindow { pct: 50.0, resets_at: None }),
            seven_day: None,
            models: None,
            at: w.now() - 10_000,
        },
    )
    .expect("reported");
    assert_eq!(five(&c, "1"), Some(20.0));
}

#[tokio::test]
async fn a_keychain_that_cannot_be_read_is_said_once_and_leaves_the_health_as_it_was() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    w.w().stored.insert(SLOT2.into(), Stored::Unreadable(51));
    w.advance(TICK_MS).await;
    w.advance(TICK_MS).await;
    assert_eq!(c.state().notices.iter().filter(|n| n.text.contains("keychain")).count(), 1);
    assert_eq!(account(&c, "2").health.kind(), "ok");
}

// ------------------------------------------------------------ expired sign-ins

#[tokio::test]
async fn an_idle_login_whose_token_expired_has_claude_code_refresh_it() {
    let w = World::new();
    let persisted = three_logins(&w);
    w.w().stored.insert(SLOT3.into(), blob("t3-old", now0() - HOUR));
    w.w().refresh_to.insert(SLOT3.into(), "t3".into());
    let c = started(&w, Some(&persisted)).await;
    assert_eq!(w.w().refreshes, [SLOT3]);
    let calls = w.w().usage_calls.clone();
    assert!(calls.contains(&"t3".into()), "read with the refreshed token");
    assert!(!calls.contains(&"t3-old".into()));
    assert_eq!(week(&c, "3"), Some(70.0));
    assert_eq!(account(&c, "3").health.kind(), "ok");
    // The refreshed token lasts: no refresh at the next polls.
    w.advance(2 * POLL_MS + TICK_MS).await;
    assert_eq!(w.w().refreshes, [SLOT3]);
}

#[tokio::test]
async fn a_login_a_session_is_on_is_left_for_that_session_to_refresh() {
    let w = World::new();
    let persisted = three_logins(&w);
    w.w().stored.insert(SLOT3.into(), blob("t3-old", now0() - HOUR));
    w.w().refresh_to.insert(SLOT3.into(), "t3".into());
    let c = started(&w, Some(&persisted)).await;
    {
        let mut x = w.w();
        x.refreshes.clear();
        let at = x.now - HOUR;
        x.stored.insert(SLOT2.into(), blob("t2-old", at));
        x.refresh_to.insert(SLOT2.into(), "t2".into());
    }
    c.register("s1", None, SLOT2, None, true);
    w.advance(POLL_MS + TICK_MS).await;
    assert!(w.w().refreshes.is_empty());
    assert!(!w.w().usage_calls.contains(&"t2-old".into()), "an expired token is never sent");
}

#[tokio::test]
async fn a_refresh_that_fails_is_logged_and_tried_again_only_after_a_while() {
    let w = World::new();
    let persisted = three_logins(&w);
    w.w().stored.insert(SLOT3.into(), blob("t3-old", now0() - HOUR));
    let c = started(&w, Some(&persisted)).await;
    assert_eq!(w.w().refreshes, [SLOT3]);
    assert!(w.w().logs.iter().any(|l| l.contains("account 3: Claude Code could not refresh")));
    w.advance(POLL_MS + TICK_MS).await;
    assert_eq!(w.w().refreshes, [SLOT3], "not at the next poll");
    w.w().refresh_to.insert(SLOT3.into(), "t3".into());
    w.advance(REFRESH_RETRY_MS).await;
    assert_eq!(w.w().refreshes, [SLOT3, SLOT3]);
    assert_eq!(week(&c, "3"), Some(70.0));
}

// ------------------------------------------------------------ limits

#[tokio::test]
async fn a_sessions_reading_replaces_the_logins_and_the_state_holds_no_move_decision() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    c.report("1", reading(Some(95.0), None, w.now() + 1)).expect("reported");
    assert_eq!(five(&c, "1"), Some(95.0));
    assert!(is_spent(account(&c, "1").reading.as_ref(), w.now(), None));
    let body = serde_json::to_value(account(&c, "1")).expect("json");
    for key in ["moveTo", "moveToFor", "isSpent"] {
        assert!(body.get(key).is_none(), "{key}");
    }
}

#[tokio::test]
async fn a_turn_that_hits_the_limit_first_counts_the_5_hour_window_full_with_one_call_for_many_sessions() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    w.w().usage_calls.clear();
    w.advance(FRESH_MS).await;
    let (a, b, d) = tokio::join!(c.spent("1", None), c.spent("1", None), c.spent("1", None));
    assert!(a.is_ok() && b.is_ok() && d.is_ok());
    assert_eq!(w.w().usage_calls, ["t1"]);
    assert_eq!(five(&c, "1"), Some(100.0));
}

#[tokio::test]
async fn with_only_a_weekly_limit_a_turn_that_hits_it_counts_the_week_full() {
    let w = World::new();
    let persisted = three_logins(&w);
    for v in w.w().usage.values_mut() {
        v.0 = None;
    }
    let c = started(&w, Some(&persisted)).await;
    assert_eq!(five(&c, "1"), None);
    w.advance(FRESH_MS).await;
    c.spent("1", None).await.expect("spent");
    assert_eq!(week(&c, "1"), Some(100.0));
    assert!(is_spent(account(&c, "1").reading.as_ref(), w.now(), None));
    assert!(c.state().combined.five_hour.is_none());
}

#[tokio::test]
async fn a_rejected_sign_in_is_announced_once_and_the_first_session_to_claim_the_notice_notifies() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    c.rejected("2").expect("known");
    c.rejected("2").expect("known");
    let notices: Vec<Notice> = c.state().notices.into_iter().filter(|n| n.notify).collect();
    assert_eq!(notices.len(), 1);
    assert!(c.claim(notices[0].id, "a"));
    assert!(!c.claim(notices[0].id, "b"));
    assert_eq!(account(&c, "2").health.kind(), "login-needed");
}

// ------------------------------------------------------------ model weeks

#[tokio::test]
async fn a_logins_fable_week_is_read_from_the_usage_endpoint_and_a_responses_reading_keeps_it() {
    let w = World::new();
    let persisted = three_logins(&w);
    {
        let mut x = w.w();
        x.usage =
            HashMap::from([("t1".into(), (None, 30.0)), ("t2".into(), (None, 50.0)), ("t3".into(), (None, 70.0))]);
        x.fable = HashMap::from([("t1".into(), 99.0), ("t2".into(), 10.0), ("t3".into(), 40.0)]);
    }
    let c = started(&w, Some(&persisted)).await;
    let fable =
        |c: &Coordinator<World>| account(c, "1").reading.and_then(|r| r.models).and_then(|m| m.get("Fable").cloned());
    assert_eq!(fable(&c), Some(UsageWindow { pct: 99.0, resets_at: Some("2026-11-14T03:00:00Z".into()) }));
    c.report("1", reading(None, Some(31.0), w.now() + 1)).expect("reported");
    assert_eq!(week(&c, "1"), Some(31.0));
    assert_eq!(fable(&c).map(|w| w.pct), Some(99.0));
}

// ------------------------------------------------------------ usage calls

#[tokio::test]
async fn after_a_failed_call_the_next_waits_two_minutes_then_twice_as_long_and_a_longer_retry_after() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    w.w().usage_calls.clear();
    w.w().failing.insert("t1".into(), (429, None));
    let count = |t: &str| w.w().usage_calls.iter().filter(|x| *x == t).count();
    w.advance(FRESH_MS).await;
    c.check("1", Usage::Fresh).await;
    assert_eq!(count("t1"), 1);
    // Sessions asking for fresh usage every minute don't get a call each: the first waits two minutes.
    w.advance(FRESH_MS).await;
    c.check("1", Usage::Fresh).await;
    assert_eq!(count("t1"), 1);
    w.advance(BACKOFF_MS - FRESH_MS).await;
    c.check("1", Usage::Fresh).await;
    assert_eq!(count("t1"), 2);
    // The second failure waits four minutes.
    w.advance(BACKOFF_MS).await;
    c.check("1", Usage::Fresh).await;
    assert_eq!(count("t1"), 2);
    w.advance(BACKOFF_MS).await;
    w.w().failing.remove("t1");
    c.check("1", Usage::Fresh).await;
    assert_eq!(count("t1"), 3);
    // A call that answers clears the wait.
    w.advance(FRESH_MS).await;
    c.check("1", Usage::Fresh).await;
    assert_eq!(count("t1"), 4);
    // A Retry-After of ten minutes is kept, past the minute, its polls included.
    w.w().usage_calls.clear();
    w.w().failing.insert("t2".into(), (429, Some(10 * 60_000)));
    c.check("2", Usage::Fresh).await;
    w.advance(9 * 60_000).await;
    c.check("2", Usage::Fresh).await;
    assert_eq!(count("t2"), 1);
    w.advance(60_000).await;
    c.check("2", Usage::Fresh).await;
    assert_eq!(count("t2"), 2);
}

#[test]
fn a_retry_after_header_reads_as_seconds_or_as_a_date() {
    use super::deps::retry_after;
    let now = parse_iso("2026-11-10T00:00:00Z").expect("a time");
    assert_eq!(retry_after(Some("120"), now), Some(120_000));
    assert_eq!(retry_after(Some("Tue, 10 Nov 2026 00:05:00 GMT"), now), Some(300_000));
    assert_eq!(retry_after(None, now), None);
    assert_eq!(retry_after(Some("soon"), now), None);
}

// ------------------------------------------------------------ combined usage

#[tokio::test]
async fn combined_usage_weighs_each_login_by_its_plan_and_leaves_out_logins_that_need_a_sign_in() {
    let w = World::new();
    let persisted = three_logins(&w);
    w.w().tiers.insert("t3".into(), "default_claude_max_5x".into());
    let c = started(&w, Some(&persisted)).await;
    assert_eq!(account(&c, "1").weight, 10);
    assert_eq!(account(&c, "3").weight, 5);
    let round = |x: f64| (x * 10.0).round() / 10.0;
    assert_eq!(
        c.state().combined.five_hour.map(|w| w.pct),
        Some(round((10.0 * 20.0 + 10.0 * 40.0 + 5.0 * 60.0) / 25.0))
    );
    c.rejected("2").expect("known");
    let combined = c.state().combined.five_hour.expect("a window");
    assert_eq!(combined.pct, round((10.0 * 20.0 + 5.0 * 60.0) / 15.0));
    assert_eq!(combined.accounts, 2);
    assert_eq!(combined.next_reset_at.as_deref(), Some("2026-11-09T12:00:00.000Z"));
}

#[tokio::test]
async fn a_window_past_its_reset_counts_as_empty() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    w.advance(2 * HOUR + 60_000).await;
    assert_eq!(c.state().combined.five_hour.map(|w| w.pct), Some(0.0));
}

// ------------------------------------------------------------ long polls

#[tokio::test]
async fn long_polls_answer_at_once_when_the_state_moved_on_the_next_change_or_after_the_wait() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    let v = c.state().version as f64;
    let is_done = Arc::new(Mutex::new(false));
    let wait = |since: f64, ms: f64| {
        *is_done.lock().expect("lock") = false;
        let (c, flag) = (c.clone(), is_done.clone());
        tokio::spawn(async move {
            c.wait_for_change(since, ms, None).await;
            *flag.lock().expect("lock") = true;
        });
    };
    let done = || *is_done.lock().expect("lock");
    wait(v - 1.0, 50_000.0);
    settle().await;
    assert!(done(), "already moved");

    wait(v, 50_000.0);
    settle().await;
    assert!(!done());
    c.patch_settings(json!({ "rotateTo": "room" }).as_object().expect("object")).expect("patched");
    settle().await;
    assert!(done(), "woken by a change");

    wait(c.state().version as f64, 120_000.0);
    settle().await;
    w.advance(MAX_WAIT_MS - 1).await;
    assert!(!done());
    w.advance(1).await;
    assert!(done(), "capped at the longest wait");
}

#[tokio::test]
async fn the_version_starts_above_any_earlier_run() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    // A run's changes can't outpace the clock, so the next run, started later, begins higher.
    assert!(c.state().version >= u64::try_from(now0()).unwrap(), "{}", c.state().version);
}

#[tokio::test]
async fn a_session_that_repeats_a_stale_since_waits_instead_of_spinning() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    let stale = c.state().version as f64 + 500.0; // seen from an earlier run, say
    let is_done = Arc::new(Mutex::new(false));
    let wait = |session: &'static str| {
        *is_done.lock().expect("lock") = false;
        let (c, flag) = (c.clone(), is_done.clone());
        tokio::spawn(async move {
            c.wait_for_change(stale, 50_000.0, Some(session)).await;
            *flag.lock().expect("lock") = true;
        });
    };
    let done = || *is_done.lock().expect("lock");
    wait("s1");
    settle().await;
    assert!(done(), "the first stale poll answers at once");
    wait("s2");
    settle().await;
    assert!(done(), "per session");
    wait("s1");
    settle().await;
    assert!(!done(), "the same stale since again waits");
    c.patch_settings(json!({ "rotateTo": "room" }).as_object().expect("object")).expect("patched");
    settle().await;
    assert!(done(), "and still wakes on a change");
}

#[tokio::test]
async fn get_state_through_the_api_waits_the_same_way() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    let v = c.state().version.to_string();
    let answer = Arc::new(Mutex::new(None));
    {
        let (c, answer) = (c.clone(), answer.clone());
        tokio::spawn(async move {
            let r = c.handle(req("GET", "/state", &[("since", &v), ("wait", "5000")], None)).await;
            *answer.lock().expect("lock") = Some(r.status);
        });
    }
    settle().await;
    w.advance(4_000).await;
    assert_eq!(*answer.lock().expect("lock"), None);
    w.advance(1_000).await;
    assert_eq!(*answer.lock().expect("lock"), Some(200));
}

// ------------------------------------------------------------ identity

#[tokio::test]
async fn a_new_sign_in_in_a_slot_is_asked_about_once_and_names_the_login() {
    let w = World::new();
    let mut persisted = three_logins(&w);
    persisted["accounts"][1] = json!({ "id": "2", "slot": SLOT2 });
    let c = started(&w, Some(&persisted)).await;
    let two = account(&c, "2").account;
    assert_eq!(two.email.as_deref(), Some("two@example.com"));
    assert_eq!(two.tier.as_deref(), Some("default_claude_max_20x"));
    assert_eq!(two.account_uuid.as_deref(), Some("acct-two@example.com"));
    w.w().profile_calls.clear();
    w.advance(TICK_MS).await;
    assert!(w.w().profile_calls.is_empty(), "the same token is not asked about again");
}

#[tokio::test]
async fn a_slot_that_holds_another_logins_sign_in_needs_its_own_again() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    {
        let mut x = w.w();
        x.stored.insert(SLOT3.into(), live("t2b"));
        x.owners.insert("t2b".into(), "two@example.com".into());
        x.usage.insert("t2b".into(), (Some(40.0), 50.0));
    }
    w.advance(TICK_MS).await;
    assert_eq!(account(&c, "3").health, Health::login_needed("signed in as account 2 now"));
}

#[tokio::test]
async fn a_login_that_signs_claude_codes_login_in_to_another_account_renames_it_and_says_so() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    {
        let mut x = w.w();
        x.stored.insert(String::new(), live("t9"));
        x.owners.insert("t9".into(), "nine@example.com".into());
        x.usage.insert("t9".into(), (Some(5.0), 5.0));
    }
    c.check("1", Usage::Fresh).await;
    assert_eq!(account(&c, "1").account.email.as_deref(), Some("nine@example.com"));
    assert!(
        c.state().notices.iter().any(|n| { n.text.starts_with("Claude Code's login, account 1, is nine@example.com") })
    );
}

// ------------------------------------------------------------ sign-in

#[tokio::test]
async fn signs_in_every_login_that_needs_it_one_at_a_time_and_adopts_who_signed_in() {
    let w = World::new();
    let persisted = three_logins(&w);
    w.w().stored.remove(SLOT2);
    w.w().stored.remove(SLOT3);
    let c = started(&w, Some(&persisted)).await;
    assert_eq!(account(&c, "2").health.kind(), "login-needed");
    c.sign_in(None).expect("queued");
    assert_eq!(w.w().sign_ins.len(), 1);
    assert_eq!(account(&c, "2").health.kind(), "logging-in");
    assert_eq!(c.state().sign_in.map(|s| s.account_id).as_deref(), Some("2"));
    {
        let mut x = w.w();
        x.stored.insert(SLOT2.into(), live("t2"));
        x.status.insert(
            SLOT2.into(),
            Identity {
                email: Some("two@example.com".into()),
                org_id: Some("org-two@example.com".into()),
                org_name: Some("Org".into()),
                plan: Some("max".into()),
                ..Default::default()
            },
        );
    }
    w.finish_sign_in(0, Some(0), "");
    settle().await;
    assert_eq!(account(&c, "2").health.kind(), "ok");
    assert_eq!(w.w().sign_ins.len(), 2, "then the next");
    assert_eq!(w.w().sign_ins[1].slot, SLOT3);
}

#[tokio::test]
async fn a_sign_in_as_a_login_already_on_the_list_asks_for_another_account() {
    let w = World::new();
    let persisted = three_logins(&w);
    w.w().stored.remove(SLOT3);
    let c = started(&w, Some(&persisted)).await;
    c.sign_in(Some("3")).expect("queued");
    w.w().status.insert(
        SLOT3.into(),
        Identity {
            email: Some("one@example.com".into()),
            org_id: Some("org-one@example.com".into()),
            ..Default::default()
        },
    );
    w.finish_sign_in(0, Some(0), "");
    settle().await;
    assert_eq!(account(&c, "3").health, Health::login_needed("that was account 1 (one@example.com · Org) again"));
    assert!(c.state().notices.iter().any(|n| n.text.contains("Sign out of claude.ai in your browser")));
}

#[tokio::test]
async fn a_sign_in_that_fails_says_why_and_cancel_ends_the_queue() {
    let w = World::new();
    let persisted = three_logins(&w);
    w.w().stored.remove(SLOT2);
    w.w().stored.remove(SLOT3);
    let c = started(&w, Some(&persisted)).await;
    c.sign_in(None).expect("queued");
    w.finish_sign_in(0, Some(1), "browser closed");
    settle().await;
    assert!(c.state().notices.iter().any(|n| n.text == "Sign-in for Claude account 2 did not finish: browser closed"));
    c.cancel_sign_in();
    settle().await;
    assert!(*w.w().sign_ins[1].cancelled.lock().expect("lock"));
    assert_eq!(c.state().sign_in, None);
}

#[tokio::test]
async fn a_monitor_that_died_mid_sign_in_counts_it_unfinished() {
    let w = World::new();
    let mut persisted = three_logins(&w);
    persisted["health"] = json!({ "2": { "kind": "logging-in", "since": now0() - 1000 } });
    w.w().stored.remove(SLOT2);
    let c = started(&w, Some(&persisted)).await;
    assert_eq!(account(&c, "2").health.kind(), "login-needed");
}

// ------------------------------------------------------------ sessions

#[tokio::test]
async fn a_session_on_a_slot_nobody_registered_adopts_it_as_a_new_login() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    let slot = "/Users/tester/elsewhere";
    {
        let mut x = w.w();
        x.stored.insert(slot.into(), live("t4"));
        x.owners.insert("t4".into(), "four@example.com".into());
        x.usage.insert("t4".into(), (Some(1.0), 1.0));
    }
    let id = c.register("s", None, slot, None, true);
    settle().await;
    assert_eq!(id, "4");
    assert_eq!(account(&c, "4").account.email.as_deref(), Some("four@example.com"));
}

#[tokio::test]
async fn a_session_id_arrives_percent_encoded_in_a_path_and_is_found() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    c.register("session-1:4242", None, "", None, true);
    let answer = c.handle(req("PATCH", "/sessions/session-1%3A4242", &[], Some(json!({ "accountId": "2" })))).await;
    assert_eq!(answer.status, 200);
    assert_eq!(c.state().sessions[0].account_id, "2");
    assert_eq!(c.handle(req("PATCH", "/sessions/%E0%A4%A", &[], Some(json!({})))).await.status, 400);
}

#[tokio::test]
async fn a_session_whose_process_ended_or_that_went_quiet_is_dropped() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    w.w().alive.insert(100);
    c.register("live", Some(100), "", None, true);
    c.register("dead", Some(101), "", None, true);
    c.register("quiet", None, "", None, true);
    c.register("polling", None, "", None, true);
    c.poll_opened("polling");
    let ids = |c: &Coordinator<World>| sorted(c.state().sessions.into_iter().map(|s| s.id).collect());
    w.advance(TICK_MS).await;
    assert_eq!(ids(&c), ["live", "polling", "quiet"]);
    w.advance(STALE_SESSION_MS + TICK_MS).await;
    assert_eq!(ids(&c), ["live", "polling"]);
}

// ------------------------------------------------------------ removing a login

#[tokio::test]
async fn claude_codes_own_login_cant_be_removed() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    assert_eq!(c.handle(req("POST", "/accounts/1/remove", &[], Some(json!({})))).await.status, 400);
}

#[tokio::test]
async fn a_removed_login_waits_for_its_sessions_to_move_off_then_is_signed_out_and_dropped() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    c.register("s", None, SLOT2, None, true);
    c.remove("2").await.expect("removing");
    assert!(account(&c, "2").is_retiring);
    assert!(w.w().sign_outs.is_empty());
    c.patch_session("s", Some("1"), None, None, None).expect("patched");
    settle().await;
    assert_eq!(w.w().sign_outs, [SLOT2]);
    assert!(!c.state().accounts.iter().any(|a| a.account.id == "2"));
}

#[tokio::test]
async fn the_log_names_each_move_a_session_makes_and_why_and_a_rotation_that_stops() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    c.register("s", Some(4242), "", Some("/work"), true);
    let patch = |body: Value| req("PATCH", "/sessions/s", &[], Some(body));
    assert_eq!(
        c.handle(patch(json!({ "accountId": "2", "isPending": true, "why": "account 1 has 2% of its week left" })))
            .await
            .status,
        200
    );
    assert_eq!(c.handle(patch(json!({ "isPending": false }))).await.status, 200);
    assert_eq!(
        c.handle(patch(json!({ "why": "rotation paused after switching 1 -> 2 -> 1 -> 2 within a minute" })))
            .await
            .status,
        200
    );
    let logs: Vec<String> = w.w().logs.iter().filter(|l| l.starts_with("session ")).cloned().collect();
    assert_eq!(
        logs,
        [
            "session pid 4242 in /work: account 1 -> 2 (account 1 has 2% of its week left)",
            "session pid 4242 in /work: on account 2 (rotation paused after switching 1 -> 2 -> 1 -> 2 within a minute)",
        ]
    );
}

// ------------------------------------------------------------ the API

#[tokio::test]
async fn the_api_refuses_what_it_does_not_know_and_readings_that_are_not_readings() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    assert_eq!(c.handle(req("GET", "/nope", &[], None)).await.status, 404);
    assert_eq!(
        c.handle(req("POST", "/readings", &[], Some(json!({ "accountId": "1", "reading": { "pct": 3 } }))))
            .await
            .status,
        400
    );
    assert_eq!(c.handle(req("PATCH", "/settings", &[], Some(json!({ "rotateTo": "sideways" })))).await.status, 400);
    assert_eq!(c.handle(req("PATCH", "/sessions/ghost", &[], Some(json!({ "accountId": "1" })))).await.status, 404);
}

#[tokio::test]
async fn hello_names_the_node_so_the_plugin_leaves_it_alone() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    let body = c.handle(req("GET", "/hello", &[], None)).await.body;
    assert_eq!(body["name"], "fleetlayer-claude-auth-manager");
    assert_eq!(body["version"], "0.4.0");
}

#[tokio::test]
async fn keeps_what_it_learned_across_a_restart_in_the_typescript_coordinators_format() {
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    c.patch_settings(json!({ "rotateTo": "order" }).as_object().expect("object")).expect("patched");
    c.patch_chrome(
        json!({ "accountId": "2", "seen": { "accountId": "2", "isSeen": true } }).as_object().expect("object"),
    )
    .expect("patched");
    c.stop();
    let saved = serde_json::to_value(w.w().saved.clone().expect("saved")).expect("json");
    // The TypeScript coordinator's field names, so either can read the other's file.
    assert_eq!(saved["accounts"][1]["orgId"], "org-two@example.com");
    assert_eq!(saved["readings"]["1"]["fiveHour"]["pct"], 20.0);
    assert!(saved["readings"]["1"]["at"].is_number());
    let again = started(&World::new(), Some(&saved)).await;
    assert_eq!(again.state().settings.rotate_to, "order");
    assert_eq!(again.state().chrome.account_id.as_deref(), Some("2"));
    assert_eq!(again.state().chrome.account_ids, ["2"]);
}

// ------------------------------------------------------------ what the node sends the deck

#[tokio::test]
async fn serves_the_api_on_its_socket_and_wont_be_shut_down_by_a_client() {
    let dir = tempfile::Builder::new().prefix("qd").tempdir_in("/tmp").expect("tempdir");
    let w = World::new();
    let c = started(&w, Some(&three_logins(&w))).await;
    let listener = super::server::bind(dir.path()).expect("bound");
    let mode = |p: &std::path::Path| {
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(p).expect("metadata").permissions()) & 0o777
    };
    let sock = super::server::socket_path(dir.path());
    assert_eq!(mode(&sock), 0o600);
    assert_eq!(mode(dir.path()), 0o700);
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let serving = tokio::spawn(super::server::serve(listener, c.clone(), async move {
        let _ = stop_rx.await;
    }));
    let call = |method: &'static str, path: String, body: Option<Value>| {
        let sock = sock.clone();
        async move {
            super::server::call(&sock, method, &path, body.as_ref(), std::time::Duration::from_secs(5))
                .await
                .expect("answered")
        }
    };
    let (status, hello) = call("GET", "/hello".into(), None).await;
    assert_eq!((status, hello["name"].as_str()), (200, Some("fleetlayer-claude-auth-manager")));
    let (status, body) =
        call("POST", "/sessions".into(), Some(json!({ "id": "s1", "slot": SLOT2, "isInteractive": true }))).await;
    assert_eq!((status, body["accountId"].as_str()), (200, Some("2")));
    assert_eq!(body["state"]["accounts"][1]["sessionCount"], 1);
    let (status, _) = call("POST", "/shutdown".into(), None).await;
    assert_eq!(status, 409, "the node keeps the API running");
    // A long poll counts as the session being alive while it is open, and ends when the server stops.
    let v = c.state().version;
    let poll = tokio::spawn(call("GET", format!("/state?since={v}&wait=50000&session=s1"), None));
    settle().await;
    let _ = stop_tx.send(());
    let (status, _) = poll.await.expect("joined");
    assert_eq!(status, 200);
    serving.await.expect("joined").expect("served");
}

// ------------------------------------------------------------ borrow mode (claude-auth-manager on a node)

fn shared(id: &str, slot: &str, uuid: &str) -> Account {
    Account {
        id: id.into(),
        slot: slot.into(),
        account_uuid: Some(uuid.into()),
        email: Some(format!("{id}@example.com")),
        ..Default::default()
    }
}

async fn borrowed(w: &World) -> Coordinator<World> {
    let c = Coordinator::borrowing(
        w.clone(),
        Info { version: "0.7.1".into(), pid: 1, started_at: now0(), is_managed: true },
        None,
    );
    c.set_shared_accounts(vec![shared("2", SLOT2, "uuid-2"), shared("3", SLOT3, "uuid-3")]);
    c.start().await;
    settle().await;
    c
}

fn models(fable: f64) -> Option<std::collections::BTreeMap<String, UsageWindow>> {
    Some(std::collections::BTreeMap::from([("Fable".to_string(), UsageWindow { pct: fable, resets_at: None })]))
}

#[tokio::test]
async fn borrowing_never_polls_refreshes_reads_profiles_or_signs_in() {
    let w = World::new();
    let c = borrowed(&w).await;
    w.advance(30 * 60_000).await;
    let r = c.handle(req("POST", "/accounts/2/check", &[], Some(json!({ "fresh": true })))).await;
    assert_eq!(r.status, 200);
    settle().await;
    let x = w.w();
    assert!(x.usage_calls.is_empty() && x.profile_calls.is_empty() && x.refreshes.is_empty() && x.sign_ins.is_empty());
    drop(x);
    let ids: Vec<String> = c.state().accounts.iter().map(|a| a.account.id.clone()).collect();
    assert_eq!(ids, vec!["1", "2", "3"], "Claude Code's own sign-in stays, as account 1");
    assert_eq!(account(&c, "2").health, Health::Ok { refresh_expires_at: None });
}

#[tokio::test]
async fn borrowing_refuses_to_add_remove_or_sign_in() {
    let w = World::new();
    let c = borrowed(&w).await;
    for (method, path) in [("POST", "/accounts"), ("POST", "/accounts/2/remove"), ("POST", "/signin")] {
        let r = c.handle(req(method, path, &[], Some(json!({})))).await;
        assert_eq!(r.status, 409, "{method} {path}");
        assert!(r.body.to_string().contains("auth hub"), "{}", r.body);
    }
    assert!(w.w().sign_outs.is_empty());
    // The state says so, for the rotator to offer none of them.
    let state = serde_json::to_value(c.state()).expect("state");
    assert_eq!(state["managedBy"], json!("the auth hub"));
    let own = started(&World::new(), None).await;
    assert_eq!(serde_json::to_value(own.state()).expect("state").get("managedBy"), None);
}

#[tokio::test]
async fn readings_from_elsewhere_are_found_by_account_uuid_and_the_newest_wins() {
    let w = World::new();
    let c = borrowed(&w).await;
    let t = w.now();
    // The hub's poll, with the model week.
    let hub = Reading { models: models(80.0), ..reading(Some(10.0), Some(40.0), t - 60_000) };
    assert!(c.borrow_reading("uuid-2", hub, super::ReadingSource::Borrowed));
    assert_eq!(week(&c, "2"), Some(40.0));
    // A session here, newer: it wins and keeps the hub's model week.
    c.report("2", reading(Some(12.0), Some(41.0), t)).expect("reported");
    let r = account(&c, "2").reading.expect("a reading");
    assert_eq!((r.seven_day.map(|w| w.pct), r.models.map(|m| m["Fable"].pct)), (Some(41.0), Some(80.0)));
    // An older hub reading only brings its model week when the current reading has none.
    c.borrow_reading("uuid-3", reading(Some(5.0), Some(5.0), t), super::ReadingSource::Borrowed);
    let older = Reading { models: models(90.0), ..reading(Some(1.0), Some(1.0), t - 120_000) };
    assert!(c.borrow_reading("uuid-3", older, super::ReadingSource::Borrowed));
    let r = account(&c, "3").reading.expect("a reading");
    assert_eq!((r.seven_day.map(|w| w.pct), r.models.map(|m| m["Fable"].pct)), (Some(5.0), Some(90.0)));
    assert!(!c.borrow_reading("uuid-unknown", reading(None, Some(1.0), t), super::ReadingSource::Borrowed));
}

#[tokio::test]
async fn only_readings_sessions_here_reported_go_out_and_each_once() {
    let w = World::new();
    let c = borrowed(&w).await;
    let t = w.now();
    c.borrow_reading("uuid-3", reading(None, Some(3.0), t), super::ReadingSource::Borrowed);
    c.report("2", reading(None, Some(2.0), t)).expect("reported");
    let out = c.session_readings(&HashMap::new());
    assert_eq!(out.iter().map(|(a, _)| a.id.as_str()).collect::<Vec<_>>(), vec!["2"]);
    let sent = HashMap::from([("2".to_string(), t)]);
    assert!(c.session_readings(&sent).is_empty());
    c.report("2", reading(None, Some(2.5), t + 1)).expect("reported");
    assert_eq!(c.session_readings(&sent).len(), 1);
    // As a claude.usage reading: keyed by account UUID, the coordinator's reading shape, nothing secret.
    let (a, r) = &c.session_readings(&sent)[0];
    let reading = super::usage_reading(a, r).expect("a reading");
    assert_eq!((reading.kind.as_str(), reading.key.as_str()), ("claude.usage", "uuid-2"));
    assert_eq!(reading.data["sevenDay"]["pct"], 2.5);
    assert_eq!(reading.data["source"], "session");
    assert_eq!(reading.data["at"], t + 1);
}

#[tokio::test]
async fn take_readings_skips_this_nodes_own_and_other_kinds() {
    let w = World::new();
    let c = borrowed(&w).await;
    let at = jiff::Timestamp::from_millisecond(w.now()).expect("time");
    let r = |kind: &str, node: &str, pct: f64| fleetlayer_protocol::ext::Reading {
        kind: kind.into(),
        key: "uuid-2".into(),
        at,
        node: Some(node.into()),
        data: json!({ "sevenDay": { "pct": pct }, "at": w.now(), "source": "poll" }),
    };
    let taken =
        super::take_readings(&c, "laptop", &[r("claude.usage", "laptop", 99.0), r("claude.token_health", "hub", 98.0)]);
    assert_eq!((taken, week(&c, "2")), (0, None));
    assert_eq!(super::take_readings(&c, "laptop", &[r("claude.usage", "hub", 42.0)]), 1);
    assert_eq!(week(&c, "2"), Some(42.0));
}

#[tokio::test]
async fn an_account_the_hub_stops_sharing_goes_once_its_sessions_move_off() {
    let w = World::new();
    let c = borrowed(&w).await;
    w.w().alive.insert(77);
    c.register("s1", Some(77), SLOT3, None, true);
    c.set_shared_accounts(vec![shared("2", SLOT2, "uuid-2")]);
    assert!(matches!(account(&c, "3").health, Health::LoginNeeded { .. }), "marked so the rotator moves it off");
    w.w().alive.clear();
    w.advance(super::core::TICK_MS).await;
    assert!(c.state().accounts.iter().all(|a| a.account.id != "3"));
}

#[tokio::test]
async fn a_rejected_token_needs_a_new_one_until_a_response_or_a_check_shows_it_works() {
    let w = World::new();
    let c = borrowed(&w).await;
    c.rejected("2").expect("known");
    assert!(matches!(account(&c, "2").health, Health::LoginNeeded { .. }));
    let notice = c.state().notices.last().map(|n| n.text.clone()).unwrap_or_default();
    assert!(notice.contains("auth hub"), "{notice}");
    // The bundle applied again (same accounts): still rejected until shown otherwise.
    c.set_shared_accounts(vec![shared("2", SLOT2, "uuid-2"), shared("3", SLOT3, "uuid-3")]);
    assert!(matches!(account(&c, "2").health, Health::LoginNeeded { .. }));
    c.report("2", reading(None, Some(2.0), w.now())).expect("reported");
    assert_eq!(account(&c, "2").health, Health::Ok { refresh_expires_at: None });
    c.token_checked("uuid-3", false, Some("401 OAuth access token is invalid"));
    assert!(matches!(account(&c, "3").health, Health::LoginNeeded { ref why } if why.contains("401")));
    c.token_checked("uuid-3", true, None);
    assert_eq!(account(&c, "3").health, Health::Ok { refresh_expires_at: None });
}
