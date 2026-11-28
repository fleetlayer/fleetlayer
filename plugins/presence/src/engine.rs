//! How the authority decides a person's state from what it knows: their desks' reports, the last time they looked at
//! a page, what they said, their planned absences and the time of day where they are. Pure: everything comes in as
//! arguments, so the tests can set the clock.
//!
//! In order, the first that holds wins:
//! 1. **present**: a desk reported in the last few minutes, unlocked, with input in the last `away_after`; or they
//!    looked at a page on a desk lately. A stated away holds against a desk only until the desk has shown them gone
//!    (locked, idle 5 minutes, or silent): someone who says "going to lunch" is still typing when they say it.
//! 2. **semi_present**: they looked at a page on their phone in the last `seen_for`, whatever else holds (an away
//!    isn't absolute: someone who checks their phone may answer).
//! 3. **away** or **asleep** as they said, until its time.
//! 4. **away** during a planned absence.
//! 5. **asleep** during their night.
//! 6. **away**: nothing says they're here.

use jiff::{SignedDuration, Timestamp, ZonedDateTime};
use serde::{Deserialize, Serialize};

use crate::settings::{Person, words};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Present,
    SemiPresent,
    Away,
    Asleep,
}

impl State {
    pub fn words(self) -> &'static str {
        match self {
            State::Present => "present",
            State::SemiPresent => "semi-present",
            State::Away => "away",
            State::Asleep => "asleep",
        }
    }
}

/// A desk's latest report (`presence.desk` reading from one of the person's desk nodes).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Desk {
    pub node: String,
    /// None when the source couldn't tell.
    pub locked: Option<bool>,
    /// Seconds since the last keyboard or mouse input; None when the source couldn't tell.
    pub idle_seconds: Option<u64>,
    pub at: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Device {
    Phone,
    Desk,
    #[default]
    Other,
}

/// The last time the person looked at a page (Quietdeck, a relay, a dashboard).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Seen {
    pub at: Timestamp,
    pub via: String,
    pub device: Device,
    pub by: String,
}

/// What the person said, through any session: "I'm going out for lunch".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stated {
    pub state: State,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub set_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<Timestamp>,
    pub by: String,
    /// A desk has shown them gone since they said it, so the next input at a desk means they're back.
    #[serde(default)]
    pub left: bool,
}

/// A planned absence: a holiday, a day out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub id: u64,
    pub from: Timestamp,
    pub to: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub by: String,
    pub added_at: Timestamp,
}

/// A desk report older than this says nothing (the machine sleeps, or its source stopped): presence never sticks.
pub const DESK_FRESH: SignedDuration = SignedDuration::from_mins(3);
/// A desk idle this long (or locked, or silent) after a stated away has shown the person gone.
pub const LEFT_AFTER: SignedDuration = SignedDuration::from_mins(5);
/// A look at a page on a desk counts as present this long.
pub const DESK_SEEN_FOR: SignedDuration = SignedDuration::from_mins(5);

pub struct Look<'a> {
    pub person: &'a Person,
    pub desks: &'a [Desk],
    pub seen: Option<&'a Seen>,
    pub stated: Option<&'a Stated>,
    pub plans: &'a [Plan],
    pub now: Timestamp,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub state: State,
    /// One line for a person or an agent: what decided it.
    pub why: String,
    pub until: Option<Timestamp>,
    pub note: Option<String>,
    /// A stated away no longer holds: input at a desk after the desk showed them gone.
    pub back_at_desk: bool,
}

/// The person's desks that reported lately.
fn fresh<'a>(l: &'a Look) -> impl Iterator<Item = &'a Desk> {
    l.desks.iter().filter(move |d| l.person.desks.contains(&d.node) && l.now.duration_since(d.at) <= DESK_FRESH)
}

/// A desk in use now: unlocked, with input within `limit`.
fn active<'a>(l: &'a Look, limit: SignedDuration) -> Option<&'a Desk> {
    fresh(l)
        .filter(|d| d.locked != Some(true))
        .filter(|d| d.idle_seconds.is_some_and(|i| i64::try_from(i).unwrap_or(i64::MAX) < limit.as_secs()))
        .min_by_key(|d| d.idle_seconds)
}

/// Whether the desks show the person gone now (for a stated away): none is in use with input in the last 5 minutes.
pub fn desks_show_gone(l: &Look) -> bool {
    active(l, LEFT_AFTER).is_none()
}

pub fn judge(l: &Look) -> Verdict {
    let p = l.person;
    let v = |state, why: String, until, note: Option<String>| Verdict { state, why, until, note, back_at_desk: false };
    let ago = |t: Timestamp| words(l.now.duration_since(t).max(SignedDuration::ZERO));

    // 1. Present.
    if let Some(d) = active(l, p.away_after) {
        let blocked = l.stated.is_some_and(|s| !s.left);
        if !blocked {
            let idle = SignedDuration::from_secs(i64::try_from(d.idle_seconds.unwrap_or(0)).unwrap_or(0));
            let mut verdict =
                v(State::Present, format!("in use at {} (last input {} ago)", d.node, words(idle)), None, None);
            verdict.back_at_desk = l.stated.is_some();
            return verdict;
        }
    }
    if let Some(s) = l.seen.filter(|s| s.device == Device::Desk && l.now.duration_since(s.at) <= DESK_SEEN_FOR)
        && l.stated.is_none()
    {
        return v(
            State::Present,
            format!("looked at {} on a desk {} ago (reported by {})", s.via, ago(s.at), s.by),
            None,
            None,
        );
    }

    // 2. Semi-present: a look at the phone, whatever else holds.
    let said = l.stated.map(|s| {
        let note = s.note.as_ref().map(|n| format!(" (note {})", crate::engine::quote(n))).unwrap_or_default();
        let until = s.until.map(|u| format!(" until {}", local(p, u, l.now))).unwrap_or_default();
        format!("said {}{note}{until} at {} via {}", s.state.words(), local(p, s.set_at, l.now), s.by)
    });
    if let Some(s) = l.seen.filter(|s| s.device != Device::Desk && l.now.duration_since(s.at) <= p.seen_for) {
        let device = if s.device == Device::Phone { "the phone" } else { "a device" };
        let also = said.as_ref().map(|w| format!("; {w}")).unwrap_or_default();
        return v(
            State::SemiPresent,
            format!("looked at {} on {device} {} ago (reported by {}){also}", s.via, ago(s.at), s.by),
            None,
            None,
        );
    }

    // 3. What they said.
    if let (Some(s), Some(w)) = (l.stated, said) {
        return v(s.state, w, s.until, s.note.clone());
    }

    // 4. A planned absence.
    if let Some(plan) = l.plans.iter().find(|x| x.from <= l.now && l.now < x.to) {
        let note = plan.note.as_ref().map(|n| format!(" (note {})", crate::engine::quote(n))).unwrap_or_default();
        return v(
            State::Away,
            format!("planned absence{note} until {}", local(p, plan.to, l.now)),
            Some(plan.to),
            plan.note.clone(),
        );
    }

    // 5. Night where they are.
    let desks = desk_words(l);
    if let Some(end) = night_end(p, l.now) {
        let (a, b) = p.night.expect("a night");
        return v(
            State::Asleep,
            format!("night in {} ({}-{}); {desks}", p.tz_name, a.strftime("%H:%M"), b.strftime("%H:%M")),
            Some(end),
            None,
        );
    }

    // 6. Nothing says they're here.
    v(State::Away, desks, None, None)
}

/// What the desks say, for a reason line: "laptop locked", "laptop idle 40 min", "laptop silent since 18:02".
fn desk_words(l: &Look) -> String {
    let p = l.person;
    if p.desks.is_empty() {
        return "no desk source is set up".into();
    }
    let parts: Vec<String> = p
        .desks
        .iter()
        .map(|node| match l.desks.iter().find(|d| &d.node == node) {
            None => format!("{node} hasn't reported"),
            Some(d) if l.now.duration_since(d.at) > DESK_FRESH => {
                format!("{node} silent since {}", local(p, d.at, l.now))
            }
            Some(d) if d.locked == Some(true) => format!("{node} locked"),
            Some(d) => match d.idle_seconds {
                Some(i) => format!("{node} idle {}", words(SignedDuration::from_secs(i64::try_from(i).unwrap_or(0)))),
                None => format!("{node} unreadable"),
            },
        })
        .collect();
    parts.join(", ")
}

/// When the person's night ends, if it's night for them now.
pub fn night_end(p: &Person, now: Timestamp) -> Option<Timestamp> {
    let (start, end) = p.night?;
    let z = now.to_zoned_datetime(p.tz.clone());
    let t = z.time();
    let at =
        |date: jiff::civil::Date| date.to_datetime(end).to_zoned_datetime(p.tz.clone()).ok().map(|z| z.timestamp());
    if start < end {
        (start <= t && t < end).then(|| at(z.date())).flatten()
    } else if t >= start {
        at(z.date().tomorrow().ok()?)
    } else if t < end {
        at(z.date())
    } else {
        None
    }
}

/// A note as it's quoted in a reason line: JSON-escaped, so a note can't close its own quotes and pass for the
/// authority's words.
pub fn quote(n: &str) -> String {
    serde_json::to_string(n).unwrap_or_default()
}

/// A time as the person reads it: `13:05` today, `Mon 5 Jan 13:05` another day.
pub fn local(p: &Person, t: Timestamp, now: Timestamp) -> String {
    let z: ZonedDateTime = t.to_zoned_datetime(p.tz.clone());
    if z.date() == now.to_zoned_datetime(p.tz.clone()).date() {
        z.strftime("%H:%M").to_string()
    } else {
        z.strftime("%a %-d %b %H:%M").to_string()
    }
}
