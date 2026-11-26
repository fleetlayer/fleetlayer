//! When to summarise, from herdr's status events. Pure: every method takes `now`, so tests drive it directly; the
//! driver in `service.rs` sleeps until `next_due` and runs what `take_ready` hands it.
//!
//! - A session that turns idle (herdr's `idle`, or `done`: finished and not yet looked at) and stays idle for
//!   `idle_delay` is summarised. Working again inside that time cancels it.
//! - Never while `blocked`: blocked cancels anything pending.
//! - At most one summary per session per `min_gap`, measured from the start of its last summary call. Idle inside the
//!   gap schedules one summary for when the gap ends; nothing is dropped.
//! - At most one pending summary per session: idle again while one is pending queues nothing new.
//! - At most `max_concurrent` summaries at once on the machine; the rest wait their turn, earliest due first.
//! - A session whose agent ended (tab closed, process exited) is idle for good: it's summarised after `idle_delay` too.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Idle,
    /// Finished a turn, not yet seen: as idle as `Idle` for summaries.
    Done,
    Working,
    /// Waiting on the user (a permission prompt, a question).
    Blocked,
    /// The agent ended: tab closed or process exited.
    Gone,
    Unknown,
}

impl Status {
    fn is_idle(self) -> bool {
        matches!(self, Status::Idle | Status::Done | Status::Gone)
    }

    /// Running in a tab, for the sweep's "isn't running".
    pub fn is_running(self) -> bool {
        matches!(self, Status::Idle | Status::Done | Status::Working | Status::Blocked)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub idle_delay: Duration,
    pub min_gap: Duration,
    pub max_concurrent: usize,
}

#[derive(Debug, Clone, Default)]
struct Entry {
    status: Option<Status>,
    idle_since: Option<Instant>,
    /// When the pending summary is due.
    due: Option<Instant>,
    running: bool,
    /// The start of the last summary that made a call.
    last_call: Option<Instant>,
    /// `last_call` before the running summary started, restored if it made no call.
    prev_call: Option<Instant>,
}

#[derive(Debug)]
pub struct Scheduler {
    limits: Limits,
    sessions: HashMap<String, Entry>,
    in_flight: usize,
}

impl Scheduler {
    pub fn new(limits: Limits) -> Self {
        Scheduler { limits, sessions: HashMap::new(), in_flight: 0 }
    }

    pub fn status(&self, session: &str) -> Option<Status> {
        self.sessions.get(session).and_then(|e| e.status)
    }

    pub fn pending(&self, session: &str) -> Option<Instant> {
        self.sessions.get(session).and_then(|e| e.due)
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    fn due_for(&self, e: &Entry, idle_since: Instant) -> Instant {
        let after_idle = idle_since + self.limits.idle_delay;
        match e.last_call {
            Some(last) => after_idle.max(last + self.limits.min_gap),
            None => after_idle,
        }
    }

    /// A status event from herdr.
    pub fn on_status(&mut self, session: &str, status: Status, now: Instant) {
        let mut e = self.sessions.remove(session).unwrap_or_default();
        let was_idle = e.status.is_some_and(Status::is_idle);
        e.status = Some(status);
        if status.is_idle() {
            if !was_idle || e.idle_since.is_none() {
                e.idle_since = Some(now);
            }
            if e.due.is_none() {
                e.due = Some(self.due_for(&e, e.idle_since.expect("set above")));
            }
        } else {
            // Working, blocked or unknown: no summary until it's idle again.
            e.idle_since = None;
            e.due = None;
        }
        self.sessions.insert(session.to_owned(), e);
    }

    /// When the earliest pending summary is due, for the driver's sleep. None while every slot is taken: a finished
    /// summary wakes the driver instead (otherwise a due summary waiting for a slot would make it spin).
    pub fn next_due(&self) -> Option<Instant> {
        if self.in_flight >= self.limits.max_concurrent {
            return None;
        }
        self.sessions
            .values()
            .filter(|e| !e.running && e.status.is_some_and(Status::is_idle))
            .filter_map(|e| e.due)
            .min()
    }

    /// Sessions to summarise now, earliest due first, up to the concurrency limit. Each is marked running.
    pub fn take_ready(&mut self, now: Instant) -> Vec<String> {
        let mut ready: Vec<(Instant, String)> = self
            .sessions
            .iter()
            .filter(|(_, e)| !e.running && e.status.is_some_and(Status::is_idle))
            .filter_map(|(id, e)| e.due.filter(|d| *d <= now).map(|d| (d, id.clone())))
            .collect();
        ready.sort();
        let room = self.limits.max_concurrent.saturating_sub(self.in_flight);
        let mut out = Vec::new();
        for (_, id) in ready.into_iter().take(room) {
            let e = self.sessions.get_mut(&id).expect("listed above");
            e.running = true;
            e.due = None;
            e.prev_call = e.last_call;
            e.last_call = Some(now);
            self.in_flight += 1;
            out.push(id);
        }
        out
    }

    /// Run a session's summary outside the idle trigger (the sweep), if there's room and it isn't already running.
    pub fn try_start(&mut self, session: &str, now: Instant) -> bool {
        if self.in_flight >= self.limits.max_concurrent {
            return false;
        }
        let e = self.sessions.entry(session.to_owned()).or_default();
        if e.running {
            return false;
        }
        e.running = true;
        e.due = None;
        e.prev_call = e.last_call;
        e.last_call = Some(now);
        self.in_flight += 1;
        true
    }

    /// A summary finished. `called` is false when nothing was worth a model call (or no account had room), which
    /// doesn't count against the session's gap.
    pub fn on_finished(&mut self, session: &str, called: bool, _now: Instant) {
        if let Some(e) = self.sessions.get_mut(session) {
            if e.running {
                e.running = false;
                self.in_flight -= 1;
            }
            if !called {
                e.last_call = e.prev_call;
                // An idle that arrived while it ran was scheduled against the run's start: move it back.
                if let (Some(due), Some(idle_since)) = (e.due, e.idle_since) {
                    let after_idle = idle_since + self.limits.idle_delay;
                    let earliest = match e.last_call {
                        Some(last) => after_idle.max(last + self.limits.min_gap),
                        None => after_idle,
                    };
                    e.due = Some(due.min(earliest));
                }
            }
        }
    }

    /// Sessions herdr reports as running: the sweep leaves these to the idle trigger.
    pub fn running_sessions(&self) -> Vec<String> {
        self.sessions
            .iter()
            .filter(|(_, e)| e.status.is_some_and(Status::is_running))
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub fn forget(&mut self, session: &str) {
        if self.sessions.get(session).is_some_and(|e| !e.running) {
            self.sessions.remove(session);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);

    fn sched() -> (Scheduler, Instant) {
        (Scheduler::new(Limits { idle_delay: MIN, min_gap: 30 * MIN, max_concurrent: 2 }), Instant::now())
    }

    #[test]
    fn idle_for_a_minute_triggers_and_working_cancels() {
        let (mut s, t0) = sched();
        s.on_status("a", Status::Working, t0);
        s.on_status("a", Status::Idle, t0 + MIN);
        assert_eq!(s.next_due(), Some(t0 + 2 * MIN));
        assert!(s.take_ready(t0 + MIN + Duration::from_secs(59)).is_empty());
        // Working again inside the minute: cancelled.
        s.on_status("a", Status::Working, t0 + MIN + Duration::from_secs(30));
        assert_eq!(s.next_due(), None);
        assert!(s.take_ready(t0 + 5 * MIN).is_empty());
        // Done (finished, unread) counts as idle.
        s.on_status("a", Status::Done, t0 + 6 * MIN);
        assert_eq!(s.take_ready(t0 + 7 * MIN), vec!["a"]);
    }

    #[test]
    fn never_while_blocked() {
        let (mut s, t0) = sched();
        s.on_status("a", Status::Idle, t0);
        s.on_status("a", Status::Blocked, t0 + Duration::from_secs(30));
        assert!(s.take_ready(t0 + 10 * MIN).is_empty());
        assert_eq!(s.next_due(), None);
        // Blocked from the start: nothing either.
        s.on_status("b", Status::Blocked, t0);
        assert!(s.take_ready(t0 + 60 * MIN).is_empty());
    }

    #[test]
    fn repeated_idle_events_keep_one_pending_summary() {
        let (mut s, t0) = sched();
        s.on_status("a", Status::Idle, t0);
        s.on_status("a", Status::Done, t0 + Duration::from_secs(20));
        s.on_status("a", Status::Idle, t0 + Duration::from_secs(40));
        assert_eq!(s.pending("a"), Some(t0 + MIN), "the first idle's timer stands");
        assert_eq!(s.take_ready(t0 + MIN), vec!["a"]);
        assert!(s.take_ready(t0 + 2 * MIN).is_empty(), "only one");
    }

    #[test]
    fn idle_inside_the_gap_waits_for_the_gap_and_is_never_dropped() {
        let (mut s, t0) = sched();
        s.on_status("a", Status::Idle, t0);
        assert_eq!(s.take_ready(t0 + MIN), vec!["a"]);
        s.on_finished("a", true, t0 + MIN + Duration::from_secs(12));
        // Works and goes idle again 10 minutes later: due when the 30 minutes from the last call are up.
        s.on_status("a", Status::Working, t0 + 10 * MIN);
        s.on_status("a", Status::Idle, t0 + 11 * MIN);
        assert_eq!(s.pending("a"), Some(t0 + 31 * MIN));
        assert!(s.take_ready(t0 + 30 * MIN).is_empty());
        assert_eq!(s.take_ready(t0 + 31 * MIN), vec!["a"]);
    }

    #[test]
    fn working_when_the_gap_ends_means_the_next_idle_runs_after_the_idle_minute() {
        let (mut s, t0) = sched();
        s.on_status("a", Status::Idle, t0);
        assert_eq!(s.take_ready(t0 + MIN), vec!["a"]);
        s.on_finished("a", true, t0 + MIN);
        s.on_status("a", Status::Idle, t0 + 5 * MIN); // still idle: no change of state, nothing new
        s.on_status("a", Status::Working, t0 + 20 * MIN);
        assert_eq!(s.next_due(), None);
        s.on_status("a", Status::Idle, t0 + 40 * MIN);
        assert_eq!(s.pending("a"), Some(t0 + 41 * MIN));
    }

    #[test]
    fn idle_while_running_is_scheduled_against_the_runs_start() {
        let (mut s, t0) = sched();
        s.on_status("a", Status::Idle, t0);
        assert_eq!(s.take_ready(t0 + MIN), vec!["a"]);
        s.on_status("a", Status::Working, t0 + MIN + Duration::from_secs(5));
        s.on_status("a", Status::Idle, t0 + MIN + Duration::from_secs(8));
        assert!(s.take_ready(t0 + 5 * MIN).is_empty(), "still running");
        s.on_finished("a", true, t0 + MIN + Duration::from_secs(14));
        assert_eq!(s.pending("a"), Some(t0 + 31 * MIN));
    }

    #[test]
    fn a_pass_without_a_call_doesnt_count_against_the_gap() {
        let (mut s, t0) = sched();
        s.on_status("a", Status::Idle, t0);
        assert_eq!(s.take_ready(t0 + MIN), vec!["a"]);
        s.on_status("a", Status::Working, t0 + MIN + Duration::from_secs(1));
        s.on_status("a", Status::Idle, t0 + 2 * MIN);
        s.on_finished("a", false, t0 + 2 * MIN);
        assert_eq!(s.pending("a"), Some(t0 + 3 * MIN));
    }

    #[test]
    fn at_most_two_at_once_earliest_first() {
        let (mut s, t0) = sched();
        s.on_status("c", Status::Idle, t0 + Duration::from_secs(2));
        s.on_status("a", Status::Idle, t0);
        s.on_status("b", Status::Idle, t0 + Duration::from_secs(1));
        let mut first = s.take_ready(t0 + 2 * MIN);
        first.sort();
        assert_eq!(first, vec!["a", "b"]);
        assert_eq!(s.in_flight(), 2);
        assert!(s.take_ready(t0 + 3 * MIN).is_empty());
        assert_eq!(s.next_due(), None, "full: nothing to wake for until one finishes");
        assert!(!s.try_start("d", t0 + 3 * MIN), "the sweep waits too");
        s.on_finished("a", true, t0 + 3 * MIN);
        assert_eq!(s.take_ready(t0 + 3 * MIN), vec!["c"]);
        assert_eq!(s.in_flight(), 2);
    }

    #[test]
    fn ended_sessions_are_summarised_and_not_running() {
        let (mut s, t0) = sched();
        s.on_status("a", Status::Working, t0);
        s.on_status("b", Status::Blocked, t0);
        s.on_status("a", Status::Gone, t0 + MIN);
        let mut running = s.running_sessions();
        running.sort();
        assert_eq!(running, vec!["b"]);
        assert_eq!(s.take_ready(t0 + 2 * MIN), vec!["a"]);
    }
}
