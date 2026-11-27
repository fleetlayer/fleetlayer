//! What fires when, what happened, and whether the scheduler does its job. Pure: the caller passes the time in, sends
//! the messages [`Engine::tick`] hands back, and reports each outcome with [`Engine::record`]. The state it keeps
//! (`state.json` in the state directory) is what makes a restart neither send twice nor skip.

use std::collections::BTreeMap;

use fleetlayer_protocol::rpc::{RpcError, code};
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::settings::{Entry, IfBusy, Settings};

/// A fire handled within this long of its time is on time; later, it's late (the scheduler wasn't running).
pub const GRACE: SignedDuration = SignedDuration::from_secs(120);
/// How often a message waiting on a busy session is tried again.
pub const RETRY_BUSY: SignedDuration = SignedDuration::from_secs(60);
/// A tick loop that hasn't finished a tick for this long is stuck.
pub const STUCK_AFTER: SignedDuration = SignedDuration::from_secs(180);
/// Fired-by-hand results kept for `fleetlayer-scheduler fire` to read.
const MANUAL_KEPT: usize = 20;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    #[serde(default)]
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    #[serde(default)]
    pub entries: BTreeMap<String, EntryState>,
    #[serde(default)]
    pub manual: Vec<ManualResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryState {
    /// Every fire up to this moment has been handled. A new entry starts at the moment it was first seen, so it never
    /// fires for times before it existed.
    pub base: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fire: Option<Fire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sent_at: Option<Timestamp>,
    /// A fire that found its session waiting on a person, tried again every minute (`if_busy: queue`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
    /// A fire being sent now. Still set after a restart means the scheduler stopped mid-send: the message may have
    /// arrived, so it isn't sent again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_flight: Option<Timestamp>,
    /// Fires in a row that weren't delivered.
    #[serde(default)]
    pub failures: u32,
    /// Fires never sent: missed while the scheduler was off (without `catch_up`), or folded into one late message.
    #[serde(default)]
    pub missed: u64,
    // For `fleetlayer-scheduler list`, refreshed on each save.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_fire: Option<Timestamp>,
}

fn yes() -> bool {
    true
}

impl EntryState {
    fn new(base: Timestamp) -> EntryState {
        EntryState {
            base,
            last_fire: None,
            last_sent_at: None,
            pending: None,
            in_flight: None,
            failures: 0,
            missed: 0,
            to: None,
            schedule: None,
            enabled: true,
            next_fire: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fire {
    /// When it was due.
    pub due: Timestamp,
    /// When it was tried.
    pub at: Timestamp,
    pub result: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pending {
    pub due: Timestamp,
    pub next_try: Timestamp,
    pub attempts: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Delivered.
    Sent,
    /// The session is waiting on a person; tried again every minute.
    Waiting,
    /// The session was waiting on a person, and the entry says skip.
    SkippedBusy,
    /// Missed while the scheduler wasn't running, and the entry doesn't catch up.
    Missed,
    /// The session isn't running, or no session has that name.
    Offline,
    /// Refused: by access control, a missing grant, or the node.
    Refused,
    /// No answer in time, or the scheduler stopped mid-send: it may have arrived.
    Unknown,
    /// Still waiting on a person when the next fire came.
    Expired,
}

impl Outcome {
    fn failed(self) -> bool {
        matches!(self, Outcome::Offline | Outcome::Refused | Outcome::Unknown | Outcome::Expired)
    }

    pub fn words(self) -> &'static str {
        match self {
            Outcome::Sent => "sent",
            Outcome::Waiting => "waiting: the session is waiting on a person",
            Outcome::SkippedBusy => "skipped: the session was waiting on a person",
            Outcome::Missed => "missed while the scheduler wasn't running",
            Outcome::Offline => "not delivered: the session isn't running",
            Outcome::Refused => "refused",
            Outcome::Unknown => "no answer: it may have arrived",
            Outcome::Expired => "not delivered: still waiting on a person at the next fire",
        }
    }
}

/// How a send went, from the node's answer to `_fleetlayer.dev/message/send`.
pub fn classify(result: &Result<Value, RpcError>) -> (Outcome, Option<String>) {
    match result {
        Ok(_) => (Outcome::Sent, None),
        Err(e) => {
            let outcome = match e.code {
                code::BUSY => Outcome::Waiting,
                code::NOT_HERE | code::NO_SUCH | code::UNKNOWN_SESSION => Outcome::Offline,
                code::REFUSED | code::METHOD_NOT_FOUND => Outcome::Refused,
                _ => Outcome::Unknown,
            };
            (outcome, Some(e.message.clone()))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManualResult {
    pub id: String,
    pub name: String,
    pub at: Timestamp,
    pub result: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// Its time came. `late` when the scheduler wasn't running then; `missed` other fires were folded into it.
    Scheduled { late: bool, missed: u32 },
    /// Trying again a fire that found the session busy.
    Retry,
    /// `fleetlayer-scheduler fire`, with the request's ID.
    Manual(String),
}

/// A message to send now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Send {
    pub name: String,
    pub to: String,
    pub text: String,
    pub due: Timestamp,
    pub why: Why,
}

#[derive(Debug, Default)]
pub struct Engine {
    pub state: State,
    pub settings: Settings,
    /// The settings file couldn't be read: the last good entries keep running.
    pub settings_problem: Option<String>,
    pub last_tick: Option<Timestamp>,
}

impl Engine {
    /// Start from saved state. A fire still marked in flight was cut off by a stop: it isn't sent again.
    pub fn new(mut state: State, settings: Settings) -> Engine {
        for st in state.entries.values_mut() {
            if let Some(due) = st.in_flight.take() {
                st.last_fire = Some(Fire {
                    due,
                    at: due,
                    result: Outcome::Unknown,
                    error: Some("the scheduler stopped while sending it".into()),
                });
            }
        }
        Engine { state, settings, settings_problem: None, last_tick: None }
    }

    /// What to send at `now`. Each returned fire is marked in flight: save the state before sending it.
    pub fn tick(&mut self, now: Timestamp) -> Vec<Send> {
        let mut sends = vec![];
        let entries = self.settings.entries.clone();
        self.state.entries.retain(|name, _| entries.iter().any(|e| &e.name == name));
        for e in &entries {
            let st = self.state.entries.entry(e.name.clone()).or_insert_with(|| EntryState::new(now));
            if !e.enabled {
                // Turned back on, it starts afresh rather than catching up on the time it was off.
                st.base = now;
                st.pending = None;
                continue;
            }
            if let Some((count, latest)) = e.schedule.due(st.base, now) {
                st.base = latest;
                if st.pending.take().is_some() {
                    st.last_fire = Some(Fire { due: latest, at: now, result: Outcome::Expired, error: None });
                    st.failures += 1;
                }
                let late = now.duration_since(latest) > GRACE;
                if !late || e.catch_up {
                    let missed = count.saturating_sub(1);
                    st.missed += u64::from(missed);
                    st.in_flight = Some(latest);
                    sends.push(send(e, latest, Why::Scheduled { late, missed }));
                } else {
                    st.missed += u64::from(count);
                    st.last_fire = Some(Fire { due: latest, at: now, result: Outcome::Missed, error: None });
                }
            } else if let Some(p) = &st.pending
                && now >= p.next_try
            {
                st.in_flight = Some(p.due);
                sends.push(send(e, p.due, Why::Retry));
            }
        }
        sends
    }

    /// Fire `name` now, by hand: the schedule doesn't change.
    pub fn fire_now(&self, name: &str, id: &str, now: Timestamp) -> Result<Send, String> {
        let e = self
            .settings
            .entries
            .iter()
            .find(|e| e.name == name)
            .ok_or_else(|| format!("no entry is named '{name}'"))?;
        Ok(send(e, now, Why::Manual(id.to_string())))
    }

    /// How a send went.
    pub fn record(&mut self, s: &Send, outcome: Outcome, error: Option<String>, now: Timestamp) {
        if let Why::Manual(id) = &s.why {
            self.state.manual.push(ManualResult {
                id: id.clone(),
                name: s.name.clone(),
                at: now,
                result: outcome,
                error,
            });
            let extra = self.state.manual.len().saturating_sub(MANUAL_KEPT);
            self.state.manual.drain(..extra);
            return;
        }
        let if_busy = self.settings.entries.iter().find(|e| e.name == s.name).map(|e| e.if_busy);
        let Some(st) = self.state.entries.get_mut(&s.name) else { return };
        st.in_flight = None;
        let outcome = match (outcome, if_busy) {
            (Outcome::Waiting, Some(IfBusy::Queue)) => {
                let attempts = st.pending.as_ref().map_or(0, |p| p.attempts) + 1;
                st.pending = Some(Pending { due: s.due, next_try: now + RETRY_BUSY, attempts });
                Outcome::Waiting
            }
            (Outcome::Waiting, _) => {
                st.pending = None;
                Outcome::SkippedBusy
            }
            (o, _) => {
                st.pending = None;
                o
            }
        };
        if outcome == Outcome::Sent {
            st.failures = 0;
            st.last_sent_at = Some(now);
        } else if outcome.failed() {
            st.failures += 1;
        }
        st.last_fire = Some(Fire { due: s.due, at: now, result: outcome, error });
    }

    /// Refresh what `list` shows, and the problem, before a save.
    pub fn refresh(&mut self, now: Timestamp) {
        for e in &self.settings.entries {
            if let Some(st) = self.state.entries.get_mut(&e.name) {
                st.to = Some(e.to.clone());
                st.schedule = Some(e.schedule.describe());
                st.enabled = e.enabled;
                st.next_fire = if !e.enabled {
                    None
                } else if let Some(p) = &st.pending {
                    Some(p.next_try)
                } else {
                    e.schedule.next_after(now.max(st.base), st.base)
                };
            }
        }
        self.state.version = 1;
        self.state.updated_at = Some(now);
        self.state.problem = self.problem(now);
    }

    /// One line for a person when the scheduler isn't doing its job, else None.
    pub fn problem(&self, now: Timestamp) -> Option<String> {
        let mut problems = vec![];
        if let Some(t) = self.last_tick
            && now.duration_since(t) > STUCK_AFTER
        {
            problems.push(format!("the scheduler has been stuck since {}", round(t)));
        }
        if let Some(p) = &self.settings_problem {
            problems.push(format!("its settings file can't be read, so the last good entries run: {}", first_line(p)));
        }
        for e in &self.settings.entries {
            let Some(st) = self.state.entries.get(&e.name) else { continue };
            if e.enabled && st.failures >= self.settings.alert_after {
                let why = st.last_fire.as_ref().map(|f| match &f.error {
                    Some(err) => format!("{}: {err}", f.result.words()),
                    None => f.result.words().to_string(),
                });
                problems.push(format!(
                    "'{}': the last {} fires to {} weren't delivered ({})",
                    e.name,
                    st.failures,
                    e.to,
                    why.unwrap_or_default()
                ));
            }
        }
        match problems.len() {
            0 => None,
            1 => problems.pop(),
            n => Some(format!("{} (and {} more)", problems[0], n - 1)),
        }
    }

    /// The `detail` of a status report: per entry its last fire, next fire and last error, cut to a summary if it
    /// would pass the node's 4 KB limit.
    pub fn detail(&self) -> Value {
        let mut entries = serde_json::Map::new();
        for (name, st) in &self.state.entries {
            let last = st.last_fire.as_ref();
            entries.insert(
                name.clone(),
                json!({
                    "next": st.next_fire,
                    "last": last.map(|f| f.at),
                    "result": last.map(|f| f.result),
                    "error": last.and_then(|f| f.error.as_deref()).map(|e| truncate(e, 200)),
                    "failures": st.failures,
                }),
            );
        }
        let failing: Vec<&String> = self.state.entries.iter().filter(|(_, s)| s.failures > 0).map(|(n, _)| n).collect();
        let full = json!({"lastTickAt": self.last_tick, "entries": entries});
        if full.to_string().len() <= 3500 {
            return full;
        }
        json!({"lastTickAt": self.last_tick, "entryCount": self.state.entries.len(), "failing": failing})
    }
}

fn send(e: &Entry, due: Timestamp, why: Why) -> Send {
    Send { name: e.name.clone(), to: e.to.clone(), text: render(e, due, &why), due, why }
}

/// The message as the session sees it, under the node's "Message from plugin scheduler (<node>)" line.
pub fn render(e: &Entry, due: Timestamp, why: &Why) -> String {
    let mut out = format!("From the scheduler entry \"{}\" ({}", e.name, e.schedule.describe());
    if matches!(why, Why::Manual(_)) {
        out.push_str(", fired by hand");
    }
    out.push_str("):\n\n");
    out.push_str(e.text.trim_end());
    match why {
        Why::Scheduled { late: true, missed } => {
            out.push_str(&format!("\n\n(Sent late: it was due at {}, while the scheduler wasn't running", round(due)));
            if *missed > 0 {
                out.push_str(&format!(
                    "; {missed} earlier fire{} folded into this one",
                    if *missed == 1 { "" } else { "s" }
                ));
            }
            out.push_str(".)");
        }
        Why::Retry => {
            out.push_str(&format!("\n\n(Due at {}; held until the session could take it.)", round(due)));
        }
        _ => {}
    }
    out
}

pub fn round(t: Timestamp) -> Timestamp {
    t.round(jiff::Unit::Second).unwrap_or(t)
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}
