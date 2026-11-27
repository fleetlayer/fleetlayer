//! When an entry fires: a fixed interval (`every: 3h`), a cron expression in a time zone (`cron: "0 9 * * 1-5"`), or
//! once at a moment (`at: 2026-11-28T15:00:00Z`).

use std::fmt;

use jiff::civil::{Date, DateTime, Time};
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};

/// The most fires counted between two moments; past it, the count stops (a schedule that fires every minute and a
/// scheduler that was off for years).
const COUNT_CAP: u32 = 10_000;

#[derive(Debug, Clone)]
pub enum Schedule {
    Every(SignedDuration),
    Cron { cron: Box<Cron>, tz: TimeZone },
    At(Timestamp),
}

impl Schedule {
    /// The first fire strictly after `t`, if there's one.
    pub fn next_after(&self, t: Timestamp, base: Timestamp) -> Option<Timestamp> {
        match self {
            // Intervals count from the entry's base (the last fire it handled), so they never drift.
            Schedule::Every(d) => {
                if t < base {
                    return base.checked_add(*d).ok();
                }
                let n = (t.duration_since(base).as_secs() / d.as_secs()) + 1;
                base.checked_add(SignedDuration::from_secs(d.as_secs().checked_mul(n)?)).ok()
            }
            Schedule::Cron { cron, tz } => cron.next_after(t, tz),
            Schedule::At(at) => (*at > t).then_some(*at),
        }
    }

    /// The fires in `(base, now]`: how many, and the latest.
    pub fn due(&self, base: Timestamp, now: Timestamp) -> Option<(u32, Timestamp)> {
        if let Schedule::Every(d) = self {
            if now <= base {
                return None;
            }
            let n = now.duration_since(base).as_secs() / d.as_secs();
            let latest = base.checked_add(SignedDuration::from_secs(n.checked_mul(d.as_secs())?)).ok()?;
            return (n > 0).then(|| (n.min(i64::from(u32::MAX)) as u32, latest));
        }
        let mut count = 0;
        let mut latest = None;
        let mut t = base;
        while let Some(next) = self.next_after(t, base) {
            if next > now {
                break;
            }
            latest = Some(next);
            t = next;
            if count < COUNT_CAP {
                count += 1;
            } else {
                // Stop counting, and look for the latest from a week before `now`.
                t = t.max(now.saturating_sub(SignedDuration::from_hours(24 * 8)).unwrap_or(now));
            }
        }
        latest.map(|l| (count, l))
    }

    /// How a person would write it: "every 3h", "cron 0 9 * * 1-5 (Europe/London)", "at 2026-11-28 15:00 UTC".
    pub fn describe(&self) -> String {
        match self {
            Schedule::Every(d) => format!("every {}", format_duration(*d)),
            Schedule::Cron { cron, tz } => {
                format!("cron {} ({})", cron.source, tz.iana_name().unwrap_or("UTC"))
            }
            Schedule::At(at) => format!("at {}", at.round(jiff::Unit::Second).unwrap_or(*at)),
        }
    }
}

/// `3h`, `90m`, `1h30m`, `1d`, `2w`: at least a minute.
pub fn parse_duration(s: &str) -> Result<SignedDuration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("an empty interval".into());
    }
    let mut total: i64 = 0;
    let mut digits = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        if c.is_whitespace() {
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            'w' => 604_800,
            _ => return Err(format!("'{s}' isn't an interval: use s, m, h, d or w, as in 3h or 1h30m")),
        };
        let n: i64 = digits.parse().map_err(|_| format!("'{s}' isn't an interval: a number goes before each unit"))?;
        digits.clear();
        total = n.checked_mul(unit).and_then(|v| total.checked_add(v)).ok_or_else(|| format!("'{s}' is too long"))?;
    }
    if !digits.is_empty() {
        return Err(format!("'{s}' needs a unit after its last number (s, m, h, d or w)"));
    }
    if total < 60 {
        return Err(format!("'{s}' is shorter than a minute"));
    }
    Ok(SignedDuration::from_secs(total))
}

pub fn format_duration(d: SignedDuration) -> String {
    let mut s = d.as_secs();
    let mut out = String::new();
    for (unit, len) in [("w", 604_800), ("d", 86_400), ("h", 3600), ("m", 60), ("s", 1)] {
        if s >= len {
            out.push_str(&format!("{}{unit}", s / len));
            s %= len;
        }
    }
    out
}

/// A five-field cron expression (minute, hour, day of month, month, day of week), with `*`, lists, ranges, steps,
/// month and day names, and `@hourly`, `@daily`, `@weekly`, `@monthly`, `@yearly`. As in cron, when both the day of
/// month and the day of week are restricted, a day matching either fires.
#[derive(Clone)]
pub struct Cron {
    pub source: String,
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    days_restricted: bool,
    weekdays_restricted: bool,
}

impl fmt::Debug for Cron {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Cron({})", self.source)
    }
}

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

impl Cron {
    pub fn parse(source: &str) -> Result<Cron, String> {
        let expanded = match source.trim() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            other => other,
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "'{source}' isn't a cron expression: it takes five fields (minute hour day-of-month month day-of-week)"
            ));
        }
        let bad = |what: &str, e: String| format!("'{source}': the {what} field {e}");
        let minutes = field(fields[0], 0, 59, &[]).map_err(|e| bad("minute", e))?;
        let hours = field(fields[1], 0, 23, &[]).map_err(|e| bad("hour", e))?;
        let days = field(fields[2], 1, 31, &[]).map_err(|e| bad("day-of-month", e))?;
        let months = field(fields[3], 1, 12, &MONTHS).map_err(|e| bad("month", e))?;
        let mut weekdays = field(fields[4], 0, 7, &WEEKDAYS).map_err(|e| bad("day-of-week", e))?;
        if weekdays & (1 << 7) != 0 {
            weekdays = (weekdays | 1) & !(1 << 7); // 7 is Sunday too
        }
        Ok(Cron {
            source: source.trim().to_string(),
            minutes,
            hours: hours as u32,
            days: days as u32,
            months: months as u16,
            weekdays: weekdays as u8,
            days_restricted: !fields[2].starts_with('*'),
            weekdays_restricted: !fields[4].starts_with('*'),
        })
    }

    fn day_matches(&self, d: Date) -> bool {
        if self.months & (1 << d.month()) == 0 {
            return false;
        }
        let dom = self.days & (1 << d.day()) != 0;
        let dow = self.weekdays & (1 << d.weekday().to_sunday_zero_offset()) != 0;
        match (self.days_restricted, self.weekdays_restricted) {
            (true, true) => dom || dow,
            (true, false) => dom,
            (false, true) => dow,
            (false, false) => true,
        }
    }

    /// The first moment strictly after `t` that matches, read in `tz`. A time that doesn't exist on a day the clocks
    /// go forward fires at the same offset after the gap, as most crons do.
    pub fn next_after(&self, t: Timestamp, tz: &TimeZone) -> Option<Timestamp> {
        let start = t.to_zoned_datetime(tz.clone()).datetime();
        let mut date = start.date();
        // Five years covers every expression that can match at all (29 February on a Monday, say).
        for _ in 0..(366 * 5 + 2) {
            if self.day_matches(date) {
                for h in 0..24i8 {
                    if self.hours & (1 << h) == 0 {
                        continue;
                    }
                    for m in 0..60i8 {
                        if self.minutes & (1 << m) == 0 {
                            continue;
                        }
                        let dt = DateTime::from_parts(date, Time::new(h, m, 0, 0).ok()?);
                        if dt < start.saturating_sub(SignedDuration::from_hours(3)) {
                            continue;
                        }
                        let Ok(z) = tz.to_ambiguous_timestamp(dt).compatible() else { continue };
                        if z > t {
                            return Some(z);
                        }
                    }
                }
            }
            date = date.tomorrow().ok()?;
        }
        None
    }
}

/// One cron field as a bit set of the values it matches.
fn field(s: &str, min: u8, max: u8, names: &[&str]) -> Result<u64, String> {
    let value = |v: &str| -> Result<u8, String> {
        if let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(v)) {
            // Months are named from 1, days of the week from 0: each field's own minimum.
            return Ok(i as u8 + min);
        }
        let n: u8 = v.parse().map_err(|_| format!("has '{v}', which isn't a number"))?;
        if n < min || n > max {
            return Err(format!("has {n}, outside {min}-{max}"));
        }
        Ok(n)
    };
    let mut bits = 0u64;
    for part in s.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, st)) => {
                let st: u8 = st.parse().map_err(|_| format!("has a step '{st}' that isn't a number"))?;
                if st == 0 {
                    return Err("has a step of 0".into());
                }
                (r, st)
            }
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (value(a)?, value(b)?)
        } else {
            let v = value(range)?;
            // `5/15` means from 5 to the end, every 15.
            (v, if part.contains('/') { max } else { v })
        };
        if lo > hi {
            return Err(format!("has a range {lo}-{hi} that goes backwards"));
        }
        let mut v = lo;
        while v <= hi {
            bits |= 1 << v;
            v = match v.checked_add(step) {
                Some(n) => n,
                None => break,
            };
        }
    }
    if bits == 0 {
        return Err("matches nothing".into());
    }
    Ok(bits)
}
