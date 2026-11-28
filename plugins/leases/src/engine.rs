//! The leases themselves, with no I/O: who holds what, who waits, and when each ends. Every method takes the time,
//! so tests can drive it. The runner (`runner.rs`) saves the state after each change and acts on the [`Change`]s.
//!
//! A lease goes through up to three phases: `asking` (the access policy is deciding its payload, maybe asking the
//! person), `waiting` (in the kind's queue for a slot, first come first served) and `held`. It ends when its holder
//! releases it, at its TTL, or when its holder dies:
//! - tied to a **session** (`<tab>@<node>`): the session stops running, its node goes offline, or a held lease's
//!   session stays idle (or blocked on the person) past the lease's idle limit;
//! - tied to a **process** (a script outside a session): no connection keeps it (the CLI's keeper, which watches the
//!   process, or a waiting `--wait`) for [`PROCESS_GRACE`].

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::settings::{Kind, Settings, payload_key};

/// How long a process-tied lease outlives its last connection: a keeper reconnecting after a network blip, or after
/// the authority restarted, keeps its lease.
pub const PROCESS_GRACE: SignedDuration = SignedDuration::from_mins(2);
/// How long a session-tied lease outlives its session not being listed (a restart, a session manager hiccup).
pub const SESSION_GRACE: SignedDuration = SignedDuration::from_mins(2);
/// How long a session-tied lease outlives its node being offline.
pub const NODE_GRACE: SignedDuration = SignedDuration::from_mins(5);
/// The longest a lease waits for the person's approval.
pub const ASK_LIMIT: SignedDuration = SignedDuration::from_hours(24);
/// How long an ended lease stays in the list.
const ENDED_KEEP: SignedDuration = SignedDuration::from_hours(24);
const ENDED_MAX: usize = 200;
/// How long a lease that ran into its TTL keeps the plugin's problem set, so monitoring hears about it.
pub const EXPIRED_PROBLEM_FOR: SignedDuration = SignedDuration::from_mins(30);
const MAX_REASON: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Asking,
    Waiting,
    Held,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Tie {
    Session,
    Process { pid: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lease {
    pub id: u64,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_key: Option<String>,
    pub reason: String,
    /// `<tab>@<node>`, or `-@<node>` for a script.
    pub holder: String,
    pub tie: Tie,
    pub phase: Phase,
    pub requested_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
    pub ttl_secs: i64,
    pub idle_secs: i64,
    /// While asking: what the access policy said last (the approval request it waits on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<String>,
    /// It waits only while its caller waits for it (`--wait` on a kind without approval): a session's waiting
    /// lease whose caller stopped waiting leaves the queue.
    #[serde(default)]
    pub bound: bool,
    /// Live connections keeping it: a `--wait` caller, a keeper. Not saved: none survive a restart.
    #[serde(skip)]
    pub attached: u32,
    /// A process-tied lease with no connection since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detached_since: Option<Timestamp>,
    /// A session-tied lease whose session or node wasn't seen since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing_since: Option<Timestamp>,
}

impl Lease {
    pub fn holder_node(&self) -> &str {
        self.holder.rsplit_once('@').map(|(_, n)| n).unwrap_or("")
    }
    fn holder_tab(&self) -> &str {
        self.holder.rsplit_once('@').map(|(t, _)| t).unwrap_or("")
    }
    fn same_claim(&self, other: &Lease) -> bool {
        self.kind == other.kind && self.payload_key.is_some() && self.payload_key == other.payload_key
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    Released,
    Expired,
    Idle,
    SessionGone,
    NodeGone,
    ProcessGone,
    Refused,
    NotApproved,
    Cancelled,
    KindRemoved,
}

impl EndReason {
    pub fn words(self) -> &'static str {
        match self {
            EndReason::Released => "released",
            EndReason::Expired => "expired at its TTL",
            EndReason::Idle => "its session was idle past the lease's idle limit",
            EndReason::SessionGone => "its session stopped running",
            EndReason::NodeGone => "its holder's node went offline",
            EndReason::ProcessGone => "the process holding it went away",
            EndReason::Refused => "the access policy refused it",
            EndReason::NotApproved => "nobody approved it within a day",
            EndReason::Cancelled => "its caller stopped waiting",
            EndReason::KindRemoved => "its kind was removed from the settings",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ended {
    #[serde(flatten)]
    pub lease: Lease,
    pub ended_at: Timestamp,
    pub why: EndReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Whoever ended it, when it was released.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

/// What happened, for the runner to act on: events, messages, answers to callers waiting.
#[derive(Debug, Clone)]
pub enum Change {
    /// `in_call`: it started within its own `acquire`, whose answer tells the caller.
    Started {
        lease: Lease,
        first_of_key: bool,
        in_call: bool,
    },
    Ended {
        ended: Ended,
        was_held: bool,
        last_of_key: bool,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub next_id: u64,
    pub leases: BTreeMap<u64, Lease>,
    #[serde(default)]
    pub ended: VecDeque<Ended>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<Timestamp>,
}

/// What a caller asks for.
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub kind: String,
    pub payload: Option<Value>,
    pub reason: String,
    pub holder: String,
    pub tie: Option<Tie>,
    /// Used when the holder turns out not to be a session (`-@<node>`).
    pub fallback_pid: Option<u32>,
    pub ttl: Option<SignedDuration>,
    pub idle: Option<SignedDuration>,
    pub wait: bool,
}

/// Why a request wasn't taken.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// Every slot is held and the caller won't wait: who holds and who waits.
    Busy {
        holders: Vec<String>,
        waiting: usize,
    },
    Invalid(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Busy { holders, waiting } => {
                write!(f, "every slot is held ({})", holders.join("; "))?;
                if *waiting > 0 {
                    write!(f, ", {waiting} waiting")?;
                }
                write!(f, ": add --wait to wait your turn")
            }
            Refusal::Invalid(s) => f.write_str(s),
        }
    }
}

/// The fleet as the control plane lists it, reduced to what liveness needs.
#[derive(Debug, Clone, Default)]
pub struct FleetView {
    pub online: HashSet<String>,
    /// Running sessions by (node, tab): their status and when it last changed.
    pub sessions: HashMap<(String, String), SessionInfo>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionInfo {
    /// Idle, done (a turn nobody looked at) or blocked on the person: not working.
    pub resting: bool,
    pub since: Option<Timestamp>,
}

pub struct Engine {
    pub state: State,
}

impl Engine {
    pub fn new(mut state: State, now: Timestamp) -> Engine {
        // No connection survived the restart: a process-tied lease's keeper has the grace to come back.
        for l in state.leases.values_mut() {
            l.attached = 0;
            if matches!(l.tie, Tie::Process { .. }) {
                l.detached_since = Some(now);
            }
        }
        if state.next_id == 0 {
            state.next_id = 1;
        }
        Engine { state }
    }

    pub fn get(&self, id: u64) -> Option<&Lease> {
        self.state.leases.get(&id)
    }

    pub fn ended(&self, id: u64) -> Option<&Ended> {
        self.state.ended.iter().rev().find(|e| e.lease.id == id)
    }

    /// Take a request: a new lease asking, waiting or held, or the holder's own live lease on the same claim.
    pub fn acquire(
        &mut self,
        settings: &Settings,
        req: Request,
        now: Timestamp,
    ) -> Result<(u64, Vec<Change>), Refusal> {
        let invalid = |s: String| Refusal::Invalid(s);
        let kind = settings.kinds.get(&req.kind).ok_or_else(|| {
            let known: Vec<&str> = settings.kinds.keys().map(String::as_str).collect();
            invalid(if known.is_empty() {
                format!("no kind of lease is called '{}': this authority has none set up", req.kind)
            } else {
                format!("no kind of lease is called '{}' (kinds: {})", req.kind, known.join(", "))
            })
        })?;
        let reason = req.reason.trim().to_string();
        if reason.chars().count() < 4 || reason.chars().count() > MAX_REASON || reason.chars().any(char::is_control) {
            return Err(invalid(format!("give a reason: one line of 4 to {MAX_REASON} characters")));
        }
        if !req.holder.contains('@') {
            return Err(invalid("the caller isn't <tab>@<node>".into()));
        }
        let payload = kind.check_payload(req.payload.as_ref()).map_err(invalid)?;
        let key = payload.as_ref().map(payload_key);
        let ttl = match req.ttl {
            Some(t) if t > kind.max_ttl => {
                return Err(invalid(format!(
                    "--ttl {} is longer than {}'s limit ({})",
                    words(t),
                    kind.name,
                    words(kind.max_ttl)
                )));
            }
            Some(t) => t,
            None => kind.ttl,
        };
        let idle = req.idle.unwrap_or(kind.idle);
        let is_script = req.holder.starts_with("-@");
        let tie = match (req.tie, req.fallback_pid) {
            (Some(Tie::Session), Some(pid)) if is_script => Tie::Process { pid },
            (Some(Tie::Session), None) if is_script => {
                return Err(invalid(
                    "this caller isn't in a session its node knows: give the process with --pid".into(),
                ));
            }
            (Some(t), _) => t,
            (None, Some(pid)) if is_script => Tie::Process { pid },
            (None, _) if is_script => {
                return Err(invalid(
                    "this caller isn't in a session its node knows: give the process with --pid".into(),
                ));
            }
            (None, _) => Tie::Session,
        };
        // Asking twice for the same claim gets the lease you have.
        if key.is_some()
            && let Some(l) = self
                .state
                .leases
                .values()
                .find(|l| l.holder == req.holder && l.kind == kind.name && l.payload_key == key)
        {
            return Ok((l.id, vec![]));
        }
        if let Some(max) = kind.max_per_holder {
            let n = self.state.leases.values().filter(|l| l.holder == req.holder && l.kind == kind.name).count();
            if n >= max as usize {
                return Err(invalid(format!("{} holds {n} {} leases already, the most it may", req.holder, kind.name)));
            }
        }
        let id = self.state.next_id;
        self.state.next_id += 1;
        let lease = Lease {
            id,
            kind: kind.name.clone(),
            payload,
            payload_key: key,
            reason,
            holder: req.holder.clone(),
            detached_since: matches!(tie, Tie::Process { .. }).then_some(now),
            tie,
            phase: if kind.approval { Phase::Asking } else { Phase::Waiting },
            requested_at: now,
            started_at: None,
            expires_at: None,
            ttl_secs: ttl.as_secs(),
            idle_secs: idle.as_secs(),
            approval: None,
            bound: req.wait && !kind.approval,
            attached: 0,
            missing_since: None,
        };
        self.state.leases.insert(id, lease);
        let mut changes = self.promote(settings, now);
        for c in &mut changes {
            if let Change::Started { lease, in_call, .. } = c
                && lease.id == id
            {
                *in_call = true;
            }
        }
        let l = &self.state.leases[&id];
        if l.phase == Phase::Waiting && !req.wait {
            let holders = self.holders(&kind.name);
            let waiting = self.waiting(&kind.name).len().saturating_sub(1);
            self.state.leases.remove(&id);
            return Err(Refusal::Busy { holders, waiting });
        }
        Ok((id, changes))
    }

    /// `<holder> since <time>: <reason>` for each held lease of a kind.
    pub fn holders(&self, kind: &str) -> Vec<String> {
        self.state
            .leases
            .values()
            .filter(|l| l.kind == kind && l.phase == Phase::Held)
            .map(|l| {
                format!(
                    "{} has #{} since {}: {}",
                    l.holder,
                    l.id,
                    short(l.started_at.unwrap_or(l.requested_at)),
                    l.reason
                )
            })
            .collect()
    }

    /// A kind's waiting leases, first in line first.
    pub fn waiting(&self, kind: &str) -> Vec<&Lease> {
        self.state.leases.values().filter(|l| l.kind == kind && l.phase == Phase::Waiting).collect()
    }

    /// Where a waiting lease is in its queue (1: next).
    pub fn position(&self, id: u64) -> Option<usize> {
        let l = self.state.leases.get(&id)?;
        if l.phase != Phase::Waiting {
            return None;
        }
        self.waiting(&l.kind).iter().position(|w| w.id == id).map(|p| p + 1)
    }

    /// The access policy allowed an asking lease: it joins the queue.
    pub fn approved(&mut self, settings: &Settings, id: u64, now: Timestamp) -> Vec<Change> {
        if let Some(l) = self.state.leases.get_mut(&id)
            && l.phase == Phase::Asking
        {
            l.phase = Phase::Waiting;
            l.approval = None;
            // It waits in the queue now, ahead of anyone who asked after it.
            return self.promote(settings, now);
        }
        vec![]
    }

    /// The access policy refused an asking lease, or it's still waiting for the person (`pending`).
    pub fn asked(&mut self, id: u64, pending: Option<String>, refused: Option<String>, now: Timestamp) -> Vec<Change> {
        match refused {
            Some(why) => self.end(id, EndReason::Refused, Some(why), None, now).into_iter().collect(),
            None => {
                if let Some(l) = self.state.leases.get_mut(&id) {
                    l.approval = pending;
                }
                vec![]
            }
        }
    }

    /// End a lease on its holder's (or the authority's admin's) request. `caller` must be on the holder's node, or
    /// `None` for the admin.
    pub fn release(
        &mut self,
        settings: &Settings,
        id: u64,
        caller: Option<&str>,
        now: Timestamp,
    ) -> Result<Vec<Change>, String> {
        let Some(l) = self.state.leases.get(&id) else {
            // Releasing a lease that already ended is fine: the caller's work is done either way.
            return if self.ended(id).is_some() { Ok(vec![]) } else { Err(format!("no lease #{id}")) };
        };
        if let Some(c) = caller {
            let node = c.rsplit_once('@').map(|(_, n)| n).unwrap_or("");
            if node != l.holder_node() {
                return Err(format!("lease #{id} is {}'s: only its own node may release it", l.holder));
            }
        }
        let mut changes: Vec<Change> =
            self.end(id, EndReason::Released, None, caller.map(str::to_string), now).into_iter().collect();
        changes.extend(self.promote(settings, now));
        Ok(changes)
    }

    /// A connection now keeps this lease (a `--wait` caller, a keeper). False if there's no such live lease.
    pub fn attach(&mut self, id: u64) -> bool {
        match self.state.leases.get_mut(&id) {
            Some(l) => {
                l.attached += 1;
                l.detached_since = None;
                true
            }
            None => false,
        }
    }

    /// A connection that kept this lease closed.
    pub fn detach(&mut self, settings: &Settings, id: u64, now: Timestamp) -> Vec<Change> {
        let Some(l) = self.state.leases.get_mut(&id) else { return vec![] };
        l.attached = l.attached.saturating_sub(1);
        if l.attached > 0 {
            return vec![];
        }
        if matches!(l.tie, Tie::Process { .. }) {
            l.detached_since = Some(now);
            return vec![];
        }
        if l.phase == Phase::Waiting && l.bound {
            let mut changes: Vec<Change> = self.end(id, EndReason::Cancelled, None, None, now).into_iter().collect();
            changes.extend(self.promote(settings, now));
            return changes;
        }
        vec![]
    }

    /// Every check that depends on time and on the fleet: TTLs, holders that died, approvals nobody gave, and the
    /// queue moving up. `fleet` is None when it couldn't be read: sessions are then left alone.
    pub fn tick(&mut self, settings: &Settings, fleet: Option<&FleetView>, now: Timestamp) -> Vec<Change> {
        let mut ending: Vec<(u64, EndReason)> = vec![];
        for l in self.state.leases.values_mut() {
            if !settings.kinds.contains_key(&l.kind) && l.phase != Phase::Held {
                ending.push((l.id, EndReason::KindRemoved));
                continue;
            }
            if l.phase == Phase::Held && l.expires_at.is_some_and(|e| e <= now) {
                ending.push((l.id, EndReason::Expired));
                continue;
            }
            if l.phase == Phase::Asking && l.requested_at + ASK_LIMIT <= now {
                ending.push((l.id, EndReason::NotApproved));
                continue;
            }
            match l.tie {
                Tie::Process { .. } => {
                    if l.attached == 0 && l.detached_since.is_some_and(|d| d + PROCESS_GRACE <= now) {
                        ending.push((l.id, EndReason::ProcessGone));
                    }
                }
                Tie::Session => {
                    let Some(f) = fleet else { continue };
                    let node = l.holder_node().to_string();
                    let tab = l.holder_tab().to_string();
                    let (gone, grace) = if !f.online.contains(&node) {
                        (Some(EndReason::NodeGone), NODE_GRACE)
                    } else if !f.sessions.contains_key(&(node.clone(), tab.clone())) {
                        (Some(EndReason::SessionGone), SESSION_GRACE)
                    } else {
                        (None, SignedDuration::ZERO)
                    };
                    match gone {
                        Some(why) => {
                            let since = *l.missing_since.get_or_insert(now);
                            if since + grace <= now {
                                ending.push((l.id, why));
                            }
                        }
                        None => {
                            l.missing_since = None;
                            let s = &f.sessions[&(node, tab)];
                            if l.phase == Phase::Held
                                && s.resting
                                && let Some(since) = s.since
                            {
                                let from = since.max(l.started_at.unwrap_or(since));
                                if from + SignedDuration::from_secs(l.idle_secs) <= now {
                                    ending.push((l.id, EndReason::Idle));
                                }
                            }
                        }
                    }
                }
            }
        }
        let mut changes: Vec<Change> = vec![];
        for (id, why) in ending {
            changes.extend(self.end(id, why, None, None, now));
        }
        changes.extend(self.promote(settings, now));
        let cutoff = now - ENDED_KEEP;
        while self.state.ended.front().is_some_and(|e| e.ended_at < cutoff) || self.state.ended.len() > ENDED_MAX {
            self.state.ended.pop_front();
        }
        changes
    }

    /// Start waiting leases while their kind has room, first in line first; a full kind starts nobody behind its
    /// first.
    fn promote(&mut self, settings: &Settings, now: Timestamp) -> Vec<Change> {
        let mut changes = vec![];
        for kind in settings.kinds.values() {
            loop {
                let held = self.state.leases.values().filter(|l| l.kind == kind.name && l.phase == Phase::Held).count();
                if kind.slots.is_some_and(|s| held >= s as usize) {
                    break;
                }
                let Some(id) = self.waiting(&kind.name).first().map(|l| l.id) else { break };
                changes.push(self.start(kind, id, now));
            }
        }
        changes
    }

    fn start(&mut self, _kind: &Kind, id: u64, now: Timestamp) -> Change {
        let first_of_key = {
            let l = &self.state.leases[&id];
            !self.state.leases.values().any(|o| o.id != id && o.phase == Phase::Held && o.same_claim(l))
        };
        let l = self.state.leases.get_mut(&id).expect("listed as waiting");
        l.phase = Phase::Held;
        l.started_at = Some(now);
        l.expires_at = Some(now + SignedDuration::from_secs(l.ttl_secs));
        Change::Started { lease: l.clone(), first_of_key, in_call: false }
    }

    fn end(
        &mut self,
        id: u64,
        why: EndReason,
        detail: Option<String>,
        by: Option<String>,
        now: Timestamp,
    ) -> Option<Change> {
        let lease = self.state.leases.remove(&id)?;
        let was_held = lease.phase == Phase::Held;
        let last_of_key = !self.state.leases.values().any(|o| o.phase == Phase::Held && o.same_claim(&lease));
        let ended = Ended { lease, ended_at: now, why, detail, by };
        self.state.ended.push_back(ended.clone());
        Some(Change::Ended { ended, was_held, last_of_key })
    }

    /// Leases that ran into their TTL lately (the problem monitoring hears about).
    pub fn recently_expired(&self, now: Timestamp) -> Vec<&Ended> {
        self.state
            .ended
            .iter()
            .filter(|e| e.why == EndReason::Expired && e.ended_at + EXPIRED_PROBLEM_FOR > now)
            .collect()
    }

    /// Kinds whose first waiting lease has waited past the kind's `stuck_after`.
    pub fn stuck(&self, settings: &Settings, now: Timestamp) -> Vec<(String, Timestamp)> {
        settings
            .kinds
            .values()
            .filter_map(|k| {
                let first = self.waiting(&k.name).first().map(|l| l.requested_at)?;
                (first + k.stuck_after <= now).then(|| (k.name.clone(), first))
            })
            .collect()
    }
}

/// A time to the second, for people.
pub fn short(t: Timestamp) -> String {
    t.round(jiff::Unit::Second).map(|t| t.to_string()).unwrap_or_else(|_| t.to_string())
}

/// `2h`, `1h30m`, `45m`.
pub fn words(d: SignedDuration) -> String {
    let s = d.as_secs();
    let (h, m) = (s / 3600, (s % 3600) / 60);
    match (h, m) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h{m}m"),
    }
}
