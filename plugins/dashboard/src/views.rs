//! The pages, as HTML: the overview, machines, sessions, events, access (approvals and admin windows), the graph and
//! readings. Each renders from one `Snapshot`; the page's `<main>` is fetched again every few seconds for the live view.
//!
//! The building blocks are the Claude hub's: a page head with a one-line summary, then sections that are each one
//! bordered list of rows (an icon tile, the text, and what to do or when on the right).

use std::fmt::Write as _;

use crate::icons::icon;
use crate::model::{Event, Node, Session, Snapshot};

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

fn enc(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

pub fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

/// "just now", "4 min ago", "3 h ago", "2 days ago".
pub fn ago(ms: i64, now: i64) -> String {
    let s = (now - ms).max(0) / 1000;
    match s {
        0..=44 => "just now".into(),
        45..=3599 => format!("{} min ago", (s + 30) / 60),
        3600..=86_399 => format!("{} h ago", s / 3600),
        _ => {
            let d = s / 86_400;
            format!("{d} day{} ago", if d == 1 { "" } else { "s" })
        }
    }
}

/// "in 38 min", "in 2 h 5 min".
fn until(ms: i64, now: i64) -> String {
    let s = (ms - now).max(0) / 1000;
    match s {
        0..=59 => "in under a minute".into(),
        60..=3599 => format!("in {} min", (s + 30) / 60),
        _ => format!("in {} h {} min", s / 3600, (s % 3600) / 60),
    }
}

/// "28 Nov 2026", for an expiry.
pub fn until_words(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms).map(|t| t.strftime("%-d %b %Y").to_string()).unwrap_or_default()
}

fn iso_ms(s: &str) -> Option<i64> {
    s.parse::<jiff::Timestamp>().ok().map(|t| t.as_millisecond())
}

fn ago_iso(s: Option<&str>, now: i64) -> String {
    s.and_then(iso_ms).map(|ms| ago(ms, now)).unwrap_or_default()
}

fn time_tag(ms: i64, now: i64) -> String {
    let iso = jiff::Timestamp::from_millisecond(ms).map(|t| t.to_string()).unwrap_or_default();
    format!(r#"<time datetime="{0}" title="{0}">{1}</time>"#, esc(&iso), esc(&ago(ms, now)))
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn status_word(s: &str) -> &str {
    match s {
        "blocked" => "Waiting on you",
        "working" => "Working",
        "idle" => "Idle",
        "done" => "Done",
        _ => "Unknown",
    }
}

fn os_word(os: Option<&str>) -> &'static str {
    match os {
        Some("macos") => "macOS",
        Some("linux") => "Linux",
        Some("windows") => "Windows",
        _ => "",
    }
}

/// A detail as a sentence: a full stop added when it ends without one.
fn sentence(s: &str) -> String {
    let s = s.trim();
    if s.is_empty() || s.ends_with(['.', '!', '?', ':']) { s.to_string() } else { format!("{s}.") }
}

/// What the viewer may do, and the form token for doing it.
pub struct Ctx<'a> {
    pub can_act: bool,
    pub csrf: &'a str,
    pub now: i64,
    /// The passkey step-up page for an approval that opens a role window, when this viewer may open one here.
    pub stepup: &'a dyn Fn(&str) -> Option<String>,
    /// This page's path, for its forms to come back to.
    pub here: &'a str,
}

// ------------------------------------------------------------------ building blocks

/// One notice per part of the control plane that couldn't be read.
pub fn errors(s: &Snapshot) -> String {
    if s.errors.is_empty() {
        return String::new();
    }
    let mut o = format!(
        r#"<div class="notice is-bad" role="alert">{}<div><p class="notice-text"><strong>Some of the fleet can't be read right now.</strong></p><ul>"#,
        icon("alert-triangle")
    );
    for (part, e) in &s.errors {
        let _ = write!(o, "<li><span class=mono>{}</span>: {}</li>", esc(part), esc(e));
    }
    o.push_str("</ul></div></div>");
    o
}

pub fn page_head(title: &str, summary: &str) -> String {
    format!(r#"<div class="page-head"><h1>{}</h1><p class="summary">{}</p></div>"#, esc(title), summary)
}

/// A section's heading over its list: a title, an optional count and an optional link.
pub fn sec_head(id: &str, title: &str, count: Option<usize>, link: Option<(&str, &str)>) -> String {
    format!(
        r#"<div class="sec-head"><h2 id="{id}">{}{}</h2>{}</div>"#,
        esc(title),
        count.map(|n| format!(r#" <span class="count">{n}</span>"#)).unwrap_or_default(),
        link.map(|(href, text)| format!(r#"<a href="{href}">{}</a>"#, esc(text))).unwrap_or_default()
    )
}

/// An icon in a small square: `warn`, `bad`, `accent` or plain.
pub fn tile(name: &str, tone: &str) -> String {
    format!(
        r#"<span class="tile{}">{}</span>"#,
        if tone.is_empty() { String::new() } else { format!(" is-{tone}") },
        icon(name)
    )
}

/// A list with nothing in it: an icon, a line, and what puts something there.
pub fn empty(icon_name: &str, title: &str, body: &str) -> String {
    format!(
        r#"<div class="empty"><span class="empty-icon">{}</span><h3>{}</h3><p>{}</p></div>"#,
        icon(icon_name),
        esc(title),
        body
    )
}

/// A dot that means something: a session's state or a machine's health.
pub fn dot(kind: &str, label: &str) -> String {
    format!(r#"<span class="dot {}" role="img" aria-label="{}" title="{}"></span>"#, esc(kind), esc(label), esc(label))
}

fn session_dot(s: &Session) -> String {
    let st = s.status();
    let kind = match st {
        "blocked" => "warn",
        "working" => "busy",
        "idle" | "done" => "",
        _ => "",
    };
    dot(kind, status_word(st))
}

pub fn addr(a: &str) -> String {
    format!(r#"<span class="addr">{}</span>"#, esc(a))
}

fn verb_words(v: &str) -> String {
    match v {
        "use_tool" => "use".into(),
        "activate" => "open a window for".into(),
        "message" => "message".into(),
        "start" => "start a session on".into(),
        "sync" => "receive".into(),
        "read" => "read".into(),
        "write" => "write to".into(),
        other => other.replace('_', " "),
    }
}

fn approval_buttons(id: &str, verb: &str, c: &Ctx) -> String {
    if !c.can_act {
        return String::new();
    }
    // Opening a role window needs the approver's passkey for that very request (`docs/access-control.md`): approving
    // goes through the sign-in service's step-up page, and comes back to confirm.
    if verb == "activate" {
        let deny = format!(
            r#"<form class="row-actions" method="post" action="/_/approvals/{id}"><input type="hidden" name="csrf" value="{csrf}"><input type="hidden" name="back" value="{here}"><button class="btn btn-sm" name="decision" value="deny">Deny</button>{approve}</form>"#,
            id = esc(&enc(id)),
            csrf = esc(c.csrf),
            here = esc(c.here),
            approve = match (c.stepup)(id) {
                Some(url) => format!(
                    r#"<a class="btn btn-sm btn-primary" href="{}">{}Approve with passkey</a>"#,
                    esc(&url),
                    icon("key")
                ),
                None => String::new(),
            },
        );
        return deny;
    }
    format!(
        r#"<form class="row-actions" method="post" action="/_/approvals/{id}"><input type="hidden" name="csrf" value="{csrf}"><input type="hidden" name="back" value="{here}"><button class="btn btn-sm" name="decision" value="deny">Deny</button><button class="btn btn-sm btn-primary" name="decision" value="approve">{check}Approve</button></form>"#,
        id = esc(&enc(id)),
        csrf = esc(c.csrf),
        here = esc(c.here),
        check = icon("check"),
    )
}

fn close_button(role: &str, c: &Ctx) -> String {
    if !c.can_act {
        return String::new();
    }
    format!(
        r#"<form class="row-actions" method="post" action="/_/windows/{r}/close"><input type="hidden" name="csrf" value="{csrf}"><input type="hidden" name="back" value="{here}"><button class="btn btn-sm">Close now</button></form>"#,
        r = esc(&enc(role)),
        csrf = esc(c.csrf),
        here = esc(c.here),
    )
}

fn channel_icon(channel: &str) -> &'static str {
    match channel {
        "ci" | "github" | "git" => "git-commit",
        "pr" | "review" => "git-pull-request",
        "email" | "mail" => "mail",
        "schedule" | "scheduler" | "cron" => "clock",
        "deploy" | "release" => "rocket",
        "uptime" | "monitor" | "alerts" => "activity",
        "fleet" | "node" | "nodes" => "server",
        "access" | "policy" => "hand-stop",
        _ => "webhook",
    }
}

// ------------------------------------------------------------------ overview

pub fn overview(s: &Snapshot, c: &Ctx) -> String {
    let now = c.now;
    let machines = s.machines();
    let online_machines = machines.iter().filter(|(_, ns)| ns.iter().any(|n| n.online)).count();
    let live: Vec<&Session> = s.live_sessions().collect();
    let working = live.iter().filter(|x| x.status() == "working").count();
    let blocked: Vec<&&Session> = live.iter().filter(|x| x.status() == "blocked").collect();
    let approvals = s.pending_approvals();
    let waiting = blocked.len() + approvals.len();
    let offline: Vec<&Node> = s.nodes.iter().filter(|n| !n.online).collect();
    let plugin_problems = s.plugin_problems();
    let problems = offline.len() + plugin_problems.len();
    let windows = s.open_windows();

    let mut o = errors(s);
    // The same count as the "Needs you" list below: what's waiting, the open windows and the problems.
    let need = waiting + windows.len() + problems;
    let summary = if need == 0 {
        format!(r#"{}Everything's running. Nothing needs you."#, dot("ok", "Healthy"))
    } else {
        let mut parts = vec![];
        if waiting > 0 {
            parts.push(format!("{waiting} waiting for an answer"));
        }
        if !windows.is_empty() {
            parts.push(format!("{} open", plural(windows.len(), "admin window", "admin windows")));
        }
        if problems > 0 {
            parts.push(plural(problems, "problem", "problems"));
        }
        format!(
            "{}{} {}: {}.",
            dot(if problems > 0 { "bad" } else { "warn" }, "Needs a look"),
            plural(need, "thing", "things"),
            if need == 1 { "needs you" } else { "need you" },
            esc(&parts.join(", "))
        )
    };
    o.push_str(&page_head("Fleet", &summary));

    // The four numbers, as one strip.
    let cell = |href: &str, label: &str, value: usize, rest: String, tone: &str| {
        format!(
            r#"<div class="cell{}"><dt><a href="{href}">{}</a></dt><dd><span class="big">{value}</span> <span class="rest">{}</span></dd></div>"#,
            if tone.is_empty() { String::new() } else { format!(" is-{tone}") },
            esc(label),
            esc(&rest)
        )
    };
    let _ = write!(
        o,
        r#"<dl class="strip" aria-label="At a glance">{}{}{}{}</dl>"#,
        cell("/machines", "Machines online", online_machines, format!("of {}", machines.len()), ""),
        cell("/sessions", "Sessions", live.len(), format!("{working} working"), ""),
        cell(
            "/access",
            "Waiting on you",
            waiting,
            format!(
                "{}, {}",
                plural(blocked.len(), "prompt", "prompts"),
                plural(approvals.len(), "request", "requests")
            ),
            if waiting > 0 { "warn" } else { "" }
        ),
        cell(
            "/machines",
            "Problems",
            problems,
            if problems == 0 {
                "all healthy".into()
            } else {
                format!("{} offline, {} failing", offline.len(), plugin_problems.len())
            },
            if problems > 0 { "bad" } else { "" }
        ),
    );

    o.push_str(r#"<div class="split"><div class="split-main">"#);

    // What needs the person, most urgent first.
    if waiting + problems + windows.len() > 0 {
        o.push_str(r#"<section aria-labelledby="needs">"#);
        o.push_str(&sec_head("needs", "Needs you", Some(waiting + problems + windows.len()), None));
        o.push_str(r#"<div class="list">"#);
        for b in &blocked {
            let _ = write!(
                o,
                r#"<div class="row">{}<div class="row-body"><p class="row-title">{} {}</p><p class="row-sub">{}</p></div><div class="row-end"><span class="row-meta">{}</span></div></div>"#,
                tile("message-question", "warn"),
                addr(&b.address()),
                esc(&b.prompt_line().unwrap_or_default()),
                esc(b.sentence().unwrap_or("")),
                esc(&ago_iso(b.meta.status_changed_at.as_deref(), now)),
            );
        }
        for a in &approvals {
            let _ = write!(
                o,
                r#"<div class="row">{}<div class="row-body"><p class="row-title">{} asks to {} {}</p><p class="row-sub">{}</p></div><div class="row-end"><span class="row-meta">{}</span>{}</div></div>"#,
                tile("key", "warn"),
                addr(&a.from),
                esc(&verb_words(&a.verb)),
                addr(&a.to),
                esc(a.detail.as_deref().unwrap_or("")),
                esc(&ago(a.created_at, now)),
                approval_buttons(&a.id, &a.verb, c),
            );
        }
        for w in &windows {
            let _ = write!(
                o,
                r#"<div class="row">{}<div class="row-body"><p class="row-title">The {} window is open</p><p class="row-sub">It closes by itself {}.</p></div><div class="row-end">{}</div></div>"#,
                tile("lock-open", "accent"),
                addr(&w.role),
                esc(&w.ends_at.map(|e| until(e, now)).unwrap_or_default()),
                close_button(&w.role, c),
            );
        }
        for n in &offline {
            let _ = write!(
                o,
                r#"<div class="row">{}<div class="row-body"><p class="row-title">{} is offline</p><p class="row-sub">Last heard from {}.</p></div></div>"#,
                tile("server-off", "bad"),
                addr(&n.name),
                esc(&n.last_seen_at.map(|t| ago(t, now)).unwrap_or_else(|| "never".into())),
            );
        }
        for (node, plugin, why) in &plugin_problems {
            let _ = write!(
                o,
                r#"<div class="row">{}<div class="row-body"><p class="row-title">{} is failing on {}</p><p class="row-sub">{}</p></div></div>"#,
                tile("plug-x", "bad"),
                addr(plugin),
                addr(node),
                esc(why),
            );
        }
        o.push_str("</div></section>");
    }

    // Machines.
    o.push_str(r#"<section aria-labelledby="machines-h">"#);
    o.push_str(&sec_head("machines-h", "Machines", Some(machines.len()), Some(("/machines", "Details"))));
    o.push_str(&machine_list(s, &machines, now, false));
    o.push_str("</section></div>");

    // The side column: what's arriving, and what's being worked on.
    o.push_str(r#"<aside class="split-side">"#);
    o.push_str(r#"<section aria-labelledby="events-h">"#);
    o.push_str(&sec_head("events-h", "Events", None, Some(("/events", "All events"))));
    o.push_str(r#"<div class="list">"#);
    for e in s.events.iter().take(7) {
        o.push_str(&event_row(e, now, true));
    }
    if s.events.is_empty() {
        o.push_str(&empty("webhook", "No events yet", "Webhooks, CI results and schedules show here as they arrive."));
    }
    o.push_str("</div></section>");

    let mut active: Vec<&&Session> = live.iter().filter(|x| x.status() == "working").collect();
    active.sort_by_key(|x| std::cmp::Reverse(x.meta.status_changed_at.clone()));
    o.push_str(r#"<section aria-labelledby="working-h">"#);
    o.push_str(&sec_head("working-h", "Working now", Some(active.len()), Some(("/sessions", "All sessions"))));
    o.push_str(r#"<div class="list">"#);
    for x in active.iter().take(6) {
        let _ = write!(
            o,
            r#"<div class="row row-tight">{}<div class="row-body"><p class="row-title">{}</p><p class="row-sub clamp">{}</p></div></div>"#,
            session_dot(x),
            addr(&x.address()),
            esc(x.sentence().unwrap_or("")),
        );
    }
    if active.is_empty() {
        o.push_str(&empty("terminal-2", "Nothing working right now", "Sessions show here while their agent is busy."));
    }
    o.push_str("</div></section></aside></div>");
    o
}

/// Every machine as a group of rows, one per node (one per Unix user).
fn machine_list(s: &Snapshot, machines: &[(String, Vec<&Node>)], now: i64, detail: bool) -> String {
    if machines.is_empty() {
        return format!(
            r#"<div class="list">{}</div>"#,
            empty(
                "server",
                "No machines yet",
                "Add one with <code>fleetlayer up --join</code>: “Add a machine” in the guides."
            )
        );
    }
    let mut o = String::from(r#"<div class="list">"#);
    for (machine, nodes) in machines {
        let online = nodes.iter().any(|n| n.online);
        let os = nodes.iter().find_map(|n| n.status.as_ref().and_then(|x| x.os.as_deref()));
        let healthy = nodes.iter().all(|n| n.problem().is_none());
        let (d, word) = if !online {
            (dot("bad", "Offline"), "Offline")
        } else if healthy {
            (dot("ok", "Healthy"), "Healthy")
        } else {
            (dot("warn", "Needs a look"), "Needs a look")
        };
        let mut meta = vec![];
        if !os_word(os).is_empty() {
            meta.push(os_word(os).to_string());
        }
        meta.push(plural(nodes.len(), "node", "nodes"));
        let _ = write!(
            o,
            r#"<div class="mgroup{}"><div class="mhead">{d}<h3>{}</h3><span class="mmeta">{}</span><span class="mstate">{word}</span></div>"#,
            if online { "" } else { " is-off" },
            esc(machine),
            esc(&meta.join(" · ")),
        );
        for n in nodes {
            o.push_str(&node_row(s, n, now, detail));
        }
        o.push_str("</div>");
    }
    o.push_str("</div>");
    o
}

fn node_row(s: &Snapshot, n: &Node, now: i64, detail: bool) -> String {
    let id = format!(
        r#"<div class="n-id"><span class="addr">{}</span><span class="sub">{}</span></div>"#,
        esc(&n.name),
        esc(&n.user.as_deref().map(|u| format!("as {u}")).unwrap_or_default())
    );
    let version = format!(
        r#"<div class="n-ver">{}</div>"#,
        esc(&n.version.as_deref().map(|v| format!("v{v}")).unwrap_or_default())
    );
    if !n.online {
        return format!(
            r#"<div class="noderow">{id}<div class="n-sess sub">Offline, last heard from {}.</div>{version}</div>"#,
            esc(&n.last_seen_at.map(|t| ago(t, now)).unwrap_or_else(|| "never".into()))
        );
    }
    // Sessions: a bar of one segment each, waiting first, then working.
    let sessions = s.sessions_on(&n.name);
    let working = sessions.iter().filter(|x| x.status() == "working").count();
    let blocked = sessions.iter().filter(|x| x.status() == "blocked").count();
    let mut sess = String::from(r#"<div class="n-sess">"#);
    if sessions.is_empty() {
        sess.push_str(r#"<span class="sub">No sessions</span>"#);
    } else {
        let mut sorted = sessions.clone();
        sorted.sort_by_key(|x| match x.status() {
            "blocked" => 0,
            "working" => 1,
            "idle" => 2,
            _ => 3,
        });
        let _ = write!(sess, r#"<span class="segs" aria-hidden="true">"#);
        for x in &sorted {
            let _ = write!(sess, r#"<i class="seg-{}"></i>"#, esc(x.status()));
        }
        let mut words = vec![plural(sessions.len(), "session", "sessions")];
        if working > 0 {
            words.push(format!("{working} working"));
        }
        if blocked > 0 {
            words.push(format!("{blocked} waiting"));
        }
        let _ = write!(sess, r#"</span><span class="sub">{}</span>"#, esc(&words.join(", ")));
    }
    sess.push_str("</div>");

    // Plugins: names in a quiet line, a failing one called out.
    let mut plugins = String::from(r#"<div class="n-plug">"#);
    match &n.status {
        Some(st) => {
            let on: Vec<_> = st.plugins.iter().filter(|p| p.enabled).collect();
            let bad: Vec<_> = on.iter().filter(|p| p.health() != "ok").collect();
            let names: Vec<String> = on
                .iter()
                .map(|p| {
                    if p.health() == "ok" {
                        esc(&p.name)
                    } else {
                        format!(
                            r#"<b class="is-{}" title="{}">{}</b>"#,
                            p.health(),
                            esc(&p.problem.clone().or_else(|| p.reason.clone()).unwrap_or_else(|| p.state.clone())),
                            esc(&p.name)
                        )
                    }
                })
                .collect();
            let _ = write!(plugins, r#"<span class="plist">{}</span>"#, names.join(", "));
            let tools: Vec<String> = s.tools_on(&n.name).iter().map(|t| esc(&t.name)).collect();
            if !tools.is_empty() {
                let _ = write!(plugins, r#"<span class="sub">Shares {}</span>"#, tools.join(", "));
            }
            if detail && let Some(p) = bad.first().and_then(|p| p.problem.clone().or_else(|| p.reason.clone())) {
                let _ = write!(plugins, r#"<span class="sub is-bad">{}</span>"#, esc(&p));
            }
        }
        None => plugins.push_str(r#"<span class="sub">Plugins not reported (Fleetlayer before 0.4)</span>"#),
    }
    plugins.push_str("</div>");
    let seen = if detail {
        format!(r#"<div class="n-seen sub">{}</div>"#, esc(&n.last_seen_at.map(|t| ago(t, now)).unwrap_or_default()))
    } else {
        String::new()
    };
    format!(
        r#"<div class="noderow{}">{id}{sess}{plugins}{seen}{version}</div>"#,
        if detail { " is-detail" } else { "" }
    )
}

fn event_row(e: &Event, now: i64, compact: bool) -> String {
    let to = e.delivered_to();
    let to = if to.is_empty() {
        "no one subscribed".to_string()
    } else {
        format!("to {}", to.iter().map(|t| addr(t)).collect::<Vec<_>>().join(", "))
    };
    let from = if compact {
        String::new()
    } else {
        format!(
            " · from {}{}",
            esc(&e.source),
            e.subject.as_deref().map(|x| format!(" · {}", addr(x))).unwrap_or_default()
        )
    };
    format!(
        r#"<div class="row{}">{}<div class="row-body"><p class="row-text">{}</p><p class="row-sub">{}{from} · {to}</p></div></div>"#,
        if compact { " row-tight" } else { "" },
        tile(channel_icon(&e.channel), ""),
        esc(&e.summary),
        time_tag(e.at, now),
    )
}

// ------------------------------------------------------------------ machines

pub fn machines(s: &Snapshot, c: &Ctx) -> String {
    let mut o = errors(s);
    let ms = s.machines();
    o.push_str(&page_head(
        "Machines",
        &esc(&format!(
            "{} with {}. A node is one Unix user's Fleetlayer on a machine.",
            plural(ms.len(), "machine", "machines"),
            plural(s.nodes.len(), "node", "nodes")
        )),
    ));
    o.push_str(&machine_list(s, &ms, c.now, true));
    if !s.tools.is_empty() {
        o.push_str(r#"<section aria-labelledby="tools-h">"#);
        o.push_str(&sec_head("tools-h", "Shared tools", Some(s.tools.len()), None));
        o.push_str(r#"<div class="list">"#);
        for t in &s.tools {
            let line = t
                .not_offered
                .as_deref()
                .map(|w| format!("Not offered now: {w}"))
                .or_else(|| t.description.clone())
                .unwrap_or_default();
            let to = if t.allow.is_empty() { "anyone the policy allows".to_string() } else { t.allow.join(", ") };
            let _ = write!(
                o,
                r#"<div class="row">{}<div class="row-body"><p class="row-title">{}</p><p class="row-sub">{}</p></div><div class="row-end"><span class="row-meta">for {}</span></div></div>"#,
                tile("bolt", if t.online { "" } else { "bad" }),
                addr(&format!("{}@{}", t.name, t.node)),
                esc(&line),
                addr(&to)
            );
        }
        o.push_str("</div></section>");
    }
    o
}

// ------------------------------------------------------------------ sessions

pub fn sessions(s: &Snapshot, c: &Ctx, node: Option<&str>) -> String {
    let mut o = errors(s);
    let live: Vec<&Session> = s.live_sessions().filter(|x| node.is_none_or(|n| x.node == n)).collect();
    let where_ = node.map(|n| format!(" on {n}")).unwrap_or_else(|| " across the fleet".into());
    o.push_str(&page_head(
        "Sessions",
        &esc(&format!("{} running{where_}.", plural(live.len(), "session", "sessions"))),
    ));
    o.push_str(r#"<nav class="filters" aria-label="Filter by node">"#);
    let _ = write!(o, r#"<a href="/sessions"{}>All</a>"#, if node.is_none() { r#" aria-current="page""# } else { "" });
    for n in &s.nodes {
        let count = s.sessions_on(&n.name).len();
        if count == 0 {
            continue;
        }
        let _ = write!(
            o,
            r#"<a href="/sessions?node={}"{}>{} <span class="count">{count}</span></a>"#,
            esc(&enc(&n.name)),
            if node == Some(n.name.as_str()) { r#" aria-current="page""# } else { "" },
            esc(&n.name)
        );
    }
    o.push_str("</nav>");
    for (st, title) in [
        ("blocked", "Waiting on you"),
        ("working", "Working"),
        ("idle", "Idle"),
        ("done", "Done"),
        ("unknown", "Other"),
    ] {
        let mut group: Vec<&&Session> = live.iter().filter(|x| x.status() == st).collect();
        if group.is_empty() {
            continue;
        }
        group.sort_by_key(|x| std::cmp::Reverse(x.meta.status_changed_at.clone()));
        let id = format!("s-{st}");
        let _ = write!(
            o,
            r#"<section aria-labelledby="{id}">{}<div class="list">"#,
            sec_head(&id, title, Some(group.len()), None)
        );
        for x in group {
            let _ = write!(
                o,
                r#"<div class="row">{}<div class="row-body"><p class="row-title">{} <span class="tag">{}</span>{}</p><p class="row-sub">{}</p></div><div class="row-end"><span class="row-meta">{}</span></div></div>"#,
                session_dot(x),
                addr(&x.address()),
                esc(x.harness()),
                x.prompt_line()
                    .map(|p| format!(r#" <span class="tag is-warn">{}</span>"#, esc(&p)))
                    .unwrap_or_default(),
                esc(x.sentence().unwrap_or("")),
                esc(&ago_iso(x.meta.status_changed_at.as_deref().or(x.updated_at.as_deref()), c.now)),
            );
        }
        o.push_str("</div></section>");
    }
    if live.is_empty() {
        let _ = write!(
            o,
            r#"<div class="list">{}</div>"#,
            empty(
                "terminal-2",
                "No sessions running",
                "Start one in your session manager, or with <code>fleetlayer session start</code>."
            )
        );
    }
    o
}

// ------------------------------------------------------------------ events

pub fn events(s: &Snapshot, c: &Ctx) -> String {
    let mut o = errors(s);
    o.push_str(&page_head(
        "Events",
        "The newest hundred: webhooks, CI results, schedules and anything else published to a channel, and who each reached.",
    ));
    o.push_str(r#"<div class="list">"#);
    for e in &s.events {
        o.push_str(&event_row(e, c.now, false));
    }
    if s.events.is_empty() {
        o.push_str(&empty(
            "webhook",
            "No events yet",
            "Point a webhook at the control plane, or subscribe a session to CI results.",
        ));
    }
    o.push_str("</div>");
    o
}

// ------------------------------------------------------------------ access

pub fn access(s: &Snapshot, c: &Ctx) -> String {
    let now = c.now;
    let mut o = errors(s);
    o.push_str(&page_head(
        "Access",
        "Requests waiting for your approval, and roles that stay off until someone opens a window for them.",
    ));
    let pending = s.pending_approvals();
    o.push_str(r#"<section aria-labelledby="req-h">"#);
    o.push_str(&sec_head("req-h", "Requests", Some(pending.len()), None));
    o.push_str(r#"<div class="list">"#);
    for a in &pending {
        let _ = write!(
            o,
            r#"<div class="row">{}<div class="row-body"><p class="row-title">{} asks to {} {}</p><p class="row-sub">{}{}</p></div><div class="row-end"><span class="row-meta">{}</span>{}</div></div>"#,
            tile("key", "warn"),
            addr(&a.from),
            esc(&verb_words(&a.verb)),
            addr(&a.to),
            esc(&sentence(a.detail.as_deref().unwrap_or(""))),
            a.expires_at.map(|e| format!(" Expires {}.", esc(&until(e, now)))).unwrap_or_default(),
            esc(&ago(a.created_at, now)),
            approval_buttons(&a.id, &a.verb, c),
        );
    }
    if pending.is_empty() {
        o.push_str(&empty(
            "circle-check",
            "Nothing waiting",
            "Agents ask here, and on your phone, when the policy says a person decides.",
        ));
    }
    o.push_str("</div>");
    if !c.can_act && !pending.is_empty() {
        o.push_str(r#"<p class="hint">You can see these but not decide them here: that needs <code>admin</code> on the dashboard in the access policy, and the dashboard's delegate token (“The dashboard” in the docs).</p>"#);
    }
    o.push_str("</section>");

    o.push_str(r#"<section aria-labelledby="win-h">"#);
    o.push_str(&sec_head("win-h", "Admin windows", None, None));
    o.push_str(r#"<div class="list">"#);
    if s.windows.is_empty() {
        o.push_str(&empty(
            "lock-open",
            "No windowed roles",
            "A role can stay switched off until you open a window for it: “An admin role that's off until you open it” in the guides.",
        ));
    }
    for w in &s.windows {
        let (t, line) = match w.state.as_str() {
            "open" => (
                tile("lock-open", "accent"),
                format!("Open, closes {}.", w.ends_at.map(|e| until(e, now)).unwrap_or_default()),
            ),
            "pending" => (tile("key", "warn"), "Asked for: approve it in Requests.".to_string()),
            _ => (tile("lock-open", ""), format!("Off. Opens for up to {} at a time.", duration_words(w.longest_ms))),
        };
        let _ = write!(
            o,
            r#"<div class="row">{t}<div class="row-body"><p class="row-title">{}</p><p class="row-sub">{}</p></div><div class="row-end">{}</div></div>"#,
            addr(&w.role),
            esc(&line),
            if w.state == "open" { close_button(&w.role, c) } else { String::new() }
        );
    }
    o.push_str("</div></section>");

    if !s.window_history.is_empty() {
        o.push_str(r#"<section aria-labelledby="hist-h">"#);
        o.push_str(&sec_head("hist-h", "Recent windows", None, None));
        o.push_str(r#"<div class="list"><div class="table-wrap"><table><thead><tr><th>Role</th><th>Asked by</th><th>Opened</th><th>Ended</th></tr></thead><tbody>"#);
        for w in s.window_history.iter().take(20) {
            let ended = match (w.closed_at, &w.closed_by) {
                (Some(t), Some(by)) => format!("{} by {}", ago(t, now), by),
                (Some(t), None) => ago(t, now),
                (None, _) if w.ends_at > now => format!("open, closes {}", until(w.ends_at, now)),
                (None, _) => ago(w.ends_at, now),
            };
            let _ = write!(
                o,
                r#"<tr><td>{}</td><td>{}</td><td>{}</td><td>{}{}</td></tr>"#,
                addr(&w.role),
                addr(&w.requested_by),
                time_tag(w.opened_at, now),
                esc(&ended),
                w.problem
                    .as_deref()
                    .map(|p| format!(r#" <span class="tag is-warn">{}</span>"#, esc(p)))
                    .unwrap_or_default()
            );
        }
        o.push_str("</tbody></table></div></div></section>");
    }
    o
}

fn duration_words(ms: i64) -> String {
    let m = ms / 60_000;
    if m >= 120 && m % 60 == 0 {
        format!("{} hours", m / 60)
    } else if m == 60 {
        "an hour".into()
    } else {
        format!("{m} minutes")
    }
}

// ------------------------------------------------------------------ readings

pub fn readings(s: &Snapshot, c: &Ctx) -> String {
    let mut o = errors(s);
    o.push_str(&page_head(
        "Readings",
        "The newest numbers each node publishes: account usage, token health, plugin status and the rest.",
    ));
    let mut kinds: Vec<&str> = s.readings.iter().map(|r| r.kind.as_str()).collect();
    kinds.sort_unstable();
    kinds.dedup();
    for (i, k) in kinds.iter().enumerate() {
        let id = format!("r-{i}");
        let _ = write!(
            o,
            r#"<section aria-labelledby="{id}"><div class="sec-head"><h2 id="{id}" class="mono">{}</h2></div><div class="list"><div class="table-wrap"><table><thead><tr><th>Key</th><th>From</th><th>When</th><th>Reading</th></tr></thead><tbody>"#,
            esc(k)
        );
        for r in s.readings.iter().filter(|r| r.kind == *k) {
            let _ = write!(
                o,
                r#"<tr><td>{}</td><td>{}</td><td class="nowrap">{}</td><td>{}</td></tr>"#,
                addr(&r.key),
                addr(&r.node),
                esc(&ago_iso(Some(&r.at), c.now)),
                reading_cell(k, &r.data)
            );
        }
        o.push_str("</tbody></table></div></div></section>");
    }
    if s.readings.is_empty() {
        let _ = write!(
            o,
            r#"<div class="list">{}</div>"#,
            empty("gauge", "No readings yet", "Nodes publish them as their plugins report.")
        );
    }
    o
}

fn reading_cell(kind: &str, data: &serde_json::Value) -> String {
    match kind {
        "node.status" => {
            let n = data["plugins"].as_array().map(Vec::len).unwrap_or(0);
            match data["problem"].as_str() {
                Some(p) => format!(r#"<span class="is-bad">{}</span>"#, esc(p)),
                None => esc(&format!("healthy, {}", plural(n, "plugin", "plugins"))),
            }
        }
        "plugin.status" => match data["problem"].as_str() {
            Some(p) => format!(r#"<span class="is-bad">{}</span>"#, esc(p)),
            None => "ok".into(),
        },
        _ => {
            // Percentages as the hub's meters; anything else as compact JSON.
            if let Some(obj) = data.as_object()
                && !obj.is_empty()
                && obj.values().all(|v| v.as_f64().is_some_and(|f| (0.0..=100.0).contains(&f)))
            {
                let mut o = String::from(r#"<div class="meters">"#);
                for (k, v) in obj {
                    let f = v.as_f64().unwrap_or(0.0);
                    let tone = if f >= 90.0 {
                        " is-bad"
                    } else if f >= 75.0 {
                        " is-warn"
                    } else {
                        ""
                    };
                    let _ = write!(
                        o,
                        r#"<div class="meter{tone}"><span>{}</span><span class="bar"><i style="--pct:{f:.0}%"></i></span><span class="pct">{f:.0}%</span></div>"#,
                        esc(k)
                    );
                }
                o.push_str("</div>");
                o
            } else {
                let t: String = data.to_string().chars().take(240).collect();
                format!(r#"<code class="json">{}</code>"#, esc(&t))
            }
        }
    }
}

// ------------------------------------------------------------------ graph

pub fn graph(s: &Snapshot, _c: &Ctx) -> String {
    let mut o = errors(s);
    o.push_str(&page_head(
        "Graph",
        "Every node linked to the control plane, the plugins each runs, and the tools they share with whom.",
    ));
    let _ = write!(o, r#"<div class="list graph-panel">{}</div>"#, crate::graph::svg(s));
    let _ = write!(
        o,
        r#"<p class="legend"><span>{}linked and healthy</span><span>{}needs a look</span><span>{}offline or failing</span><span><i class="legend-line"></i>shares a tool with</span></p>"#,
        dot("ok", "Healthy"),
        dot("warn", "Needs a look"),
        dot("bad", "Offline")
    );
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx<'static> {
        Ctx {
            can_act: true,
            csrf: "tok",
            now: now_ms(),
            stepup: &|id: &str| Some(format!("https://signin.test/stepup?approval={id}")),
            here: "/",
        }
    }

    #[test]
    fn every_page_renders_the_demo_and_escapes() {
        let s = crate::demo::Demo::new().snapshot();
        let c = ctx();
        let o = overview(&s, &c);
        assert!(o.contains("<h1>Fleet</h1>") && o.contains("Needs you") && o.contains("billing-bugfix@workers"), "{o}");
        assert!(o.contains(r#"action="/_/approvals/ap_7d21""#));
        assert!(o.contains("old-mini") && o.contains("is offline"));
        for page in [
            machines(&s, &c),
            sessions(&s, &c, None),
            sessions(&s, &c, Some("build")),
            events(&s, &c),
            access(&s, &c),
            readings(&s, &c),
            graph(&s, &c),
        ] {
            assert!(!page.is_empty());
            assert!(!page.contains("<script"));
            assert!(!page.contains('\u{2014}'), "no em dashes");
        }
        let mut evil = s.clone();
        evil.sessions[0].meta.tab = Some("<img src=x onerror=alert(1)>".into());
        assert!(!overview(&evil, &c).contains("<img src=x"));
        assert!(!sessions(&evil, &c, None).contains("<img src=x"));
    }

    #[test]
    fn without_admin_there_are_no_buttons() {
        let s = crate::demo::Demo::new().snapshot();
        let c = Ctx { can_act: false, csrf: "tok", now: now_ms(), stepup: &|_: &str| None, here: "/access" };
        assert!(!overview(&s, &c).contains("/_/approvals/"));
        assert!(!access(&s, &c).contains("Close now"));
    }

    #[test]
    fn a_window_request_is_not_approved_here() {
        let mut s = crate::demo::Demo::new().snapshot();
        s.approvals[0].verb = "activate".into();
        let o = access(&s, &ctx());
        assert!(
            o.contains(r#"href="https://signin.test/stepup?approval=ap_7d21""#),
            "approved through the passkey page: {o}"
        );
        assert!(o.contains(r#"value="deny""#) && !o.contains(r#"value="approve""#), "{o}");
        let none = Ctx { can_act: true, csrf: "tok", now: now_ms(), stepup: &|_: &str| None, here: "/access" };
        assert!(!access(&s, &none).contains("stepup"), "no passkey page, no approve button");
    }

    #[test]
    fn an_empty_fleet_says_what_to_do() {
        let s = Snapshot::default();
        let c = ctx();
        assert!(overview(&s, &c).contains("No machines yet"));
        assert!(sessions(&s, &c, None).contains("No sessions running"));
        assert!(events(&s, &c).contains("No events yet"));
    }

    #[test]
    fn times_read_naturally() {
        let now = 10 * 86_400_000;
        assert_eq!(ago(now - 10_000, now), "just now");
        assert_eq!(ago(now - 4 * 60_000, now), "4 min ago");
        assert_eq!(ago(now - 3 * 3_600_000, now), "3 h ago");
        assert_eq!(ago(now - 86_400_000, now), "1 day ago");
        assert_eq!(until(now + 38 * 60_000, now), "in 38 min");
    }
}
