//! The module against claude-summarizer's behaviour (fixtures + expected output from its own Python), and the engine,
//! trigger, sweep, catch-up and import with a fake model.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fleetlayer_protocol::ext::Status as AgentStatus;
use fleetlayer_protocol::summary::{RecordOrigin, SessionFacts, SessionSignal, SummaryEvent, SummaryPeriod};
use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::accounts::Account;
use super::call::{CallError, CallFuture, CallRequest, CallStats, Model};
use super::condense;
use super::engine::{Accounts, Config, Engine, Stop};
use super::prompt::SessionInfo;
use super::transcript::{self, Agent, Kind, Roots, Source};
use super::{CatchupRequest, start};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/summaries")
}

fn auckland() -> jiff::tz::TimeZone {
    jiff::tz::TimeZone::get("Pacific/Auckland").unwrap()
}

// ---------------------------------------------------------------- parity with claude-summarizer

#[test]
fn reads_the_fixtures_as_claude_summarizer_does() {
    let dir = fixtures();
    let expected: Value = serde_json::from_slice(&std::fs::read(dir.join("expected.json")).unwrap()).unwrap();
    let home = dir.join("home");
    let roots = Roots {
        claude_projects: home.join(".claude/projects"),
        codex_sessions: home.join(".codex/sessions"),
        home: "/Users/sam".into(),
    };
    let mut sources = transcript::discover(&roots, std::time::UNIX_EPOCH);
    sources.sort_by(|a, b| a.path.cmp(&b.path));
    let rels: Vec<String> =
        sources.iter().map(|s| s.path.strip_prefix(&dir).unwrap().to_string_lossy().into_owned()).collect();
    let mut want: Vec<String> = expected.as_object().unwrap().keys().cloned().collect();
    want.sort();
    assert_eq!(rels, want, "the same transcripts, and no subagent files");

    for (source, rel) in sources.iter().zip(&rels) {
        let exp = &expected[rel];
        let res = transcript::read(source, 0, "/Users/sam").unwrap();
        assert_eq!(res.skip.as_deref(), exp["skip"].as_str(), "{rel}");
        assert_eq!(res.session_id.as_deref(), exp["session_id"].as_str(), "{rel}");
        assert_eq!(res.cwd.as_deref(), exp["cwd"].as_str(), "{rel}");
        assert_eq!(res.end, exp["end"].as_u64().unwrap(), "{rel}");
        // The one deliberate difference: messages from other sessions are kept here, dropped there.
        let (peers, events): (Vec<_>, Vec<_>) = res.events.into_iter().partition(|e| e.kind == Kind::Peer);
        let got: Vec<Value> = events
            .iter()
            .map(|e| json!({"kind": serde_json::to_value(e.kind).unwrap(), "text": e.text, "end": e.end}))
            .collect();
        assert_eq!(Value::Array(got), exp["events"], "{rel}");
        assert_eq!(condense::is_real(&events), exp["is_real"].as_bool().unwrap(), "{rel}");
        if !events.is_empty() {
            assert_eq!(
                condense::condense(&events, condense::BUDGET_CHARS, &auckland()),
                exp["condensed"].as_str().unwrap(),
                "{rel}"
            );
            assert_eq!(
                condense::condense(&events, 400, &auckland()),
                exp["condensed_small"].as_str().unwrap(),
                "{rel}"
            );
        }
        if rel.contains("11111111") {
            assert_eq!(peers.len(), 1);
            assert_eq!(peers[0].text, "Heads-up from billing: the export now includes credit notes.");
        } else {
            assert!(peers.is_empty());
        }
    }
}

// ---------------------------------------------------------------- a fake model and transcripts written by tests

#[derive(Default)]
struct Fake {
    answers: Mutex<VecDeque<Result<Value, CallError>>>,
    prompts: Mutex<Vec<String>>,
    /// The account each call used.
    accounts: Mutex<Vec<String>>,
    delay: Duration,
    running: Arc<AtomicUsize>,
    max_running: AtomicUsize,
}

impl Fake {
    fn new(answers: Vec<Result<Value, CallError>>) -> Arc<Fake> {
        Arc::new(Fake { answers: Mutex::new(answers.into()), ..Default::default() })
    }
    fn slow(delay: Duration) -> Arc<Fake> {
        Arc::new(Fake { delay, ..Default::default() })
    }
    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

fn update(sentence: &str) -> Result<Value, CallError> {
    Ok(
        json!({"status": "update", "page": format!("Page: {sentence}"), "paragraph": format!("Para: {sentence}"), "sentence": sentence}),
    )
}

impl Model for Fake {
    fn call(&self, req: CallRequest) -> CallFuture {
        self.prompts.lock().unwrap().push(req.prompt.clone());
        self.accounts.lock().unwrap().push(req.account.id.clone());
        let answer = self.answers.lock().unwrap().pop_front();
        let is_catchup = req.schema.contains("catchup");
        let n = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_running.fetch_max(n, Ordering::SeqCst);
        let delay = self.delay;
        let running = self.running.clone();
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            running.fetch_sub(1, Ordering::SeqCst);
            let value = answer.unwrap_or_else(|| {
                if is_catchup { Ok(json!({"catchup": "Caught up."})) } else { update("Default.") }
            })?;
            Ok((
                value,
                CallStats {
                    seconds: 1.0,
                    input_tokens: 4000,
                    output_tokens: 300,
                    cost_usd: Some(0.03),
                    account: req.account.id,
                },
            ))
        })
    }
}

fn running_now(fake: &Arc<Fake>) -> usize {
    fake.running.load(Ordering::SeqCst)
}

struct Home {
    _dir: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
}

fn home() -> Home {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".claude/projects/-code-app")).unwrap();
    let state = dir.path().join("state");
    Home { home, state, _dir: dir }
}

/// The tests' "now": noon in Auckland (the tests' time zone), so "30 minutes ago" is the same local day whatever the
/// real clock and the machine's time zone say. The engine files periods against it too (`Config::now`).
fn now() -> Timestamp {
    "2026-11-25T23:00:00Z".parse().unwrap()
}

fn ts(ago_minutes: i64) -> String {
    (now() - SignedDuration::from_mins(ago_minutes)).to_string()
}

fn human(sid: &str, text: &str, ago: i64) -> Value {
    json!({"type": "user", "entrypoint": "cli", "cwd": "/code/app", "sessionId": sid, "uuid": format!("u-{text}"), "timestamp": ts(ago),
           "origin": {"kind": "human"}, "message": {"role": "user", "content": text}})
}

fn agent_text(sid: &str, text: &str, ago: i64) -> Value {
    json!({"type": "assistant", "entrypoint": "cli", "cwd": "/code/app", "sessionId": sid, "timestamp": ts(ago),
           "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}})
}

fn tool(sid: &str, name: &str, ago: i64) -> Value {
    json!({"type": "assistant", "entrypoint": "cli", "sessionId": sid, "timestamp": ts(ago),
           "message": {"role": "assistant", "content": [{"type": "tool_use", "name": name, "input": {"command": "ls"}}]}})
}

fn transcript_path(h: &Home, sid: &str) -> PathBuf {
    h.home.join(format!(".claude/projects/-code-app/{sid}.jsonl"))
}

fn append(h: &Home, sid: &str, records: &[Value]) -> PathBuf {
    use std::io::Write;
    let path = transcript_path(h, sid);
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).unwrap();
    for r in records {
        writeln!(f, "{r}").unwrap();
    }
    path
}

fn config(h: &Home) -> Config {
    let mut c = Config::new(&h.home, &h.state, "Sam");
    c.tz = auckland();
    c.now = Some(now());
    c
}

fn account() -> Account {
    Account {
        id: "3".into(),
        slot: "/slots/3".into(),
        email: "a@x".into(),
        week_pct: 10.0,
        resets_at: None,
        five_hour_resets_at: None,
    }
}

fn engine(h: &Home, model: Arc<Fake>, accounts: Accounts) -> (Engine, mpsc::Receiver<SummaryEvent>) {
    let (tx, rx) = mpsc::channel(100);
    (Engine::new(config(h), model, accounts, tx).unwrap(), rx)
}

fn drain(rx: &mut mpsc::Receiver<SummaryEvent>) -> Vec<SummaryEvent> {
    let mut out = vec![];
    while let Ok(e) = rx.try_recv() {
        out.push(e);
    }
    out
}

fn offset(e: &Engine, path: &Path) -> u64 {
    e.state.lock().unwrap().files.get(path).map(|f| f.offset).unwrap_or(0)
}

fn source(path: &Path) -> Source {
    Source { path: path.to_path_buf(), agent: Agent::Claude }
}

fn info() -> SessionInfo {
    SessionInfo { tab: Some("billing".into()), ..Default::default() }
}

// ---------------------------------------------------------------- the engine

#[tokio::test]
async fn a_pass_summarises_moves_the_offset_and_rewrites_the_page_next_time() {
    let h = home();
    let path = append(&h, "s1", &[human("s1", "Fix the export", 30), agent_text("s1", "Fixed it.", 29)]);
    let fake = Fake::new(vec![update("First."), Ok(json!({"status": "no_change"}))]);
    let (e, mut rx) = engine(&h, fake.clone(), Accounts::Fixed(Some(account())));
    let pass = e.process(&source(&path), info()).await.unwrap();
    assert_eq!((pass.calls, pass.updates), (1, 1));
    assert_eq!(offset(&e, &path), std::fs::metadata(&path).unwrap().len());
    let events = drain(&mut rx);
    let SummaryEvent::Summary(r) = &events[0] else { panic!("{events:?}") };
    assert_eq!((r.sentence.as_str(), r.session_id.as_str(), r.origin), ("First.", "s1", RecordOrigin::Node));
    assert!(matches!(&r.period, SummaryPeriod::Hour(k) if k.len() == 13), "{:?}", r.period);
    assert_eq!(r.tab.as_deref(), Some("billing"));
    assert!(matches!(events[1], SummaryEvent::Activity { .. }));
    let p = &fake.prompts()[0];
    assert!(p.contains("Current page: none yet") && p.contains("USER: Fix the export"), "{p}");

    // Nothing new: no call.
    assert_eq!(e.process(&source(&path), info()).await.unwrap(), Default::default());
    // New activity: the page it wrote is the current page; no_change sends Activity only.
    append(&h, "s1", &[human("s1", "And the CSV too", 5)]);
    let pass = e.process(&source(&path), info()).await.unwrap();
    assert_eq!((pass.calls, pass.no_change), (1, 1));
    assert!(fake.prompts()[1].contains("Current page (written "));
    assert!(fake.prompts()[1].contains("Page: First."));
    let events = drain(&mut rx);
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], SummaryEvent::Activity { .. }));
    // State survives a restart.
    let (e2, _rx) = engine(&h, fake.clone(), Accounts::Fixed(Some(account())));
    assert_eq!(e2.state.lock().unwrap().sessions["s1"].page, "Page: First.");
}

#[tokio::test]
async fn below_the_threshold_the_offset_moves_for_free() {
    let h = home();
    let path = append(&h, "s1", &[tool("s1", "Bash", 10), agent_text("s1", "Still waiting on CI.", 9)]);
    let fake = Fake::new(vec![]);
    let (e, mut rx) = engine(&h, fake.clone(), Accounts::Fixed(Some(account())));
    let pass = e.process(&source(&path), info()).await.unwrap();
    assert_eq!((pass.calls, pass.idle_chunks), (0, 1));
    assert_eq!(offset(&e, &path), std::fs::metadata(&path).unwrap().len());
    assert!(fake.prompts().is_empty());
    assert!(drain(&mut rx).is_empty());
}

#[tokio::test]
async fn a_failing_chunk_is_retried_twice_then_skipped() {
    let h = home();
    let path = append(&h, "s1", &[human("s1", "Go", 10)]);
    let fail = || Err(CallError::Failed("claude -p failed (error): boom".into()));
    let fake = Fake::new(vec![fail(), fail(), fail()]);
    let (e, _rx) = engine(&h, fake.clone(), Accounts::Fixed(Some(account())));
    for n in 1..=2 {
        let pass = e.process(&source(&path), info()).await.unwrap();
        assert_eq!(pass.failures, 1);
        let st = e.state.lock().unwrap().files[&path].clone();
        assert_eq!((st.offset, st.failures), (0, n), "the offset stays until a call succeeds");
    }
    e.process(&source(&path), info()).await.unwrap();
    assert_eq!(offset(&e, &path), std::fs::metadata(&path).unwrap().len(), "skipped after three failures");
}

#[tokio::test]
async fn a_usage_limit_or_no_account_stops_without_moving_or_counting() {
    let h = home();
    let path = append(&h, "s1", &[human("s1", "Go", 10)]);
    let fake = Fake::new(vec![Err(CallError::Limit("usage limit reached".into()))]);
    let (e, _rx) = engine(&h, fake.clone(), Accounts::Fixed(Some(account())));
    assert!(matches!(e.process(&source(&path), info()).await, Err(Stop::Limit(_))));
    assert_eq!(e.state.lock().unwrap().files.get(&path).map(|f| (f.offset, f.failures)).unwrap_or((0, 0)), (0, 0));
    // The account that hit its limit is left alone: the next pass doesn't call it again (there's no other here).
    assert_eq!(e.process(&source(&path), info()).await, Err(Stop::NoAccount));
    assert!(e.limited.lock().unwrap().contains_key(&account().id));
    let (e, _rx) = engine(&h, fake.clone(), Accounts::Fixed(None));
    assert_eq!(e.process(&source(&path), info()).await, Err(Stop::NoAccount));
    assert_eq!(offset(&e, &path), 0);
}

#[tokio::test]
async fn headless_runs_are_skipped_for_good() {
    let h = home();
    let mut r = human("s9", "summarise this", 10);
    r["entrypoint"] = json!("sdk-cli");
    let path = append(&h, "s9", &[r]);
    let fake = Fake::new(vec![]);
    let (e, _rx) = engine(&h, fake.clone(), Accounts::Fixed(Some(account())));
    e.process(&source(&path), info()).await.unwrap();
    assert_eq!(e.state.lock().unwrap().files[&path].skip.as_deref(), Some("headless run (sdk-cli)"));
    append(&h, "s9", &[human("s9", "more", 5)]);
    e.process(&source(&path), info()).await.unwrap();
    assert!(fake.prompts().is_empty());
}

#[tokio::test]
async fn the_cap_holds_and_old_history_is_chunked_by_day_and_week() {
    let h = home();
    let mut records = vec![];
    // 60 days ago (one ISO week), 10 days ago (a day), and today: three chunks, three periods.
    records.push(human("s1", "old work", 60 * 24 * 60));
    records.push(human("s1", "last week", 10 * 24 * 60));
    for i in 0..400 {
        records.push(human("s1", &format!("{i} {}", "x".repeat(400)), 30));
    }
    let path = append(&h, "s1", &records);
    let fake = Fake::new(vec![update("a"), update("b"), update("c")]);
    let (tx, mut rx) = mpsc::channel(100);
    let mut c = config(&h);
    c.budget = 5_000;
    let e = Engine::new(c, fake.clone(), Accounts::Fixed(Some(account())), tx).unwrap();
    assert_eq!(e.process(&source(&path), info()).await.unwrap().calls, 3);
    let periods: Vec<SummaryPeriod> = drain(&mut rx)
        .into_iter()
        .filter_map(|e| if let SummaryEvent::Summary(r) = e { Some(r.period) } else { None })
        .collect();
    assert!(matches!(periods[0], SummaryPeriod::Week(ref k) if k.contains("-W")), "{periods:?}");
    assert!(matches!(periods[1], SummaryPeriod::Day(ref k) if k.len() == 10), "{periods:?}");
    assert!(matches!(periods[2], SummaryPeriod::Hour(_)), "{periods:?}");
    let last = &fake.prompts()[2];
    let activity = last.split_once("New activity (").unwrap().1.split_once("):\n\n").unwrap().1;
    assert!(activity.trim_end().chars().count() <= 5_000, "{}", activity.chars().count());
    assert!(activity.contains("399 "), "the newest message survives");
}

// ---------------------------------------------------------------- the trigger, the sweep and the limits

fn facts(sid: &str) -> SessionFacts {
    SessionFacts {
        session_id: sid.into(),
        agent: Agent::Claude,
        tab: Some(format!("tab {sid}")),
        cwd: Some("/code/app".into()),
        transcript: None,
    }
}

fn status(sid: &str, s: AgentStatus) -> SessionSignal {
    SessionSignal::Status { session: facts(sid), status: s, at: Timestamp::now() }
}

struct Running {
    signals: mpsc::Sender<SessionSignal>,
    events: mpsc::Receiver<SummaryEvent>,
    handle: super::SummariesHandle,
}

fn run(h: &Home, fake: &Arc<Fake>, sweep: bool) -> Running {
    run_with(h, fake, sweep, Accounts::Fixed(Some(account())))
}

fn run_with(h: &Home, fake: &Arc<Fake>, sweep: bool, accounts: Accounts) -> Running {
    let (signals, srx) = mpsc::channel(100);
    let (etx, events) = mpsc::channel(1000);
    let mut c = config(h);
    c.sweep = sweep;
    let handle = start(c, fake.clone(), accounts, srx, etx, true);
    Running { signals, events, handle }
}

fn two_accounts() -> Accounts {
    let mut four = account();
    four.id = "4".into();
    Accounts::List(vec![account(), four])
}

async fn advance(secs: u64) {
    for _ in 0..secs {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    // Let spawned passes finish their file reads.
    for _ in 0..20 {
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

#[tokio::test(start_paused = true)]
async fn idle_for_a_minute_triggers_a_summary_and_blocked_never_does() {
    let h = home();
    append(&h, "s1", &[human("s1", "Do it", 5)]);
    append(&h, "s2", &[human("s2", "Can I run this?", 5)]);
    let fake = Fake::new(vec![]);
    let r = run(&h, &fake, false);
    r.signals.send(status("s1", AgentStatus::Done)).await.unwrap();
    r.signals.send(status("s2", AgentStatus::Idle)).await.unwrap();
    r.signals.send(status("s2", AgentStatus::Blocked)).await.unwrap();
    advance(30).await;
    assert!(fake.prompts().is_empty(), "not before a minute");
    advance(40).await;
    let prompts = fake.prompts();
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].contains("session ID s1") && prompts[0].contains("tab \"tab s1\""));
    advance(3600).await;
    assert_eq!(fake.prompts().len(), 1, "the blocked session is never summarised");
    drop(r.handle);
}

#[tokio::test(start_paused = true)]
async fn working_inside_the_minute_cancels_and_an_idle_inside_the_gap_waits_for_it() {
    let h = home();
    append(&h, "s1", &[human("s1", "One", 5)]);
    let fake = Fake::new(vec![]);
    let r = run(&h, &fake, false);
    r.signals.send(status("s1", AgentStatus::Idle)).await.unwrap();
    advance(30).await;
    r.signals.send(status("s1", AgentStatus::Working)).await.unwrap();
    advance(120).await;
    assert!(fake.prompts().is_empty());
    r.signals.send(status("s1", AgentStatus::Idle)).await.unwrap();
    advance(61).await;
    assert_eq!(fake.prompts().len(), 1);
    // Ten minutes on, more work and another idle: due 30 minutes after the first call, not dropped.
    advance(600).await;
    append(&h, "s1", &[human("s1", "Two", 1)]);
    r.signals.send(status("s1", AgentStatus::Working)).await.unwrap();
    r.signals.send(status("s1", AgentStatus::Idle)).await.unwrap();
    r.signals.send(status("s1", AgentStatus::Done)).await.unwrap();
    advance(600).await;
    assert_eq!(fake.prompts().len(), 1, "inside the gap");
    advance(660).await;
    assert_eq!(fake.prompts().len(), 2, "when the gap ended");
    assert!(fake.prompts()[1].contains("USER: Two"));
}

#[tokio::test(start_paused = true)]
async fn at_most_two_calls_at_once() {
    let h = home();
    for sid in ["s1", "s2", "s3", "s4"] {
        append(&h, sid, &[human(sid, "Go", 5)]);
    }
    let fake = Fake::slow(Duration::from_secs(15));
    let r = run(&h, &fake, false);
    for sid in ["s1", "s2", "s3", "s4"] {
        r.signals.send(status(sid, AgentStatus::Idle)).await.unwrap();
    }
    advance(62).await;
    assert_eq!(running_now(&fake), 2);
    assert_eq!(fake.prompts().len(), 2);
    advance(15).await;
    assert_eq!(fake.prompts().len(), 4);
    advance(30).await;
    assert_eq!(fake.prompts().len(), 4);
    assert_eq!(fake.max_running.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn the_start_up_sweep_takes_sessions_that_ended_and_leaves_running_ones() {
    let h = home();
    append(&h, "ended", &[human("ended", "Crashed mid-turn", 50)]);
    append(&h, "live", &[human("live", "Still going", 5)]);
    let mut headless = human("bot", "x", 5);
    headless["entrypoint"] = json!("sdk-cli");
    append(&h, "bot", &[headless]);
    let fake = Fake::new(vec![]);
    let r = run(&h, &fake, true);
    r.signals.send(status("live", AgentStatus::Working)).await.unwrap();
    advance(35).await;
    let prompts = fake.prompts();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    assert!(prompts[0].contains("session ID ended"));
    // A close event: the session ended, so it's summarised after the idle minute.
    r.signals.send(SessionSignal::Closed { session_id: "live".into(), at: Timestamp::now() }).await.unwrap();
    advance(61).await;
    assert_eq!(fake.prompts().len(), 2);
}

/// The VM's case on 27 Nov 2026: a session that hit a usage limit waited until something else happened. Now it's tried
/// again by itself, on another account, and stops waiting once it's written.
#[tokio::test(start_paused = true)]
async fn a_session_that_hit_a_limit_is_retried_by_itself_on_another_account() {
    let h = home();
    append(&h, "s1", &[human("s1", "Go", 5)]);
    let fake = Fake::new(vec![Err(CallError::Limit("You've hit your 5-hour limit".into()))]);
    let r = run_with(&h, &fake, false, two_accounts());
    r.signals.send(status("s1", AgentStatus::Idle)).await.unwrap();
    advance(62).await;
    assert_eq!(*fake.accounts.lock().unwrap(), ["3"]);
    let report = r.handle.status().borrow().clone();
    assert_eq!(report.detail["waiting"], 1);
    assert_eq!(report.detail["oldestWaiting"]["tab"], "tab s1");
    assert!(report.detail["oldestWaiting"]["why"].as_str().unwrap().contains("5-hour limit"));
    assert!(report.problem.is_none(), "not a problem yet");
    // No other event: the retry comes a minute later, on account 4.
    advance(62).await;
    assert_eq!(*fake.accounts.lock().unwrap(), ["3", "4"]);
    let report = r.handle.status().borrow().clone();
    assert_eq!(report.detail["waiting"], 0);
    assert!(report.detail["lastSummaryAt"].is_string());
}

/// A transcript the sweep found is put back in the queue too.
#[tokio::test(start_paused = true)]
async fn a_swept_transcript_that_hit_a_limit_is_queued_again() {
    let h = home();
    append(&h, "ended", &[human("ended", "Crashed mid-turn", 50)]);
    let fake = Fake::new(vec![Err(CallError::Limit("usage limit reached".into()))]);
    let r = run_with(&h, &fake, true, two_accounts());
    advance(35).await;
    assert_eq!(*fake.accounts.lock().unwrap(), ["3"]);
    advance(62).await;
    assert_eq!(*fake.accounts.lock().unwrap(), ["3", "4"]);
    assert_eq!(r.handle.status().borrow().detail["waiting"], 0);
}

/// Waiting more than an hour (no account has room, none says when) is a problem the node's health shows.
#[tokio::test(start_paused = true)]
async fn waiting_over_an_hour_is_reported_as_a_problem() {
    let h = home();
    append(&h, "s1", &[human("s1", "Go", 5)]);
    let fake = Fake::new(vec![]);
    let r = run_with(&h, &fake, false, Accounts::Fixed(None));
    r.signals.send(status("s1", AgentStatus::Idle)).await.unwrap();
    advance(62).await;
    assert!(r.handle.status().borrow().problem.is_none());
    assert_eq!(r.handle.status().borrow().detail["waiting"], 1);
    advance(3600).await;
    let report = r.handle.status().borrow().clone();
    let problem = report.problem.expect("a problem after an hour");
    assert!(problem.starts_with("summaries for tab s1 have waited since "), "{problem}");
    assert!(problem.ends_with("no account has room left this week"), "{problem}");
    assert!(fake.prompts().is_empty());
}

// ---------------------------------------------------------------- catch-up and import

#[tokio::test]
async fn catchup_waits_when_nothing_is_cached_then_serves_stale_while_it_rewrites() {
    let h = home();
    let mut compact = human("s1", "This session is being continued from a previous conversation", 20);
    compact["isCompactSummary"] = json!(true);
    let mut wake = human("s1", "Continue: check CI", 15);
    wake["isMeta"] = json!(true);
    let path = append(
        &h,
        "s1",
        &[human("s1", "Ship sc-1234", 30), compact, wake, agent_text("s1", "CI is green; PR #9 is ready.", 10)],
    );
    let fake = Fake::new(vec![Ok(json!({"catchup": "PR #9 is ready."})), Ok(json!({"catchup": "Merged."}))]);
    let mut r = run(&h, &fake, false);
    let c = r.handle.catchup(CatchupRequest { session_id: "s1".into(), transcript: Some(path.clone()) }).await.unwrap();
    assert_eq!((c.text.as_str(), c.stale), ("PR #9 is ready.", false));
    assert_eq!(
        c.anchor.as_ref().unwrap().uuid.as_deref(),
        Some("u-Ship sc-1234"),
        "wake-ups and compactions aren't the anchor"
    );
    let p = &fake.prompts()[0];
    assert!(p.contains("Ship sc-1234") && p.contains("CI is green"));
    // Cached and fresh: no call.
    assert!(!r.handle.catchup(CatchupRequest { session_id: "s1".into(), transcript: None }).await.unwrap().stale);
    assert_eq!(fake.prompts().len(), 1);
    while r.events.try_recv().is_ok() {} // the first one's event
    // The transcript grew: the old text at once, stale, and the new one as an event.
    append(&h, "s1", &[agent_text("s1", "Merged PR #9.", 1)]);
    let c = r.handle.catchup(CatchupRequest { session_id: "s1".into(), transcript: None }).await.unwrap();
    assert_eq!((c.text.as_str(), c.stale), ("PR #9 is ready.", true));
    let mut fresh = None;
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        if let Ok(SummaryEvent::Catchup(c)) = r.events.try_recv() {
            fresh = Some(c);
            break;
        }
    }
    let fresh = fresh.expect("a catch-up event");
    assert_eq!((fresh.text.as_str(), fresh.stale), ("Merged.", false));
}

#[tokio::test]
async fn the_import_sends_every_update_and_keeps_claude_summarizers_offsets() {
    let h = home();
    let vault = h.home.join("Notes/Sessions");
    std::fs::create_dir_all(vault.join("ci")).unwrap();
    let path = append(&h, "s1", &[human("s1", "Already summarised", 60)]);
    let update = |sentence: &str| {
        format!(
            "---\nsession_id: \"s1\"\ntab: \"ci\"\nagent: \"claude\"\ndirectory: \"/code/app\"\nactivity_from: \"2026-10-23T11:18+13:00\"\nactivity_to: \"2026-10-23T23:21+13:00\"\nwritten: \"2026-11-21T18:35:57+13:00\"\n---\n## Sentence\n\n{sentence}\n\n## Paragraph\n\nP\n\n## Page\n\nThe page.\n"
        )
    };
    std::fs::write(vault.join("ci/2026-10-23T23.md"), update("First.")).unwrap();
    std::fs::write(vault.join("ci/2026-10-24T09.md"), update("Second.")).unwrap();
    std::fs::write(
        vault.join("index.json"),
        json!({"sessions": {"s1": {"agent": "claude", "folder": "ci", "tab": "ci", "transcripts": [path]}}})
            .to_string(),
    )
    .unwrap();
    let old = h.home.join(".local/state/claude-summarizer");
    std::fs::create_dir_all(&old).unwrap();
    let size = std::fs::metadata(&path).unwrap().len();
    std::fs::write(old.join("state.json"), json!({"files": {path.to_string_lossy(): {"offset": size}}}).to_string())
        .unwrap();

    let fake = Fake::new(vec![]);
    let mut r = run(&h, &fake, false);
    assert_eq!(r.handle.import_claude_summarizer(vault).await.unwrap(), 2);
    let mut sentences = vec![];
    while let Ok(SummaryEvent::Summary(rec)) = r.events.try_recv() {
        assert_eq!(rec.origin, RecordOrigin::ClaudeSummarizer);
        sentences.push(rec.sentence);
    }
    assert_eq!(sentences, ["First.", "Second."]);
    // Nothing claude-summarizer already read is summarised again.
    let pass = r.handle.engine.process(&source(&path), info()).await.unwrap();
    assert_eq!(pass.calls, 0);
    append(&h, "s1", &[human("s1", "New work", 1)]);
    r.handle.engine.process(&source(&path), info()).await.unwrap();
    let p = &fake.prompts()[0];
    assert!(
        p.contains("Current page (written 2026-10-24T09)")
            && p.contains("The page.")
            && !p.contains("Already summarised"),
        "{p}"
    );
}

/// Dumps what the port reads from every real transcript on this machine, for `tests/fixtures/summaries/parity.py`.
#[test]
#[ignore = "reads this machine's transcripts; run by hand with SUMMARIES_PARITY_OUT"]
fn parity_dump() {
    use std::io::Write;
    let out = std::env::var("SUMMARIES_PARITY_OUT").expect("SUMMARIES_PARITY_OUT");
    let home = PathBuf::from(std::env::var("HOME").unwrap());
    let roots = Roots::from_home(&home);
    let cutoff = std::time::SystemTime::now() - Duration::from_secs(183 * 86400);
    let mut f = std::io::BufWriter::new(std::fs::File::create(out).unwrap());
    for s in transcript::discover(&roots, cutoff) {
        let res = transcript::read(&s, 0, &roots.home).unwrap();
        let peers = res.events.iter().filter(|e| e.kind == Kind::Peer).count();
        let events: Vec<Value> = res
            .events
            .iter()
            .filter(|e| e.kind != Kind::Peer)
            .map(|e| json!([serde_json::to_value(e.kind).unwrap(), e.text, e.end]))
            .collect();
        let row = json!({"path": s.path, "skip": res.skip, "session_id": res.session_id, "cwd": res.cwd, "end": res.end, "events": events, "peers": peers});
        writeln!(f, "{row}").unwrap();
    }
}

/// A machine that ran claude-summarizer, with a transcript it has read up to some offset and a page for it.
fn claude_summarizer_was_here(h: &Home, sid: &str, read_to: u64) {
    let vault = h.home.join(".local/share/claude-summarizer");
    std::fs::create_dir_all(vault.join("ci")).unwrap();
    std::fs::write(
        vault.join("ci/2026-11-20T10.md"),
        "---\nactivity_from: \"2026-11-20T09:00+13:00\"\nactivity_to: \"2026-11-20T10:00+13:00\"\n---\n## Sentence\n\nS.\n\n## Paragraph\n\nP.\n\n## Page\n\nThe old page.\n",
    )
    .unwrap();
    std::fs::write(
        vault.join("index.json"),
        json!({"sessions": {sid: {"agent": "claude", "folder": "ci", "tab": "ci", "transcripts": [transcript_path(h, sid)]}}}).to_string(),
    )
    .unwrap();
    let old = h.home.join(".local/state/claude-summarizer");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(
        old.join("state.json"),
        json!({"files": {transcript_path(h, sid).to_string_lossy(): {"offset": read_to}}}).to_string(),
    )
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn on_start_it_adopts_claude_summarizers_offsets_without_any_import() {
    let h = home();
    let path =
        append(&h, "s1", &[human("s1", "Weeks of old work", 60 * 24 * 40), human("s1", "Already summarised", 60)]);
    claude_summarizer_was_here(&h, "s1", std::fs::metadata(&path).unwrap().len());
    append(&h, "s1", &[human("s1", "New since then", 5)]);
    append(&h, "ended", &[human("ended", "Not in claude-summarizer's state", 50)]);
    let fake = Fake::new(vec![]);
    let r = run(&h, &fake, true);
    r.signals.send(status("s1", AgentStatus::Idle)).await.unwrap();
    advance(65).await;
    let prompts = fake.prompts();
    let s1: Vec<&String> = prompts.iter().filter(|p| p.contains("session ID s1")).collect();
    assert_eq!(s1.len(), 1, "one call, for the new activity only: {prompts:?}");
    assert!(
        s1[0].contains("New since then") && !s1[0].contains("Already summarised") && !s1[0].contains("Weeks of old")
    );
    assert!(s1[0].contains("Current page (written 2026-11-20T10)") && s1[0].contains("The old page."));
    // The sweep runs as usual for what claude-summarizer never read.
    assert!(prompts.iter().any(|p| p.contains("session ID ended")));
    // Nothing was published for the history: that's the import's job.
    let mut origins = vec![];
    let mut events = r.events;
    while let Ok(SummaryEvent::Summary(rec)) = events.try_recv() {
        origins.push(rec.origin);
    }
    assert!(origins.iter().all(|o| *o == RecordOrigin::Node));
    // Adopted once: a restart doesn't adopt again, and the import still sends the history.
    let st = super::state::State::load(&h.state.join("summaries.json")).unwrap();
    assert!(st.seeded_at.is_some() && st.imported_at.is_none());
    drop(r.handle);
}

#[tokio::test(start_paused = true)]
async fn the_import_after_adopting_still_sends_the_history_and_sweeps() {
    let h = home();
    let path = append(&h, "s1", &[human("s1", "Already summarised", 60)]);
    claude_summarizer_was_here(&h, "s1", std::fs::metadata(&path).unwrap().len());
    let fake = Fake::new(vec![]);
    let mut r = run(&h, &fake, false);
    advance(1).await;
    let vault = h.home.join(".local/share/claude-summarizer");
    assert_eq!(r.handle.import_claude_summarizer(vault).await.unwrap(), 1);
    match r.events.try_recv() {
        Ok(SummaryEvent::Summary(rec)) => {
            assert_eq!((rec.origin, rec.sentence.as_str()), (RecordOrigin::ClaudeSummarizer, "S."))
        }
        other => panic!("{other:?}"),
    }
    advance(2).await;
    assert!(fake.prompts().is_empty(), "claude-summarizer had read it all");
}
