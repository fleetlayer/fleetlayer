use std::sync::Mutex as StdMutex;

use async_trait::async_trait;
use fleetlayer_protocol::rpc::{RpcError, code};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};

use crate::engine::{Engine, Outcome, State, Why, classify};
use crate::runner::{FireRequest, Paths, Runner};
use crate::schedule::{Cron, Schedule, format_duration, parse_duration};
use crate::settings::{self, Settings};

fn t(s: &str) -> Timestamp {
    s.parse().unwrap()
}

fn mins(n: i64) -> SignedDuration {
    SignedDuration::from_mins(n)
}

fn parse(yaml: &str) -> Settings {
    settings::parse(yaml, &TimeZone::UTC).unwrap()
}

const EVERY_3H: &str =
    "entries:\n  - name: worker check\n    to: worker@server\n    every: 3h\n    text: Check the queue.\n";

// --- schedules ---

#[test]
fn intervals_parse_and_print() {
    assert_eq!(parse_duration("3h").unwrap().as_secs(), 3 * 3600);
    assert_eq!(parse_duration("1h30m").unwrap().as_secs(), 5400);
    assert_eq!(parse_duration("2d").unwrap().as_secs(), 2 * 86_400);
    assert_eq!(parse_duration("1w").unwrap().as_secs(), 604_800);
    assert_eq!(format_duration(parse_duration("90m").unwrap()), "1h30m");
    for bad in ["", "3", "30s", "3x", "h", "1h30"] {
        assert!(parse_duration(bad).is_err(), "{bad} should be refused");
    }
}

#[test]
fn intervals_count_from_the_base_without_drift() {
    let s = Schedule::Every(parse_duration("3h").unwrap());
    let base = t("2026-11-28T00:00:00Z");
    assert_eq!(s.next_after(base, base), Some(t("2026-11-28T03:00:00Z")));
    assert_eq!(s.next_after(t("2026-11-28T03:00:07Z"), base), Some(t("2026-11-28T06:00:00Z")));
    assert_eq!(s.due(base, t("2026-11-28T02:59:59Z")), None);
    assert_eq!(s.due(base, t("2026-11-28T03:00:00Z")), Some((1, t("2026-11-28T03:00:00Z"))));
    assert_eq!(s.due(base, t("2026-11-28T10:00:00Z")), Some((3, t("2026-11-28T09:00:00Z"))));
}

#[test]
fn cron_fields() {
    let c = Cron::parse("0 */3 * * *").unwrap();
    assert_eq!(c.next_after(t("2026-11-28T01:15:00Z"), &TimeZone::UTC), Some(t("2026-11-28T03:00:00Z")));
    assert_eq!(c.next_after(t("2026-11-28T03:00:00Z"), &TimeZone::UTC), Some(t("2026-11-28T06:00:00Z")));
    // 28 November 2026 is a Saturday: weekdays at nine skip to Monday.
    let c = Cron::parse("0 9 * * mon-fri").unwrap();
    assert_eq!(c.next_after(t("2026-11-28T10:00:00Z"), &TimeZone::UTC), Some(t("2026-11-30T09:00:00Z")));
    // Read in the entry's zone: 9:00 in New York is 14:00 UTC in winter.
    let ny = TimeZone::get("America/New_York").unwrap();
    assert_eq!(c.next_after(t("2026-11-28T10:00:00Z"), &ny), Some(t("2026-11-30T14:00:00Z")));
    // Both day fields restricted: either matches (the 1st, or a Sunday).
    let c = Cron::parse("30 8 1 * 0").unwrap();
    assert_eq!(c.next_after(t("2026-11-28T00:00:00Z"), &TimeZone::UTC), Some(t("2026-11-29T08:30:00Z")));
    assert_eq!(c.next_after(t("2026-11-29T09:00:00Z"), &TimeZone::UTC), Some(t("2026-12-01T08:30:00Z")));
    // 7 is Sunday too; month names; aliases; steps from a start.
    assert_eq!(
        Cron::parse("0 0 * * 7").unwrap().next_after(t("2026-11-28T00:00:00Z"), &TimeZone::UTC),
        Some(t("2026-11-29T00:00:00Z"))
    );
    assert_eq!(
        Cron::parse("0 0 1 jan *").unwrap().next_after(t("2026-11-28T00:00:00Z"), &TimeZone::UTC),
        Some(t("2027-01-01T00:00:00Z"))
    );
    assert_eq!(
        Cron::parse("@daily").unwrap().next_after(t("2026-11-28T00:00:00Z"), &TimeZone::UTC),
        Some(t("2026-11-29T00:00:00Z"))
    );
    assert_eq!(
        Cron::parse("5/20 * * * *").unwrap().next_after(t("2026-11-28T00:26:00Z"), &TimeZone::UTC),
        Some(t("2026-11-28T00:45:00Z"))
    );
    // A date that comes round only in leap years.
    assert_eq!(
        Cron::parse("0 0 29 2 *").unwrap().next_after(t("2026-11-28T00:00:00Z"), &TimeZone::UTC),
        Some(t("2028-02-29T00:00:00Z"))
    );
    for bad in
        ["* * * *", "60 * * * *", "* 24 * * *", "* * 0 * *", "* * * 13 *", "*/0 * * * *", "5-1 * * * *", "x * * * *"]
    {
        assert!(Cron::parse(bad).is_err(), "{bad} should be refused");
    }
}

#[test]
fn cron_across_the_clocks_changing() {
    let london = TimeZone::get("Europe/London").unwrap();
    // 29 March 2026: 01:00 to 02:00 doesn't exist in London. 01:30 fires once, after the gap.
    let c = Cron::parse("30 1 * * *").unwrap();
    let first = c.next_after(t("2026-03-29T00:00:00Z"), &london).unwrap();
    assert_eq!(first, t("2026-03-29T01:30:00Z"));
    assert_eq!(c.next_after(first, &london), Some(t("2026-03-30T00:30:00Z")));
    // 25 October 2026: 01:30 happens twice; it fires once, at the first.
    let first = c.next_after(t("2026-10-24T23:00:00Z"), &london).unwrap();
    assert_eq!(first, t("2026-10-25T00:30:00Z"));
    assert_eq!(c.next_after(first, &london), Some(t("2026-10-26T01:30:00Z")));
}

#[test]
fn a_cron_scheduler_off_for_years_still_finds_the_latest_fire() {
    let s = Schedule::Cron { cron: Box::new(Cron::parse("* * * * *").unwrap()), tz: TimeZone::UTC };
    let (count, latest) = s.due(t("2020-01-01T00:00:00Z"), t("2026-11-28T10:00:30Z")).unwrap();
    assert_eq!(latest, t("2026-11-28T10:00:00Z"));
    assert!(count >= 10_000);
}

// --- settings ---

#[test]
fn settings_read_every_kind_of_entry() {
    let s = parse(
        "alert_after: 3\nentries:\n  - name: a\n    to: worker@server\n    every: 3h\n    text: x\n  - name: b\n    to: \
         planning\n    cron: \"0 9 * * 1-5\"\n    tz: Europe/London\n    text: y\n    if_busy: skip\n    catch_up: false\n  \
         - name: c\n    to: fleetlayer scheduler@agents\n    at: 2026-11-28 15:00\n    tz: Europe/London\n    text: z\n    \
         enabled: false\n",
    );
    assert_eq!(s.alert_after, 3);
    assert_eq!(s.entries.len(), 3);
    assert_eq!(s.entries[1].if_busy, settings::IfBusy::Skip);
    assert!(!s.entries[1].catch_up);
    assert!(!s.entries[2].enabled);
    assert_eq!(s.entries[2].to, "fleetlayer scheduler@agents", "tab names have spaces");
    assert!(matches!(s.entries[2].schedule, Schedule::At(at) if at == t("2026-11-28T15:00:00Z")));
    assert_eq!(settings::parse("", &TimeZone::UTC).unwrap().entries.len(), 0);
}

#[test]
fn settings_problems_are_all_reported_with_their_entry() {
    let e = settings::parse(
        "entries:\n  - name: a\n    to: w\n    every: 3h\n    cron: \"* * * * *\"\n    text: x\n  - name: b\n    to: w\n    \
         text: x\n  - name: c\n    to: w\n    every: 10s\n    text: x\n  - name: d\n    to: w\n    every: 1h\n    tz: UTC\n    \
         text: x\n  - name: d\n    to: w\n    every: 1h\n    text: x\n  - name: e\n    to: w\n    cron: \"0 9 * * *\"\n    \
         tz: Mars/Olympus\n    text: x\n",
        &TimeZone::UTC,
    )
    .unwrap_err();
    for want in [
        "'a': only one of",
        "'b': say when",
        "'c': '10s' is shorter",
        "'d': 'tz' goes with",
        "'d': another entry",
        "'e': 'Mars/Olympus'",
    ] {
        assert!(e.contains(want), "missing {want:?} in:\n{e}");
    }
    assert!(settings::parse("entries:\n  - name: a\n    to: w\n    every: 1h\n    txt: x\n", &TimeZone::UTC).is_err());
    assert!(settings::parse("alert_after: 0\n", &TimeZone::UTC).is_err());
}

// --- the engine ---

fn engine(yaml: &str) -> Engine {
    Engine::new(State::default(), parse(yaml))
}

/// Tick at `now` and record every send as `outcome`; the names of what was sent.
fn tick(e: &mut Engine, now: Timestamp, outcome: Outcome) -> Vec<Why> {
    let sends = e.tick(now);
    for s in &sends {
        e.record(s, outcome, (outcome != Outcome::Sent).then(|| "why".to_string()), now);
    }
    sends.into_iter().map(|s| s.why).collect()
}

#[test]
fn a_new_entry_waits_a_full_interval_then_fires_on_time() {
    let start = t("2026-11-28T00:00:00Z");
    let mut e = engine(EVERY_3H);
    assert!(tick(&mut e, start, Outcome::Sent).is_empty(), "never fires the moment it's added");
    assert!(tick(&mut e, start + mins(179), Outcome::Sent).is_empty());
    let sends = e.tick(start + SignedDuration::from_secs(3 * 3600 + 4));
    assert_eq!(sends.len(), 1);
    let s = &sends[0];
    assert_eq!(s.to, "worker@server");
    assert_eq!(s.due, t("2026-11-28T03:00:00Z"));
    assert_eq!(s.why, Why::Scheduled { late: false, missed: 0 });
    assert_eq!(s.text, "From the scheduler entry \"worker check\" (every 3h):\n\nCheck the queue.");
    assert_eq!(e.state.entries["worker check"].in_flight, Some(s.due), "marked before it's sent");
    e.record(s, Outcome::Sent, None, start + mins(181));
    assert_eq!(e.state.entries["worker check"].in_flight, None);
    assert!(tick(&mut e, start + mins(185), Outcome::Sent).is_empty(), "once per fire");
    e.refresh(start + mins(185));
    assert_eq!(e.state.entries["worker check"].next_fire, Some(t("2026-11-28T06:00:00Z")));
}

#[test]
fn a_restart_neither_sends_twice_nor_skips() {
    let start = t("2026-11-28T00:00:00Z");
    let mut e = engine(EVERY_3H);
    tick(&mut e, start, Outcome::Sent);
    assert_eq!(tick(&mut e, start + mins(180), Outcome::Sent).len(), 1);
    // Saved and read back, as across a restart a minute later.
    let saved = serde_json::to_string(&e.state).unwrap();
    let mut e = Engine::new(serde_json::from_str(&saved).unwrap(), parse(EVERY_3H));
    assert!(tick(&mut e, start + mins(181), Outcome::Sent).is_empty());
    assert_eq!(tick(&mut e, start + mins(360), Outcome::Sent).len(), 1);
}

#[test]
fn a_stop_mid_send_is_never_sent_again() {
    let start = t("2026-11-28T00:00:00Z");
    let mut e = engine(EVERY_3H);
    tick(&mut e, start, Outcome::Sent);
    assert_eq!(e.tick(start + mins(180)).len(), 1);
    // Saved while in flight, then the process died.
    let saved = serde_json::to_string(&e.state).unwrap();
    let mut e = Engine::new(serde_json::from_str(&saved).unwrap(), parse(EVERY_3H));
    let st = &e.state.entries["worker check"];
    assert_eq!(st.in_flight, None);
    assert_eq!(st.last_fire.as_ref().unwrap().result, Outcome::Unknown);
    assert!(tick(&mut e, start + mins(181), Outcome::Sent).is_empty());
}

#[test]
fn after_downtime_one_late_message_stands_for_every_missed_fire() {
    let start = t("2026-11-28T00:00:00Z");
    let mut e = engine(EVERY_3H);
    tick(&mut e, start, Outcome::Sent);
    // Off from 00:00 to 10:00: fires at 03:00, 06:00 and 09:00 were missed.
    let sends = e.tick(t("2026-11-28T10:00:00Z"));
    assert_eq!(sends.len(), 1, "never a burst");
    assert_eq!(sends[0].due, t("2026-11-28T09:00:00Z"));
    assert_eq!(sends[0].why, Why::Scheduled { late: true, missed: 2 });
    assert!(sends[0].text.ends_with(
        "(Sent late: it was due at 2026-11-28T09:00:00Z, while the scheduler wasn't running; 2 earlier fires folded \
         into this one.)"
    ));
    e.record(&sends[0], Outcome::Sent, None, t("2026-11-28T10:00:00Z"));
    assert_eq!(e.state.entries["worker check"].missed, 2);
    // The schedule keeps its rhythm: next at 12:00, not 13:00.
    assert!(tick(&mut e, t("2026-11-28T11:59:00Z"), Outcome::Sent).is_empty());
    assert_eq!(tick(&mut e, t("2026-11-28T12:00:01Z"), Outcome::Sent).len(), 1);
}

#[test]
fn without_catch_up_missed_fires_are_dropped() {
    let yaml = format!("{EVERY_3H}    catch_up: false\n");
    let start = t("2026-11-28T00:00:00Z");
    let mut e = engine(&yaml);
    tick(&mut e, start, Outcome::Sent);
    assert!(tick(&mut e, t("2026-11-28T10:00:00Z"), Outcome::Sent).is_empty());
    let st = &e.state.entries["worker check"];
    assert_eq!(st.missed, 3);
    assert_eq!(st.last_fire.as_ref().unwrap().result, Outcome::Missed);
    assert_eq!(st.failures, 0, "missing fires while off isn't a delivery failure");
    assert_eq!(tick(&mut e, t("2026-11-28T12:00:01Z"), Outcome::Sent).len(), 1);
}

#[test]
fn a_busy_session_gets_the_message_once_it_can_take_it() {
    let start = t("2026-11-28T00:00:00Z");
    let mut e = engine(EVERY_3H);
    tick(&mut e, start, Outcome::Sent);
    let fire = start + mins(180);
    assert_eq!(tick(&mut e, fire, Outcome::Waiting).len(), 1);
    assert_eq!(e.state.entries["worker check"].last_fire.as_ref().unwrap().result, Outcome::Waiting);
    assert!(tick(&mut e, fire + SignedDuration::from_secs(30), Outcome::Sent).is_empty(), "a minute between tries");
    assert_eq!(tick(&mut e, fire + mins(1), Outcome::Waiting), vec![Why::Retry]);
    let sends = e.tick(fire + mins(2));
    assert_eq!(sends[0].why, Why::Retry);
    assert_eq!(sends[0].due, fire, "still the 03:00 fire");
    assert!(sends[0].text.contains("held until the session could take it"));
    e.record(&sends[0], Outcome::Sent, None, fire + mins(2));
    let st = &e.state.entries["worker check"];
    assert_eq!(st.pending, None);
    assert_eq!(st.failures, 0);
    assert!(tick(&mut e, fire + mins(3), Outcome::Sent).is_empty());
}

#[test]
fn a_session_busy_until_the_next_fire_counts_as_a_failure() {
    let start = t("2026-11-28T00:00:00Z");
    let mut e = engine(EVERY_3H);
    tick(&mut e, start, Outcome::Sent);
    tick(&mut e, start + mins(180), Outcome::Waiting);
    // Still busy at every retry, until 06:00 replaces it with one message, not two.
    for m in 1..180 {
        tick(&mut e, start + mins(180 + m), Outcome::Waiting);
    }
    let sends = e.tick(start + mins(360));
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].due, t("2026-11-28T06:00:00Z"));
    assert_eq!(e.state.entries["worker check"].failures, 1);
}

#[test]
fn if_busy_skip_waits_for_the_next_fire() {
    let yaml = format!("{EVERY_3H}    if_busy: skip\n");
    let start = t("2026-11-28T00:00:00Z");
    let mut e = engine(&yaml);
    tick(&mut e, start, Outcome::Sent);
    tick(&mut e, start + mins(180), Outcome::Waiting);
    let st = &e.state.entries["worker check"];
    assert_eq!(st.last_fire.as_ref().unwrap().result, Outcome::SkippedBusy);
    assert_eq!(st.failures, 0);
    assert!(tick(&mut e, start + mins(182), Outcome::Sent).is_empty());
}

#[test]
fn repeated_failures_turn_the_status_bad_until_one_gets_through() {
    let start = t("2026-11-28T00:00:00Z");
    let mut e = engine(EVERY_3H);
    tick(&mut e, start, Outcome::Sent);
    tick(&mut e, start + mins(180), Outcome::Offline);
    assert_eq!(e.problem(start + mins(181)), None, "one failed fire is allowed (alert_after: 2)");
    tick(&mut e, start + mins(360), Outcome::Refused);
    assert_eq!(tick(&mut e, start + mins(362), Outcome::Sent).len(), 0, "a refusal waits for the next fire");
    let p = e.problem(start + mins(362)).unwrap();
    assert_eq!(p, "'worker check': the last 2 fires to worker@server weren't delivered (refused: why)");
    tick(&mut e, start + mins(540), Outcome::Sent);
    assert_eq!(e.problem(start + mins(541)), None);
}

#[test]
fn a_stuck_loop_and_a_broken_settings_file_are_problems() {
    let start = t("2026-11-28T00:00:00Z");
    let mut e = engine(EVERY_3H);
    e.last_tick = Some(start);
    assert_eq!(e.problem(start + mins(2)), None);
    assert_eq!(e.problem(start + mins(4)).unwrap(), "the scheduler has been stuck since 2026-11-28T00:00:00Z");
    e.settings_problem = Some("'a': say when\n'b': …".into());
    assert_eq!(
        e.problem(start + mins(4)).unwrap(),
        "the scheduler has been stuck since 2026-11-28T00:00:00Z (and 1 more)"
    );
    e.last_tick = Some(start + mins(4));
    assert_eq!(
        e.problem(start + mins(4)).unwrap(),
        "its settings file can't be read, so the last good entries run: 'a': say when"
    );
}

#[test]
fn turning_an_entry_off_and_on_starts_it_afresh() {
    let start = t("2026-11-28T00:00:00Z");
    let mut e = engine(EVERY_3H);
    tick(&mut e, start, Outcome::Sent);
    e.settings = parse(&format!("{EVERY_3H}    enabled: false\n"));
    assert!(tick(&mut e, start + mins(600), Outcome::Sent).is_empty());
    e.settings = parse(EVERY_3H);
    assert!(tick(&mut e, start + mins(601), Outcome::Sent).is_empty(), "no catch-up for time it was off");
    assert_eq!(tick(&mut e, start + mins(780), Outcome::Sent).len(), 1);
}

#[test]
fn a_removed_entry_is_forgotten() {
    let start = t("2026-11-28T00:00:00Z");
    let mut e = engine(EVERY_3H);
    tick(&mut e, start, Outcome::Sent);
    e.settings = parse("entries: []\n");
    tick(&mut e, start + mins(1), Outcome::Sent);
    assert!(e.state.entries.is_empty());
}

#[test]
fn a_one_off_fires_once_and_never_for_a_time_before_it_was_added() {
    let yaml = "entries:\n  - name: remind\n    to: planning\n    at: 2026-11-28T15:00:00Z\n    text: Look at the PR.\n  - name: \
                old\n    to: planning\n    at: 2026-11-28T09:00:00Z\n    text: Too late.\n";
    let mut e = engine(yaml);
    assert!(tick(&mut e, t("2026-11-28T10:00:00Z"), Outcome::Sent).is_empty());
    let sends = e.tick(t("2026-11-28T15:00:03Z"));
    assert_eq!(sends.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["remind"]);
    assert!(sends[0].text.starts_with("From the scheduler entry \"remind\" (at 2026-11-28T15:00:00Z):"));
    e.record(&sends[0], Outcome::Sent, None, t("2026-11-28T15:00:03Z"));
    assert!(tick(&mut e, t("2026-11-29T15:00:03Z"), Outcome::Sent).is_empty());
    e.refresh(t("2026-11-29T15:00:03Z"));
    assert_eq!(e.state.entries["remind"].next_fire, None);
}

#[test]
fn firing_by_hand_leaves_the_schedule_alone() {
    let start = t("2026-11-28T00:00:00Z");
    let mut e = engine(EVERY_3H);
    tick(&mut e, start, Outcome::Sent);
    let s = e.fire_now("worker check", "r1", start + mins(10)).unwrap();
    assert!(s.text.starts_with("From the scheduler entry \"worker check\" (every 3h, fired by hand):"));
    e.record(&s, Outcome::Offline, Some("offline".into()), start + mins(10));
    assert_eq!(e.state.manual[0].id, "r1");
    assert_eq!(e.state.manual[0].result, Outcome::Offline);
    assert_eq!(e.state.entries["worker check"].failures, 0, "a test fire isn't a scheduled failure");
    assert_eq!(tick(&mut e, start + mins(180), Outcome::Sent).len(), 1);
    assert!(e.fire_now("nope", "r2", start).is_err());
}

#[test]
fn the_node_answers_map_to_outcomes() {
    let err = |c, m: &str| Err(RpcError::new(c, m));
    assert_eq!(classify(&Ok(json!({"via": "harness_channel"}))).0, Outcome::Sent);
    assert_eq!(classify(&err(code::BUSY, "waiting on a person")).0, Outcome::Waiting);
    assert_eq!(classify(&err(code::NOT_HERE, "offline")).0, Outcome::Offline);
    assert_eq!(classify(&err(code::NO_SUCH, "no session")).0, Outcome::Offline);
    let (o, why) = classify(&err(code::REFUSED, "message.send isn't granted"));
    assert_eq!((o, why.as_deref()), (Outcome::Refused, Some("message.send isn't granted")));
    assert_eq!(classify(&err(code::INTERNAL_ERROR, "didn't answer within 30s")).0, Outcome::Unknown);
}

#[test]
fn the_status_detail_stays_under_the_node_limit() {
    let mut yaml = "entries:\n".to_string();
    for i in 0..60 {
        yaml.push_str(&format!("  - name: entry number {i}\n    to: worker@server\n    every: 1h\n    text: x\n"));
    }
    let mut e = engine(&yaml);
    e.tick(t("2026-11-28T00:00:00Z"));
    let d = e.detail();
    assert!(d.to_string().len() < 4096);
    assert_eq!(d["entryCount"], 60);
    let mut small = engine(EVERY_3H);
    small.tick(t("2026-11-28T00:00:00Z"));
    small.refresh(t("2026-11-28T00:00:00Z"));
    assert_eq!(small.detail()["entries"]["worker check"]["next"], "2026-11-28T03:00:00Z");
}

// --- the runner, against a fake node ---

#[derive(Default)]
struct FakeNode {
    sent: StdMutex<Vec<(String, String)>>,
    statuses: StdMutex<Vec<Option<String>>>,
    answer: StdMutex<Option<(i64, String)>>,
}

#[async_trait]
impl crate::runner::Node for FakeNode {
    async fn send(&self, to: &str, text: &str) -> Result<Value, RpcError> {
        self.sent.lock().unwrap().push((to.into(), text.into()));
        match self.answer.lock().unwrap().clone() {
            Some((c, m)) => Err(RpcError::new(c, m)),
            None => Ok(json!({"via": "harness_channel"})),
        }
    }
    fn status(&self, problem: Option<String>, _detail: Value) {
        self.statuses.lock().unwrap().push(problem);
    }
}

fn runner(dir: &std::path::Path) -> Runner {
    let paths = Paths { settings: dir.join("scheduler.yaml"), state_dir: dir.join("state") };
    Runner::open(paths, TimeZone::UTC).unwrap()
}

#[tokio::test]
async fn the_runner_saves_state_reloads_settings_and_takes_fire_requests() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("scheduler.yaml"), EVERY_3H).unwrap();
    let node = FakeNode::default();
    let start = t("2026-11-28T00:00:00Z");
    let r = runner(dir.path());
    r.step(start, &node).await;
    r.step(start + mins(180), &node).await;
    assert_eq!(node.sent.lock().unwrap().len(), 1);
    let saved: State = serde_json::from_slice(&std::fs::read(dir.path().join("state/state.json")).unwrap()).unwrap();
    assert_eq!(saved.entries["worker check"].last_sent_at, Some(start + mins(180)));
    assert_eq!(saved.entries["worker check"].schedule.as_deref(), Some("every 3h"));
    assert_eq!(*node.statuses.lock().unwrap().last().unwrap(), None);

    // Reopened (a restart): nothing is sent again.
    let r = runner(dir.path());
    r.step(start + mins(181), &node).await;
    assert_eq!(node.sent.lock().unwrap().len(), 1);

    // A request from `fleetlayer-scheduler fire`.
    let req = FireRequest { id: "abc".into(), name: "worker check".into() };
    std::fs::write(dir.path().join("state/fire/abc.json"), serde_json::to_vec(&req).unwrap()).unwrap();
    r.step(start + mins(182), &node).await;
    assert_eq!(node.sent.lock().unwrap().len(), 2);
    assert!(std::fs::read_dir(dir.path().join("state/fire")).unwrap().next().is_none(), "the request is taken");
    let saved: State = serde_json::from_slice(&std::fs::read(dir.path().join("state/state.json")).unwrap()).unwrap();
    assert_eq!(saved.manual[0].result, Outcome::Sent);

    // A broken edit keeps the last good entries and says so. Its modification time is the same as the last good
    // file's: the content is what counts.
    let mtime = std::fs::metadata(dir.path().join("scheduler.yaml")).unwrap().modified().unwrap();
    std::fs::write(dir.path().join("scheduler.yaml"), "entries:\n  - name: x\n").unwrap();
    let f = std::fs::File::options().append(true).open(dir.path().join("scheduler.yaml")).unwrap();
    f.set_modified(mtime).unwrap();
    *node.answer.lock().unwrap() = Some((code::NOT_HERE, "worker isn't running".into()));
    r.step(start + mins(360), &node).await;
    assert_eq!(node.sent.lock().unwrap().len(), 3, "the old entry still fires");
    let problem = node.statuses.lock().unwrap().last().unwrap().clone().unwrap();
    assert!(problem.starts_with("its settings file can't be read, so the last good entries run"), "{problem}");
}
