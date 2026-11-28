//! The presence authority: one per fleet, a plugin on an always-on node. It keeps what people said and planned in
//! its state directory, hears their desks through `presence.desk` readings, decides each person's state every few
//! seconds (`engine.rs`), and tells the fleet: a `presence` reading per person, an event on the `presence` channel when
//! a state changes and holds, and the plugin's status. Callers reach it on a Unix socket only its user can open: the
//! tool its node shares (`serve.rs`) passes each call on with the caller the control plane recorded.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fleetlayer_protocol::ext::{Event, Reading};
use fleetlayer_protocol::rpc::RpcError;
use jiff::civil::{Date, DateTime, Time};
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::engine::{self, Desk, Device, Look, Plan, Seen, State, Stated};
use crate::settings::{self, Person, Settings, parse_duration};

/// How often the authority decides again.
pub const ROUND: Duration = Duration::from_secs(15);
/// A new state is announced (an event) once it has held this long, so a moment's flicker wakes nobody.
pub const SETTLE: SignedDuration = SignedDuration::from_secs(60);
/// The `presence` reading is sent again this often even when nothing changed, so a reader can tell the authority is
/// alive from its `at`.
pub const REFRESH: SignedDuration = SignedDuration::from_mins(5);
/// A stated away with no end lasts at most this long.
pub const STATED_MAX: SignedDuration = SignedDuration::from_hours(24);
/// The furthest ahead a stated status or a plan may reach.
const AHEAD_MAX: SignedDuration = SignedDuration::from_hours(24 * 366 * 2);
const PLANS_MAX: usize = 50;
/// Events that keep failing this long are a problem.
const EVENTS_FAILING: SignedDuration = SignedDuration::from_mins(10);

/// What the authority asks of its node.
#[async_trait]
pub trait Node: Send + Sync {
    async fn publish(&self, event: Event) -> Result<(), RpcError>;
    fn readings(&self, readings: Vec<Reading>);
    fn status(&self, problem: Option<String>, detail: Value);
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub settings: PathBuf,
    pub state_dir: PathBuf,
}

impl Paths {
    pub fn socket(&self) -> PathBuf {
        self.state_dir.join("presence.sock")
    }
    fn state(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Current {
    pub state: State,
    pub since: Timestamp,
}

/// One person's kept state.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Kept {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stated: Option<Stated>,
    #[serde(default)]
    pub plans: Vec<Plan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen: Option<Seen>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<Current>,
    /// The state the last event announced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub announced: Option<State>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Saved {
    #[serde(default)]
    people: BTreeMap<String, Kept>,
    #[serde(default)]
    next_plan: u64,
}

struct Inner {
    settings: Settings,
    settings_problem: Option<String>,
    settings_stamp: Option<std::time::SystemTime>,
    saved: Saved,
    /// The latest desk report per (person, node).
    desks: BTreeMap<(String, String), Desk>,
    /// Desk reports from nodes that aren't a desk of that person, logged once each.
    ignored: BTreeSet<(String, String)>,
    refreshed: BTreeMap<String, Timestamp>,
    save_problem: Option<String>,
    events_failing: Option<(Timestamp, String)>,
    subscribe_problem: Option<String>,
    last_round: Option<Timestamp>,
}

pub struct Authority {
    paths: Paths,
    node: Arc<dyn Node>,
    inner: Mutex<Inner>,
    /// Rounds that announce; a call's own decision only updates the reading.
    announcing: Mutex<()>,
}

impl Authority {
    pub fn open(paths: Paths, node: Arc<dyn Node>) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(&paths.state_dir)?;
        let saved = match std::fs::read_to_string(paths.state()) {
            Ok(t) => serde_json::from_str(&t).map_err(|e| anyhow::anyhow!("{}: {e}", paths.state().display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Saved::default(),
            Err(e) => return Err(e.into()),
        };
        let me = Arc::new(Self {
            paths,
            node,
            inner: Mutex::new(Inner {
                settings: Settings::default(),
                settings_problem: None,
                settings_stamp: None,
                saved,
                desks: BTreeMap::new(),
                ignored: BTreeSet::new(),
                refreshed: BTreeMap::new(),
                save_problem: None,
                events_failing: None,
                subscribe_problem: None,
                last_round: None,
            }),
            announcing: Mutex::new(()),
        });
        // Read the settings now: desk reports arrive before the first round, and are kept only for known desks.
        if let Ok(mut i) = me.inner.try_lock() {
            me.reload(&mut i);
        }
        Ok(me)
    }

    pub async fn set_subscribe_problem(&self, p: Option<String>) {
        self.inner.lock().await.subscribe_problem = p;
    }

    pub async fn last_round(&self) -> Option<Timestamp> {
        self.inner.lock().await.last_round
    }

    /// Read the settings again when the file changed (or on the first round).
    fn reload(&self, i: &mut Inner) {
        let stamp = std::fs::metadata(&self.paths.settings).and_then(|m| m.modified()).ok();
        if stamp == i.settings_stamp && (i.last_round.is_some() || !i.settings.people.is_empty()) {
            return;
        }
        i.settings_stamp = stamp;
        let text = match std::fs::read_to_string(&self.paths.settings) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                i.settings_problem = Some(format!("can't read {}: {e}", self.paths.settings.display()));
                return;
            }
        };
        match settings::parse(&text) {
            Ok(s) => {
                if s.people.is_empty() {
                    i.settings_problem = Some(format!(
                        "no people are set up in {}: add one (docs/plugins/presence.md)",
                        self.paths.settings.display()
                    ));
                } else {
                    i.settings_problem = None;
                }
                tracing::info!(people = s.people.len(), "presence settings read");
                i.settings = s;
            }
            Err(e) => {
                // Keep the last good settings running.
                tracing::warn!("presence settings: {e}");
                i.settings_problem = Some(format!(
                    "{} has mistakes (the last good settings still run): {}",
                    self.paths.settings.display(),
                    e.lines().next().unwrap_or("")
                ));
            }
        }
    }

    /// Desk reports from the fleet (`presence.desk` readings): kept only from a node that's one of that person's
    /// desks, and only if newer than what's kept.
    pub async fn desk_readings(&self, readings: &[Reading]) {
        let mut i = self.inner.lock().await;
        for r in readings.iter().filter(|r| r.kind == crate::DESK_KIND) {
            let Some(node) = r.node.clone() else { continue };
            let person = r.key.clone();
            let is_desk = i.settings.people.get(&person).is_some_and(|p| p.desks.contains(&node));
            if !is_desk {
                if i.ignored.insert((person.clone(), node.clone())) {
                    tracing::info!(
                        person,
                        node,
                        "ignoring desk reports from a node that isn't one of that person's desks"
                    );
                }
                continue;
            }
            let desk = Desk {
                node: node.clone(),
                locked: r.data["locked"].as_bool(),
                idle_seconds: r.data["idleSeconds"].as_u64(),
                at: r.at.min(Timestamp::now()),
            };
            let key = (person, node);
            if i.desks.get(&key).is_none_or(|d| d.at <= desk.at) {
                i.desks.insert(key, desk);
            }
        }
    }

    /// One round: read settings, decide every person, save, tell the fleet.
    pub async fn round(&self) {
        self.round_at(Timestamp::now()).await;
    }

    pub async fn round_at(&self, now: Timestamp) {
        let _one = self.announcing.lock().await;
        let (readings, events) = {
            let mut i = self.inner.lock().await;
            self.reload(&mut i);
            let (readings, events) = self.decide_all(&mut i, now, true);
            i.last_round = Some(now);
            (readings, events)
        };
        self.node.readings(readings);
        for (person, ev) in events {
            let state = ev.body["state"].clone();
            match self.node.publish(ev).await {
                Ok(()) => {
                    let mut i = self.inner.lock().await;
                    i.events_failing = None;
                    if let Ok(s) = serde_json::from_value::<State>(state)
                        && let Some(k) = i.saved.people.get_mut(&person)
                    {
                        k.announced = Some(s);
                        self.save(&mut i);
                    }
                }
                Err(e) => {
                    tracing::warn!(person, "publishing a presence event: {}", e.message);
                    let mut i = self.inner.lock().await;
                    if i.events_failing.is_none() {
                        i.events_failing = Some((now, e.message.clone()));
                    }
                }
            }
        }
        self.report().await;
    }

    /// Decide every person now. Answers the readings to send and the events to publish (when `announce`).
    fn decide_all(&self, i: &mut Inner, now: Timestamp, announce: bool) -> (Vec<Reading>, Vec<(String, Event)>) {
        let mut readings = vec![];
        let mut events = vec![];
        let mut changed = false;
        let people: Vec<Person> = i.settings.people.values().cloned().collect();
        for p in &people {
            let before = i.saved.people.get(&p.name).cloned().unwrap_or_default();
            let (view, kept) = decide(p, &before, &desks_of(i, &p.name), now);
            if kept != before {
                changed = true;
            }
            let state_changed = before.current.as_ref().map(|c| c.state) != Some(kept.current.as_ref().unwrap().state);
            let due = i.refreshed.get(&p.name).is_none_or(|t| now.duration_since(*t) >= REFRESH);
            if state_changed || due || kept.stated != before.stated || kept.plans != before.plans {
                readings.push(Reading {
                    kind: crate::KIND.into(),
                    key: p.name.clone(),
                    at: now,
                    node: None,
                    data: view.clone(),
                });
                i.refreshed.insert(p.name.clone(), now);
            }
            let cur = kept.current.clone().expect("decided");
            if announce && kept.announced != Some(cur.state) && now.duration_since(cur.since) >= SETTLE {
                let from = kept.announced.map_or("unknown", State::words);
                events.push((
                    p.name.clone(),
                    Event {
                        channel: crate::CHANNEL.into(),
                        kind: "presence.changed".into(),
                        subject: Some(format!("person:{}", p.name)),
                        at: cur.since,
                        dedupe_key: Some(format!(
                            "presence:{}:{}:{}",
                            p.name,
                            cur.state.words(),
                            cur.since.as_millisecond()
                        )),
                        summary: format!(
                            "{} is {} (was {from}): {}",
                            p.name,
                            cur.state.words(),
                            view["why"].as_str().unwrap_or("")
                        ),
                        body: json!({"person": p.name, "state": cur.state, "from": kept.announced, "since": cur.since,
                            "until": view["until"], "note": view["note"], "why": view["why"]}),
                    },
                ));
            }
            i.saved.people.insert(p.name.clone(), kept);
        }
        if changed {
            self.save(i);
        }
        (readings, events)
    }

    fn save(&self, i: &mut Inner) {
        let tmp = self.paths.state().with_extension("json.tmp");
        let r = serde_json::to_vec_pretty(&i.saved)
            .map_err(std::io::Error::other)
            .and_then(|b| std::fs::write(&tmp, b))
            .and_then(|()| std::fs::rename(&tmp, self.paths.state()));
        i.save_problem = r.err().map(|e| format!("can't save {}: {e}", self.paths.state().display()));
    }

    async fn report(&self) {
        let i = self.inner.lock().await;
        let now = Timestamp::now();
        let mut problems = vec![];
        problems.extend(i.settings_problem.clone());
        problems.extend(i.save_problem.clone());
        problems.extend(i.subscribe_problem.clone());
        if let Some((since, e)) = &i.events_failing
            && now.duration_since(*since) >= EVENTS_FAILING
        {
            problems.push(format!(
                "presence events have failed since {}: {e}",
                since.round(jiff::Unit::Second).unwrap_or(*since)
            ));
        }
        let people: Value = i
            .saved
            .people
            .iter()
            .filter(|(n, _)| i.settings.people.contains_key(*n))
            .map(|(n, k)| (n.clone(), json!(k.current)))
            .collect::<serde_json::Map<_, _>>()
            .into();
        let desks: Vec<Value> =
            i.desks.iter().map(|((person, node), d)| json!({"person": person, "node": node, "at": d.at})).collect();
        let problem = (!problems.is_empty()).then(|| problems.join("; "));
        self.node.status(problem, json!({"lastRound": now, "people": people, "desks": desks}));
    }

    /// A caller's request: `op` with `args`, from `caller` (the control plane's record of who called; None for the
    /// authority's own admin commands).
    pub async fn handle(&self, op: &str, caller: Option<&str>, args: &Value) -> Result<Value, String> {
        let by = caller.unwrap_or("the authority's admin").to_string();
        let now = Timestamp::now();
        let mut i = self.inner.lock().await;
        if i.last_round.is_none() {
            self.reload(&mut i);
        }
        let named = args["person"].as_str();
        if op == "get" {
            let people: Vec<Person> = match named {
                Some(_) => vec![i.settings.person(named)?.clone()],
                None => i.settings.people.values().cloned().collect(),
            };
            if people.is_empty() {
                return Err("no people are set up in the presence settings yet".into());
            }
            let views: Vec<Value> = people
                .iter()
                .map(|p| {
                    decide(p, &i.saved.people.get(&p.name).cloned().unwrap_or_default(), &desks_of(&i, &p.name), now).0
                })
                .collect();
            return Ok(json!({"people": views}));
        }
        let p = i.settings.person(named)?.clone();
        let mut kept = i.saved.people.get(&p.name).cloned().unwrap_or_default();
        let answer = match op {
            "set" => {
                let state = args["state"].as_str().unwrap_or("");
                let note = note(&args["note"])?;
                match state {
                    "back" => {
                        kept.stated = None;
                        kept.seen =
                            Some(Seen { at: now, via: "said back".into(), device: Device::Other, by: by.clone() });
                        json!({"person": p.name, "cleared": true})
                    }
                    "away" | "asleep" => {
                        let until = match (args["for"].as_str(), args["until"].as_str()) {
                            (Some(_), Some(_)) => return Err("give --for or --until, not both".into()),
                            (Some(f), None) => {
                                let d = parse_duration(f)?;
                                Some(now + d)
                            }
                            (None, Some(u)) => Some(when(&p, u, now, Edge::Until)?),
                            (None, None) if state == "asleep" => {
                                engine::night_end(&p, now).or_else(|| next_morning(&p, now))
                            }
                            (None, None) => None,
                        };
                        if let Some(u) = until {
                            if u <= now {
                                return Err(format!("{} has already passed", engine::local(&p, u, now)));
                            }
                            if u.duration_since(now) > SignedDuration::from_hours(24 * 31) {
                                return Err(
                                    "a stated status lasts at most 31 days: plan a longer absence with `plan`".into()
                                );
                            }
                        }
                        let s = if state == "away" { State::Away } else { State::Asleep };
                        let mut stated = Stated { state: s, note, set_at: now, until, by: by.clone(), left: false };
                        let desks = desks_of(&i, &p.name);
                        let look =
                            Look { person: &p, desks: &desks, seen: None, stated: Some(&stated), plans: &[], now };
                        stated.left = engine::desks_show_gone(&look);
                        kept.stated = Some(stated.clone());
                        json!({"person": p.name, "stated": stated})
                    }
                    other => return Err(format!("state {other:?}: away, asleep or back")),
                }
            }
            "plan_add" => {
                let from = args["from"].as_str().ok_or("from: when the absence starts")?;
                let to = args["to"].as_str().ok_or("to: when it ends")?;
                let from = when(&p, from, now, Edge::Start)?;
                let to = when(&p, to, now, Edge::End)?;
                if to <= from {
                    return Err("the absence ends before it starts".into());
                }
                if to <= now {
                    return Err("that absence has already ended".into());
                }
                if to.duration_since(now) > AHEAD_MAX {
                    return Err("plans reach at most two years ahead".into());
                }
                if kept.plans.len() >= PLANS_MAX {
                    return Err(format!("{} has {PLANS_MAX} planned absences already: remove some first", p.name));
                }
                i.saved.next_plan += 1;
                let plan =
                    Plan { id: i.saved.next_plan, from, to, note: note(&args["note"])?, by: by.clone(), added_at: now };
                kept.plans.push(plan.clone());
                kept.plans.sort_by_key(|x| x.from);
                json!({"person": p.name, "plan": plan})
            }
            "plan_remove" => {
                let id = args["id"].as_u64().ok_or("id: the plan's number (`fleetlayer presence` lists them)")?;
                let before = kept.plans.len();
                kept.plans.retain(|x| x.id != id);
                if kept.plans.len() == before {
                    return Err(format!("{} has no planned absence #{id}", p.name));
                }
                json!({"person": p.name, "removed": id})
            }
            "seen" => {
                let via = args["via"].as_str().unwrap_or("");
                if via.is_empty()
                    || via.len() > 40
                    || !via.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
                {
                    return Err("via: what they looked at, 1 to 40 letters, digits and - _ . (quietdeck)".into());
                }
                let device = match args["device"].as_str().unwrap_or("other") {
                    "phone" => Device::Phone,
                    "desk" => Device::Desk,
                    "other" => Device::Other,
                    d => return Err(format!("device {d:?}: phone, desk or other")),
                };
                kept.seen = Some(Seen { at: now, via: via.into(), device, by: by.clone() });
                json!({"person": p.name, "seen": kept.seen})
            }
            other => return Err(format!("no operation called {other}")),
        };
        if op != "seen" {
            tracing::info!(person = p.name, op, by, "{answer}");
        }
        i.saved.people.insert(p.name.clone(), kept);
        self.save(&mut i);
        // Decide again at once so the next `get` and the reading agree with what was just said; the round
        // announces it.
        let (readings, _) = self.decide_all(&mut i, now, false);
        let view = decide(&p, i.saved.people.get(&p.name).expect("kept"), &desks_of(&i, &p.name), now).0;
        drop(i);
        self.node.readings(readings);
        let mut answer = answer;
        answer["now"] = view;
        Ok(answer)
    }

    pub async fn serve(self: Arc<Self>, listener: tokio::net::UnixListener) {
        loop {
            let Ok((stream, _)) = listener.accept().await else { continue };
            let me = self.clone();
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut lines = BufReader::new(r).lines();
                let Ok(Some(line)) = lines.next_line().await else { return };
                let answer = match serde_json::from_str::<Value>(&line) {
                    Ok(v) => match me.handle(v["op"].as_str().unwrap_or(""), v["caller"].as_str(), &v["args"]).await {
                        Ok(r) => json!({"result": r}),
                        Err(e) => json!({"error": e}),
                    },
                    Err(e) => json!({"error": format!("not JSON: {e}")}),
                };
                let _ = w.write_all((answer.to_string() + "\n").as_bytes()).await;
            });
        }
    }
}

fn desks_of(i: &Inner, person: &str) -> Vec<Desk> {
    i.desks.iter().filter(|((p, _), _)| p == person).map(|(_, d)| d.clone()).collect()
}

/// Decide one person: what they look like now (the view readers get), and what to keep.
pub fn decide(p: &Person, before: &Kept, desks: &[Desk], now: Timestamp) -> (Value, Kept) {
    let mut kept = before.clone();
    // What has run out.
    if let Some(s) = &kept.stated {
        let ended = match s.until {
            Some(u) => u <= now,
            None => now.duration_since(s.set_at) >= STATED_MAX,
        };
        if ended {
            kept.stated = None;
        }
    }
    kept.plans.retain(|x| now.duration_since(x.to) < SignedDuration::from_hours(24 * 7));
    // A stated away: once the desks show them gone, input at a desk means they're back.
    if let Some(s) = kept.stated.clone()
        && !s.left
    {
        let look = Look { person: p, desks, seen: None, stated: Some(&s), plans: &[], now };
        if engine::desks_show_gone(&look) {
            kept.stated.as_mut().unwrap().left = true;
        }
    }
    let look =
        Look { person: p, desks, seen: kept.seen.as_ref(), stated: kept.stated.as_ref(), plans: &kept.plans, now };
    let mut verdict = engine::judge(&look);
    if verdict.back_at_desk {
        kept.stated = None;
        verdict.back_at_desk = false;
    }
    let since = match &kept.current {
        Some(c) if c.state == verdict.state => c.since,
        _ => now,
    };
    kept.current = Some(Current { state: verdict.state, since });
    let upcoming: Vec<&Plan> = kept.plans.iter().filter(|x| x.to > now).collect();
    let local_now = now.to_zoned_datetime(p.tz.clone());
    let night = p.night.map(|(a, b)| format!("{}-{}", a.strftime("%H:%M"), b.strftime("%H:%M")));
    let desk_list: Vec<Value> = p
        .desks
        .iter()
        .map(|node| match desks.iter().find(|d| &d.node == node) {
            Some(d) => json!({"node": node, "locked": d.locked, "idleSeconds": d.idle_seconds, "at": d.at,
                "fresh": now.duration_since(d.at) <= engine::DESK_FRESH}),
            None => json!({"node": node, "fresh": false}),
        })
        .collect();
    let mut line = format!("{} is {} since {}", p.name, verdict.state.words(), engine::local(p, since, now));
    if let Some(u) = verdict.until {
        line.push_str(&format!(", until {}", engine::local(p, u, now)));
    }
    line.push_str(&format!(": {}. Their time: {} ({}).", verdict.why, local_now.strftime("%a %H:%M"), p.tz_name));
    if let Some(next) = upcoming.iter().find(|x| x.from > now) {
        let note = next.note.as_ref().map(|n| format!(" ({n})")).unwrap_or_default();
        line.push_str(&format!(
            " Next planned absence{note}: {} to {}.",
            engine::local(p, next.from, now),
            engine::local(p, next.to, now)
        ));
    }
    let view = json!({
        "person": p.name,
        "state": verdict.state,
        "since": since,
        "until": verdict.until,
        "note": verdict.note,
        "why": verdict.why,
        "line": line,
        "at": now,
        "timezone": p.tz_name,
        "localTime": local_now.strftime("%Y-%m-%dT%H:%M:%S%:z").to_string(),
        "night": night,
        "desks": desk_list,
        "seen": kept.seen,
        "stated": kept.stated,
        "plans": upcoming,
    });
    (view, kept)
}

#[derive(Clone, Copy, PartialEq)]
pub enum Edge {
    /// The start of a plan: a bare date is the start of that day.
    Start,
    /// The end of a plan: a bare date is the end of that day (the absence includes it).
    End,
    /// The end of a stated status: a bare time is its next occurrence.
    Until,
}

/// A time a person wrote, in their time zone: `14:00`, `2026-12-20`, `2026-12-20 14:00`, or an RFC 3339 timestamp.
pub fn when(p: &Person, s: &str, now: Timestamp, edge: Edge) -> Result<Timestamp, String> {
    let s = s.trim();
    let bad = || format!("{s:?}: write a time as 14:00, 2026-12-20, 2026-12-20 14:00 or an RFC 3339 timestamp");
    if let Ok(t) = s.parse::<Timestamp>() {
        return Ok(t);
    }
    let zone = |dt: DateTime| dt.to_zoned_datetime(p.tz.clone()).map(|z| z.timestamp()).map_err(|e| e.to_string());
    if let Ok(dt) = s.replace(' ', "T").parse::<DateTime>()
        && s.len() > 10
    {
        return zone(dt);
    }
    if let Ok(d) = s.parse::<Date>() {
        let d = if edge == Edge::End { d.tomorrow().map_err(|e| e.to_string())? } else { d };
        return zone(d.to_datetime(Time::midnight()));
    }
    if let Ok(t) = s.parse::<Time>() {
        let today = now.to_zoned_datetime(p.tz.clone()).date();
        let at = zone(today.to_datetime(t))?;
        return if at > now { Ok(at) } else { zone(today.tomorrow().map_err(|e| e.to_string())?.to_datetime(t)) };
    }
    Err(bad())
}

/// The end of the coming night, when a person goes to bed outside it (or has none): 8 am their time.
fn next_morning(p: &Person, now: Timestamp) -> Option<Timestamp> {
    let end = p.night.map_or(Time::constant(8, 0, 0, 0), |(_, b)| b);
    when(p, &end.strftime("%H:%M").to_string(), now, Edge::Until).ok()
}

/// A note as one plain line of at most 200 characters.
fn note(v: &Value) -> Result<Option<String>, String> {
    let Some(s) = v.as_str() else { return Ok(None) };
    let s: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > 200 {
        return Err("a note is at most 200 characters".into());
    }
    Ok((!s.is_empty()).then_some(s))
}

/// Ask a running authority on this machine (the admin commands, and the tool's server).
pub async fn ask(socket: &Path, op: &str, caller: Option<&str>, args: Value) -> anyhow::Result<Value> {
    let stream = tokio::net::UnixStream::connect(socket).await.map_err(|e| {
        anyhow::anyhow!("can't reach the presence authority at {} ({e}): is its plugin running here?", socket.display())
    })?;
    let (r, mut w) = stream.into_split();
    w.write_all((json!({"op": op, "caller": caller, "args": args}).to_string() + "\n").as_bytes()).await?;
    let mut lines = BufReader::new(r).lines();
    let line =
        lines.next_line().await?.ok_or_else(|| anyhow::anyhow!("the presence authority closed the connection"))?;
    let v: Value = serde_json::from_str(&line)?;
    if let Some(e) = v["error"].as_str() {
        anyhow::bail!("{e}");
    }
    Ok(v["result"].clone())
}
