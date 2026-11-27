//! The scheduler's settings file (`~/.config/fleetlayer/scheduler.yaml` by default):
//!
//! ```yaml
//! alert_after: 2            # failed fires in a row before the plugin reports a problem
//! entries:
//!   - name: worker check
//!     to: worker@server
//!     every: 3h
//!     text: Check the queue and fix anything stuck.
//!   - name: weekday plan
//!     to: planning@server
//!     cron: "0 9 * * 1-5"
//!     tz: Europe/London
//!     text: Plan the day.
//!     if_busy: skip
//! ```

use std::collections::HashSet;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::Deserialize;

use crate::schedule::{Cron, Schedule, parse_duration};

pub const DEFAULT_ALERT_AFTER: u32 = 2;
const MAX_NAME: usize = 80;
const MAX_TEXT: usize = 16 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    alert_after: Option<u32>,
    #[serde(default)]
    entries: Vec<RawEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEntry {
    name: String,
    to: String,
    text: String,
    every: Option<String>,
    cron: Option<String>,
    at: Option<String>,
    tz: Option<String>,
    #[serde(default)]
    if_busy: IfBusy,
    #[serde(default = "yes")]
    catch_up: bool,
    #[serde(default = "yes")]
    enabled: bool,
}

fn yes() -> bool {
    true
}

/// What to do when the target is running but waiting on a person (a permission prompt or a question).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IfBusy {
    /// Try again every minute until it takes the message, or the next fire replaces it.
    #[default]
    Queue,
    /// Skip this fire.
    Skip,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub to: String,
    pub text: String,
    pub schedule: Schedule,
    pub if_busy: IfBusy,
    /// After downtime, send one message for the fires missed (true), or none and wait for the next (false).
    pub catch_up: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub alert_after: u32,
    pub entries: Vec<Entry>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { alert_after: DEFAULT_ALERT_AFTER, entries: vec![] }
    }
}

/// Read a settings file's text. `default_tz` is for cron entries without `tz`. Every problem is reported at once, one
/// per line, so a person fixes them in one go.
pub fn parse(text: &str, default_tz: &TimeZone) -> Result<Settings, String> {
    if text.trim().is_empty() {
        return Ok(Settings::default());
    }
    let file: File = yaml_serde::from_str(text).map_err(|e| e.to_string())?;
    let mut problems = vec![];
    let mut names = HashSet::new();
    let mut entries = vec![];
    for (i, raw) in file.entries.into_iter().enumerate() {
        let label = if raw.name.trim().is_empty() { format!("entry {}", i + 1) } else { format!("'{}'", raw.name) };
        if !raw.name.trim().is_empty() && !names.insert(raw.name.trim().to_string()) {
            problems.push(format!("{label}: another entry has that name"));
        }
        match entry(raw, default_tz) {
            Ok(e) => entries.push(e),
            Err(e) => problems.push(format!("{label}: {e}")),
        }
    }
    if file.alert_after == Some(0) {
        problems.push("alert_after: at least 1".into());
    }
    if !problems.is_empty() {
        return Err(problems.join("\n"));
    }
    Ok(Settings { alert_after: file.alert_after.unwrap_or(DEFAULT_ALERT_AFTER), entries })
}

fn entry(raw: RawEntry, default_tz: &TimeZone) -> Result<Entry, String> {
    let name = raw.name.trim().to_string();
    if name.is_empty() {
        return Err("a name is needed".into());
    }
    if name.len() > MAX_NAME || name.chars().any(char::is_control) {
        return Err(format!("a name is one line of at most {MAX_NAME} characters"));
    }
    let to = raw.to.trim().to_string();
    if to.is_empty() || to.contains('\n') {
        return Err("'to' is a session: <tab>@<node>, <tab> or a session ID".into());
    }
    if raw.text.trim().is_empty() {
        return Err("'text' is empty".into());
    }
    if raw.text.len() > MAX_TEXT {
        return Err(format!("'text' is longer than {} KB", MAX_TEXT / 1024));
    }
    let tz = match &raw.tz {
        Some(name) => TimeZone::get(name).map_err(|_| format!("'{name}' isn't a time zone (Europe/London, UTC…)"))?,
        None => default_tz.clone(),
    };
    let schedule = match (raw.every, raw.cron, raw.at) {
        (Some(every), None, None) => {
            if raw.tz.is_some() {
                return Err("'tz' goes with 'cron'; an interval is the same in every zone".into());
            }
            Schedule::Every(parse_duration(&every)?)
        }
        (None, Some(cron), None) => Schedule::Cron { cron: Box::new(Cron::parse(&cron)?), tz },
        (None, None, Some(at)) => Schedule::At(parse_at(&at, &tz)?),
        (None, None, None) => return Err("say when: 'every' (3h), 'cron' (\"0 */3 * * *\") or 'at' (a moment)".into()),
        _ => return Err("only one of 'every', 'cron' and 'at'".into()),
    };
    Ok(Entry { name, to, text: raw.text, schedule, if_busy: raw.if_busy, catch_up: raw.catch_up, enabled: raw.enabled })
}

/// `2026-11-28T15:00:00Z`, or a time with an offset, or a civil `2026-11-28 15:00` read in `tz`.
fn parse_at(s: &str, tz: &TimeZone) -> Result<Timestamp, String> {
    if let Ok(t) = s.parse::<Timestamp>() {
        return Ok(t);
    }
    if let Ok(dt) = s.parse::<jiff::civil::DateTime>() {
        return tz
            .to_ambiguous_timestamp(dt)
            .compatible()
            .map_err(|e| format!("'{s}' in {}: {e}", tz.iana_name().unwrap_or("this zone")));
    }
    Err(format!("'{s}' isn't a moment: write 2026-11-28T15:00:00Z, or 2026-11-28 15:00 with 'tz'"))
}
