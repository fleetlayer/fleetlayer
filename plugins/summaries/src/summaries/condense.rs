//! The "worth a call" threshold, the 48,000-character condensed log and the day/week chunks. Ported from
//! claude-summarizer's `transcripts.py`.

use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};

use super::transcript::{Event, Kind};

/// Condensed activity sent per summary call, in characters (about 4 per token).
pub const BUDGET_CHARS: usize = 48_000;
/// A chunk with no message from the user (or another session) needs this much assistant text, or a file edit, before
/// it's worth a call.
pub const MIN_ASSISTANT_CHARS: usize = 300;

fn chars(s: &str) -> usize {
    s.chars().count()
}

/// Worth a model call: a message from the user or another session, 300+ characters of agent text, or a file edit.
pub fn is_real(events: &[Event]) -> bool {
    if events.iter().any(|e| matches!(e.kind, Kind::Human | Kind::Peer)) {
        return true;
    }
    let assistant: usize = events.iter().filter(|e| e.kind == Kind::Assistant).map(|e| chars(&e.text)).sum();
    if assistant >= MIN_ASSISTANT_CHARS {
        return true;
    }
    events.iter().any(|e| {
        e.kind == Kind::Tool && ["Edit ", "Write ", "MultiEdit ", "NotebookEdit "].iter().any(|p| e.text.starts_with(p))
    })
}

/// When the session last did something a person would call activity, for the timeline and "last active".
pub fn last_real_ts(events: &[Event]) -> Option<Timestamp> {
    events
        .iter()
        .filter(|e| matches!(e.kind, Kind::Human | Kind::Peer) || (e.kind == Kind::Assistant && chars(&e.text) >= 80))
        .map(|e| e.ts)
        .max()
}

fn limit(kind: Kind) -> usize {
    match kind {
        Kind::Human => 2500,
        Kind::Peer | Kind::Assistant => 1500,
        Kind::Tool => 160,
    }
}

fn shrunk(kind: Kind) -> usize {
    match kind {
        Kind::Human => 700,
        _ => 500,
    }
}

fn label(kind: Kind) -> &'static str {
    match kind {
        Kind::Human => "USER",
        Kind::Peer => "FROM ANOTHER SESSION",
        Kind::Assistant => "AGENT",
        Kind::Tool => "TOOL",
    }
}

pub fn clip(text: &str, n: usize) -> String {
    let len = chars(text);
    if len <= n {
        text.to_owned()
    } else {
        format!("{} […{} chars cut]", text.chars().take(n).collect::<String>(), len - n)
    }
}

/// `Sat 24 Oct 00:00` in the machine's time zone.
pub fn stamp(ts: Timestamp, tz: &TimeZone) -> String {
    ts.to_zoned_datetime(tz.clone()).strftime("%a %d %b %H:%M").to_string()
}

/// The user's messages, the agent's text and one line per tool call, newest kept first when over budget: tool lines
/// go first, then older agent text, then older messages from the user.
pub fn condense(events: &[Event], budget: usize, tz: &TimeZone) -> String {
    let full = |e: &Event, n: usize, stamp: &str| format!("[{stamp}] {}: {}", label(e.kind), clip(&e.text, n));
    let mut items: Vec<Option<String>> = Vec::with_capacity(events.len());
    let stamps: Vec<String> = events.iter().map(|e| stamp(e.ts, tz)).collect();
    for (e, s) in events.iter().zip(&stamps) {
        items.push(Some(if e.kind == Kind::Tool {
            format!("  · {}", clip(&e.text, limit(Kind::Tool)))
        } else {
            full(e, limit(e.kind), s)
        }));
    }
    let mut total: usize = items.iter().flatten().map(|i| chars(i) + 1).sum();

    let mut drop = |kinds: &[Kind], shrink: bool, items: &mut Vec<Option<String>>| {
        for (idx, e) in events.iter().enumerate() {
            if total <= budget {
                return;
            }
            let Some(item) = &items[idx] else { continue };
            if !kinds.contains(&e.kind) {
                continue;
            }
            let before = chars(item) + 1;
            if shrink {
                let new = full(e, shrunk(e.kind), &stamps[idx]);
                total = total - before + chars(&new) + 1;
                items[idx] = Some(new);
            } else {
                total -= before;
                items[idx] = None;
            }
        }
    };
    drop(&[Kind::Tool], false, &mut items);
    drop(&[Kind::Assistant, Kind::Peer], true, &mut items);
    drop(&[Kind::Assistant, Kind::Peer], false, &mut items);
    drop(&[Kind::Human], true, &mut items);
    drop(&[Kind::Human], false, &mut items);

    let mut lines: Vec<String> = Vec::new();
    let mut gap = 0;
    for item in items {
        match item {
            None => gap += 1,
            Some(line) => {
                if gap > 0 {
                    lines.push(format!("  [… {gap} earlier lines left out …]"));
                    gap = 0;
                }
                lines.push(line);
            }
        }
    }
    // The "left out" markers can push it over: trim the oldest lines.
    let mut size: usize = lines.iter().map(|l| chars(l) + 1).sum();
    let mut start = 0;
    while lines.len() - start > 1 && size > budget {
        size -= chars(&lines[start]) + 1;
        start += 1;
    }
    lines[start..].join("\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Period {
    Day(jiff::civil::Date),
    Week(i16, i8),
}

/// One chunk per local day for the last `daily_days` days, one per ISO week before that: each chunk is one summary
/// call and one point on the session's timeline.
pub fn chunk_by_period(events: &[Event], daily_days: i64, now: Timestamp, tz: &TimeZone) -> Vec<Vec<Event>> {
    let cutoff = now - SignedDuration::from_hours(24 * daily_days);
    let mut chunks: Vec<Vec<Event>> = Vec::new();
    let mut key = None;
    for e in events {
        let date = e.ts.to_zoned_datetime(tz.clone()).date();
        let k = if e.ts >= cutoff {
            Period::Day(date)
        } else {
            let w = date.iso_week_date();
            Period::Week(w.year(), w.week())
        };
        if Some(k) != key {
            chunks.push(Vec::new());
            key = Some(k);
        }
        chunks.last_mut().expect("pushed above").push(e.clone());
    }
    chunks
}

/// "Sat 24 Oct 2026 00:00 to 19:53", or with the end's day when the span crosses midnight.
pub fn span(chunk: &[Event], tz: &TimeZone) -> String {
    let (Some(a), Some(b)) = (chunk.first(), chunk.last()) else {
        return String::new();
    };
    let (a, b) = (a.ts.to_zoned_datetime(tz.clone()), b.ts.to_zoned_datetime(tz.clone()));
    let end =
        if a.date() == b.date() { b.strftime("%H:%M").to_string() } else { b.strftime("%a %d %b %H:%M").to_string() };
    format!("{} to {end}", a.strftime("%a %d %b %Y %H:%M"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tz() -> TimeZone {
        TimeZone::get("Pacific/Auckland").unwrap()
    }

    fn ev(kind: Kind, text: &str, ts: &str) -> Event {
        Event { kind, text: text.into(), ts: ts.parse().unwrap(), end: 0, uuid: None }
    }

    #[test]
    fn threshold() {
        let t = "2026-11-23T01:00:00Z";
        assert!(!is_real(&[]));
        assert!(is_real(&[ev(Kind::Human, "hi", t)]));
        assert!(is_real(&[ev(Kind::Peer, "hi", t)]));
        assert!(!is_real(&[ev(Kind::Assistant, &"x".repeat(299), t), ev(Kind::Tool, "Bash: ls", t)]));
        assert!(is_real(&[ev(Kind::Assistant, &"x".repeat(150), t), ev(Kind::Assistant, &"y".repeat(150), t)]));
        assert!(is_real(&[ev(Kind::Tool, "Write ~/a.md", t)]));
        assert!(!is_real(&[ev(Kind::Tool, "WebFetch: Edit me", t)]));
    }

    #[test]
    fn last_real_ignores_short_agent_text_and_tools() {
        let events = [
            ev(Kind::Human, "go", "2026-11-23T01:00:00Z"),
            ev(Kind::Assistant, &"x".repeat(80), "2026-11-23T02:00:00Z"),
            ev(Kind::Assistant, "ok", "2026-11-23T03:00:00Z"),
            ev(Kind::Tool, "Bash: ls", "2026-11-23T04:00:00Z"),
        ];
        assert_eq!(last_real_ts(&events), Some("2026-11-23T02:00:00Z".parse().unwrap()));
    }

    #[test]
    fn condense_formats_lines_in_local_time() {
        let events =
            [ev(Kind::Human, "fix it", "2026-11-23T01:05:00Z"), ev(Kind::Tool, "Bash: ls", "2026-11-23T01:06:00Z")];
        assert_eq!(condense(&events, BUDGET_CHARS, &tz()), "[Mon 23 Nov 14:05] USER: fix it\n  · Bash: ls");
        let long = ev(Kind::Human, &"a".repeat(2600), "2026-11-23T01:05:00Z");
        assert!(condense(&[long], BUDGET_CHARS, &tz()).ends_with(&format!("{} […100 chars cut]", "a".repeat(10))));
    }

    #[test]
    fn condense_drops_tools_then_shrinks_and_drops_older_agent_text_then_user_messages() {
        let t = "2026-11-23T01:00:00Z";
        let mut events = vec![];
        for i in 0..40 {
            events.push(ev(Kind::Human, &format!("{i}{}", "u".repeat(2400)), t));
            events.push(ev(Kind::Assistant, &"a".repeat(1400), t));
            events.push(ev(Kind::Tool, "Bash: ls", t));
        }
        let out = condense(&events, BUDGET_CHARS, &tz());
        assert!(out.chars().count() <= BUDGET_CHARS);
        assert!(!out.contains("· Bash"), "tool lines go first");
        assert!(out.contains("left out"));
        // The newest user message survives whole.
        assert!(
            out.ends_with(&format!("USER: 39{}", "u".repeat(2400)))
                || out.contains(&format!("USER: 39{}", "u".repeat(2400)))
        );
        // Small enough logs keep everything.
        let few = &events[..6];
        assert_eq!(condense(few, BUDGET_CHARS, &tz()).lines().count(), 6);
    }

    #[test]
    fn condense_never_exceeds_the_budget_with_markers() {
        let t = "2026-11-23T01:00:00Z";
        let events: Vec<Event> =
            (0..5000).map(|i| ev(if i % 2 == 0 { Kind::Human } else { Kind::Tool }, &"h".repeat(30), t)).collect();
        for budget in [100, 1000, 48_000] {
            let out = condense(&events, budget, &tz());
            assert!(out.chars().count() <= budget, "{budget}: {}", out.chars().count());
        }
    }

    #[test]
    fn chunks_by_day_then_iso_week() {
        let now: Timestamp = "2026-11-23T01:00:00Z".parse().unwrap();
        let events = [
            ev(Kind::Human, "a", "2026-08-03T01:00:00Z"), // Mon 3 Aug, ISO week 32
            ev(Kind::Human, "b", "2026-08-08T01:00:00Z"), // Sat 8 Aug, week 32
            ev(Kind::Human, "c", "2026-08-10T01:00:00Z"), // week 33
            ev(Kind::Human, "d", "2026-11-22T01:00:00Z"),
            ev(Kind::Human, "e", "2026-11-22T10:59:00Z"), // 23:59 NZDT, same local day
            ev(Kind::Human, "f", "2026-11-22T11:01:00Z"), // 00:01 NZDT on the 23rd
        ];
        let chunks = chunk_by_period(&events, 30, now, &tz());
        let texts: Vec<Vec<&str>> = chunks.iter().map(|c| c.iter().map(|e| e.text.as_str()).collect()).collect();
        assert_eq!(texts, vec![vec!["a", "b"], vec!["c"], vec!["d", "e"], vec!["f"]]);
    }

    #[test]
    fn span_text() {
        let a = ev(Kind::Human, "a", "2026-10-23T11:00:00Z");
        let b = ev(Kind::Human, "b", "2026-10-24T06:53:00Z");
        assert_eq!(span(&[a.clone(), b.clone()], &tz()), "Sat 24 Oct 2026 00:00 to 19:53");
        let c = ev(Kind::Human, "c", "2026-10-24T11:30:00Z");
        assert_eq!(span(&[a, c], &tz()), "Sat 24 Oct 2026 00:00 to Sun 25 Oct 00:30");
    }
}
