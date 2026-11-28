//! The presence authority's settings file (`~/.config/fleetlayer/presence.yaml` by default):
//!
//! ```yaml
//! people:
//!   alice:
//!     timezone: Europe/London       # default: the authority machine's time zone
//!     night: "23:00-08:00"          # asleep unless something says otherwise; "off" for none
//!     desks: [laptop]               # nodes whose desk source reports for this person
//!     away_after: 15m               # an unlocked desk idle this long no longer counts as present
//!     seen_for: 10m                 # a look at a page on the phone counts as semi-present this long
//! ```

use std::collections::BTreeMap;

use jiff::SignedDuration;
use jiff::civil::Time;
use jiff::tz::TimeZone;
use serde::Deserialize;

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    people: BTreeMap<String, RawPerson>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawPerson {
    timezone: Option<String>,
    night: Option<String>,
    #[serde(default)]
    desks: Vec<String>,
    away_after: Option<String>,
    seen_for: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Person {
    pub name: String,
    pub tz: TimeZone,
    /// Its IANA name, for people to read.
    pub tz_name: String,
    /// The night's start and end, local time; none for no night.
    pub night: Option<(Time, Time)>,
    pub desks: Vec<String>,
    pub away_after: SignedDuration,
    pub seen_for: SignedDuration,
}

#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub people: BTreeMap<String, Person>,
}

impl Settings {
    /// The person a call names, or the only one when it names none.
    pub fn person(&self, name: Option<&str>) -> Result<&Person, String> {
        match name.filter(|n| !n.is_empty()) {
            Some(n) => self
                .people
                .get(n)
                .ok_or_else(|| format!("no person called {n} is set up here (people: {})", self.names().join(", "))),
            None => match self.people.len() {
                1 => Ok(self.people.values().next().expect("one person")),
                0 => Err("no people are set up in the presence settings yet".into()),
                _ => Err(format!("several people are set up ({}): name one with --person", self.names().join(", "))),
            },
        }
    }

    pub fn names(&self) -> Vec<String> {
        self.people.keys().cloned().collect()
    }
}

pub const DEFAULT_NIGHT: &str = "23:00-08:00";
pub const DEFAULT_AWAY_AFTER: SignedDuration = SignedDuration::from_mins(15);
pub const DEFAULT_SEEN_FOR: SignedDuration = SignedDuration::from_mins(10);

/// A person's name: what `person:<name>` and tool targets carry.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
}

/// Every problem in the file at once, one per line.
pub fn parse(text: &str) -> Result<Settings, String> {
    let file: File =
        if text.trim().is_empty() { File::default() } else { yaml_serde::from_str(text).map_err(|e| e.to_string())? };
    let mut problems = vec![];
    let mut people = BTreeMap::new();
    for (name, raw) in file.people {
        let mut bad = |m: String| problems.push(format!("{name}: {m}"));
        if !valid_name(&name) {
            bad("a name is 1 to 64 letters, digits and - _ . (it's the person's handle)".into());
            continue;
        }
        let (tz, tz_name) = match &raw.timezone {
            Some(z) => match TimeZone::get(z) {
                Ok(tz) => (tz, z.clone()),
                Err(e) => {
                    bad(format!("timezone {z:?}: {e}"));
                    continue;
                }
            },
            None => {
                let tz = TimeZone::system();
                let n = tz.iana_name().unwrap_or("UTC").to_string();
                (tz, n)
            }
        };
        let night = match raw.night.as_deref().unwrap_or(DEFAULT_NIGHT) {
            "off" | "none" => None,
            s => match parse_window(s) {
                Ok(w) => Some(w),
                Err(e) => {
                    bad(e);
                    None
                }
            },
        };
        let mut dur = |field: &str, v: &Option<String>, default| match v {
            None => default,
            Some(s) => match parse_duration(s) {
                Ok(d) => d,
                Err(e) => {
                    problems.push(format!("{name}: {field}: {e}"));
                    default
                }
            },
        };
        let away_after = dur("away_after", &raw.away_after, DEFAULT_AWAY_AFTER);
        let seen_for = dur("seen_for", &raw.seen_for, DEFAULT_SEEN_FOR);
        for d in &raw.desks {
            if d.is_empty() || d.contains(char::is_whitespace) {
                problems.push(format!("{name}: desks: {d:?} isn't a node name"));
            }
        }
        people.insert(name.clone(), Person { name, tz, tz_name, night, desks: raw.desks, away_after, seen_for });
    }
    if problems.is_empty() { Ok(Settings { people }) } else { Err(problems.join("\n")) }
}

/// `23:00-08:00`: a start and an end, local time; the end may be the next day.
pub fn parse_window(s: &str) -> Result<(Time, Time), String> {
    let bad = || format!("night {s:?}: write it as two times, \"23:00-08:00\", or \"off\"");
    let (a, b) = s.split_once('-').ok_or_else(bad)?;
    let t = |x: &str| x.trim().parse::<Time>().map_err(|_| bad());
    let (a, b) = (t(a)?, t(b)?);
    if a == b {
        return Err(bad());
    }
    Ok((a, b))
}

/// `30m`, `2h`, `1h30m`, `1d`: at least a minute.
pub fn parse_duration(s: &str) -> Result<SignedDuration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("an empty duration".into());
    }
    let mut total: i64 = 0;
    let mut digits = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            'w' => 604_800,
            _ => return Err(format!("'{s}' isn't a duration: use m, h, d or w, as in 30m, 2h or 1h30m")),
        };
        let n: i64 = digits.parse().map_err(|_| format!("'{s}' isn't a duration: a number goes before each unit"))?;
        digits.clear();
        total = n.checked_mul(unit).and_then(|v| total.checked_add(v)).ok_or_else(|| format!("'{s}' is too long"))?;
    }
    if !digits.is_empty() {
        return Err(format!("'{s}' needs a unit after its last number (m, h, d or w)"));
    }
    if total < 60 {
        return Err(format!("'{s}' is shorter than a minute"));
    }
    Ok(SignedDuration::from_secs(total))
}

/// `2h`, `1h30m`, `45m`, `3d`.
pub fn words(d: SignedDuration) -> String {
    let s = d.as_secs().max(0);
    let (days, h, m) = (s / 86_400, (s % 86_400) / 3600, (s % 3600) / 60);
    match (days, h, m) {
        (0, 0, m) => format!("{m} min"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h{m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d{h}h"),
    }
}
