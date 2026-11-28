//! A made-up fleet for `fleetlayer-dashboard demo`: four machines, a dozen sessions, events, a request waiting for
//! approval and an open admin window, all with generic names and times relative to now, in the shapes the control
//! plane's API answers. For trying the dashboard out and for the screenshots on fleetlayer.ai.

use serde_json::{Value, json};

use crate::model::Snapshot;

pub struct Demo {
    start: i64,
}

fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

fn iso(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms).map(|t| t.to_string()).unwrap_or_default()
}

impl Demo {
    pub fn new() -> Demo {
        Demo { start: now_ms() }
    }

    pub fn snapshot(&self) -> Snapshot {
        let now = now_ms();
        let min = 60_000;
        let hour = 60 * min;
        let day = 24 * hour;
        // A tick every 20 seconds, so the live view moves.
        let tick = (now - self.start) / 20_000;

        let node = |name: &str, machine: &str, user: &str, online: bool, seen: i64| {
            json!({"name": name, "machine": machine, "user": user, "version": "0.4.0", "createdAt": now - 40 * day,
                "lastSeenAt": now - seen, "online": online})
        };
        let nodes = json!([
            node("laptop", "laptop", "alex", true, 4_000),
            node("studio", "studio", "alex", true, 9_000),
            node("agents", "agents-vm", "agent", true, 2_000),
            node("agents-triage", "agents-vm", "triage", true, 6_000),
            node("build", "build-server", "ci", true, 3_000),
            node("edge", "vps", "fleet", true, 5_000),
            node("old-mini", "old-mini", "alex", false, 3 * day),
        ]);

        let session = |id: &str, node: &str, tab: &str, harness: &str, status: &str, ago: i64, sentence: &str| {
            json!({"sessionId": id, "manager": "herdr", "harnessPlugin": if harness == "codex" { "codex" } else { "claude-code" },
                "cwd": format!("/home/agent/code/{tab}"), "title": tab, "updatedAt": iso(now - ago),
                "meta": {"harness": harness, "tab": tab, "status": status, "statusChangedAt": iso(now - ago), "running": true},
                "ended": null, "node": node, "summary": {"sentence": sentence}})
        };
        let working_or = |i: i64, s: &'static str| if (tick + i) % 3 == 0 { s } else { "working" };
        let mut sessions = vec![
            session(
                "d1f0",
                "agents",
                "api-refactor",
                "claude",
                working_or(0, "idle"),
                3 * min,
                "Moving the billing API to the new client; 14 of 22 endpoints done, tests green so far.",
            ),
            session(
                "a7c2",
                "agents",
                "docs-site",
                "claude",
                "idle",
                25 * min,
                "Rewrote the quick-start guide and fixed the broken links the checker found; waiting for review.",
            ),
            session(
                "9be4",
                "agents",
                "release-notes",
                "claude",
                "done",
                2 * hour,
                "Drafted the 0.4 release notes from merged pull requests and opened them for review.",
            ),
            session(
                "44aa",
                "agents",
                "onboarding-flow",
                "claude",
                working_or(1, "idle"),
                6 * min,
                "Building the new sign-up flow: email step and team invite done, billing step next.",
            ),
            session(
                "c310",
                "agents-triage",
                "triage",
                "claude",
                "working",
                min,
                "Sorted 31 new support emails: 4 bugs filed, 2 replies drafted, the rest archived.",
            ),
            session(
                "5e61",
                "build",
                "flaky-tests",
                "codex",
                working_or(2, "idle"),
                9 * min,
                "Found the race in the upload tests (a shared temp folder) and is fixing it.",
            ),
            session(
                "71bd",
                "build",
                "perf-bench",
                "codex",
                "working",
                4 * min,
                "Benchmarking the PDF renderer after the cache change: 18% faster on the large set so far.",
            ),
            session(
                "b2d9",
                "laptop",
                "design-review",
                "claude",
                "idle",
                40 * min,
                "Compared three layouts for the settings page and wrote up the trade-offs.",
            ),
            session(
                "e8f1",
                "laptop",
                "inbox",
                "claude",
                "done",
                3 * hour,
                "Cleared the inbox and set up two reminders.",
            ),
            session(
                "0c5a",
                "studio",
                "video-captions",
                "claude",
                "working",
                2 * min,
                "Writing captions for the launch video; 6 of 9 clips done.",
            ),
            session(
                "f4e2",
                "edge",
                "deploy",
                "claude",
                "idle",
                50 * min,
                "Deployed the gateway update to the VPS and checked every service answers.",
            ),
        ];
        let mut blocked = session(
            "33c7",
            "agents",
            "billing-bugfix",
            "claude",
            "idle",
            7 * min,
            "Fixed the rounding bug in invoices; wants to run the migration on staging.",
        );
        blocked["meta"]["prompt"] = json!({"id": "p1", "kind": "permission", "at": iso(now - 7 * min), "source": "hook",
            "tool": {"name": "Bash", "input": {"command": "bin/migrate --env staging"}}});
        sessions.insert(2, blocked);
        let fleet = json!({"nodes": nodes, "sessions": sessions});

        let ev = |id: i64,
                  channel: &str,
                  kind: &str,
                  subject: &str,
                  summary: &str,
                  source: &str,
                  ago: i64,
                  to: &[&str]| {
            let d: Vec<Value> = to.iter().map(|t| json!({"target": t, "ok": true})).collect();
            json!({"id": id, "channel": channel, "kind": kind, "subject": subject, "summary": summary, "source": source,
                "at": now - ago, "deliveries": d})
        };
        let base = 4_200 + tick;
        let mut events = vec![
            ev(
                base,
                "ci",
                "workflow_run",
                "acme/web@3f9c2e1",
                "CI passed on main (3f9c2e1): 412 tests in 6m 12s",
                "github",
                40_000,
                &["api-refactor@agents"],
            ),
            ev(
                base - 1,
                "ci",
                "workflow_run",
                "acme/web@8a1d044",
                "CI failed on fix/rounding (8a1d044): 2 tests failed",
                "github",
                4 * min,
                &["billing-bugfix@agents"],
            ),
            ev(
                base - 2,
                "email",
                "message",
                "support",
                "New support email: \"Export button does nothing\"",
                "mail-bridge",
                9 * min,
                &["triage@agents-triage"],
            ),
            ev(
                base - 3,
                "schedule",
                "reminder",
                "nightly-check",
                "Every 3 hours: check the build queue",
                "scheduler",
                22 * min,
                &["perf-bench@build"],
            ),
            ev(
                base - 4,
                "deploy",
                "release",
                "web",
                "Deployed web 2026.11.28-2 to production",
                "deploy-bot",
                48 * min,
                &["deploy@edge"],
            ),
            ev(
                base - 5,
                "ci",
                "workflow_run",
                "acme/docs@c4410ab",
                "Docs build passed (c4410ab)",
                "github",
                70 * min,
                &["docs-site@agents"],
            ),
            ev(
                base - 6,
                "uptime",
                "check",
                "api",
                "api.example.com is back up after 2 minutes",
                "monitor",
                3 * hour,
                &["deploy@edge"],
            ),
        ];
        if tick % 2 == 1 {
            events.insert(
                0,
                ev(
                    base + 1,
                    "ci",
                    "workflow_run",
                    "acme/web@d02b7f3",
                    "CI running on main (d02b7f3)",
                    "github",
                    5_000,
                    &["onboarding-flow@agents"],
                ),
            );
        }

        let approvals = json!({"approvals": [
            {"id": "ap_7d21", "from": "billing-bugfix@agents", "verb": "use_tool", "to": "browser@laptop", "status": "pending",
             "createdAt": now - 5 * min, "expiresAt": now + 55 * min,
             "detail": "Open the staging admin page to check the fixed invoice totals", "once": false},
        ]});
        let windows = json!({"roles": [
            {"role": "ops-admin", "state": "open", "endsAt": now + 38 * min, "id": "w_19", "longestMs": 2 * hour},
            {"role": "db-admin", "state": "off", "longestMs": hour},
        ], "windows": [
            {"id": "w_19", "role": "ops-admin", "requestedBy": "deploy@edge", "openedAt": now - 22 * min, "endsAt": now + 38 * min},
            {"id": "w_18", "role": "db-admin", "requestedBy": "billing-bugfix@agents", "openedAt": now - day - 3 * hour,
             "endsAt": now - day - 2 * hour, "closedAt": now - day - 2 * hour - 10 * min, "closedBy": "person (dashboard)"},
        ]});

        let plugin = |name: &str, state: &str, problem: Option<&str>, services: &[&str], roles: &[&str]| {
            json!({"name": name, "enabled": true, "state": state, "since": iso(now - 3 * day), "version": "0.4.0",
                "roles": roles, "restarts": 0, "problem": problem, "services": services})
        };
        let status = |n: &str, os: &str, plugins: Vec<Value>| {
            let problem: Vec<String> = plugins
                .iter()
                .filter_map(|p| {
                    p["problem"]
                        .as_str()
                        .map(|w| format!("plugin {} is failing ({w})", p["name"].as_str().unwrap_or("")))
                })
                .collect();
            json!({"kind": "node.status", "key": n, "node": n, "at": iso(now - 30_000), "data": {
                "healthy": problem.is_empty(), "problem": if problem.is_empty() { Value::Null } else { json!(problem.join("; ")) },
                "version": "0.4.0", "os": os, "plugins": plugins}})
        };
        let sm = &["session_manager"][..];
        let h = &["harness"][..];
        let readings = json!({"readings": [
            status("laptop", "macos", vec![plugin("herdr", "running", None, &[], sm), plugin("claude-code", "running", None, &[], h),
                plugin("shared-folders", "running", None, &[], &[]),
                plugin("cookies", "running", Some("Chrome's cookies can't be read: give the terminal Full Disk Access"), &[], &[])]),
            status("studio", "macos", vec![plugin("herdr", "running", None, &[], sm), plugin("claude-code", "running", None, &[], h)]),
            status("agents", "linux", vec![plugin("herdr", "running", None, &[], sm), plugin("claude-code", "running", None, &[], h),
                plugin("summaries", "running", None, &[], &["observer"]), plugin("scheduler", "running", None, &[], &["event_source"]),
                plugin("pages", "running", None, &["pages"], &[]), plugin("1password", "running", None, &[], &["sync_target"])]),
            status("agents-triage", "linux", vec![plugin("herdr", "running", None, &[], sm), plugin("claude-code", "running", None, &[], h),
                plugin("egress", "running", None, &[], &[])]),
            status("build", "linux", vec![plugin("herdr", "running", None, &[], sm), plugin("codex", "running", None, &[], h),
                plugin("file-sync", "running", None, &[], &["sync_target"])]),
            status("edge", "linux", vec![plugin("signin", "running", None, &["signin"], &[]),
                plugin("claude-auth-manager", "running", None, &["hub"], &[]), plugin("dashboard", "running", None, &["dashboard"], &[])]),
            {"kind": "claude.usage", "key": "account-1", "node": "edge", "at": iso(now - 2 * min),
             "data": {"fiveHour": 34, "sevenDay": 58}},
            {"kind": "claude.usage", "key": "account-2", "node": "edge", "at": iso(now - 2 * min),
             "data": {"fiveHour": 12, "sevenDay": 21}},
        ]});
        let tools = json!({"tools": [
            {"name": "browser", "node": "laptop", "online": true, "description": "Alex's signed-in Chrome", "allow": ["*@agents"]},
            {"name": "folders", "node": "laptop", "online": true, "description": "Screenshots and Downloads", "allow": ["*@agents", "*@studio"]},
            {"name": "gpu", "node": "build", "online": true, "description": "The build server's GPU runner", "allow": ["*@agents"]},
        ]});
        Snapshot::from_parts(
            Ok(fleet),
            Ok(json!({"events": events})),
            Ok(approvals),
            Ok(windows),
            Ok(readings),
            Ok(tools),
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_demo_fleet_reads_cleanly() {
        let s = super::Demo::new().snapshot();
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert_eq!(s.machines().len(), 6);
        assert!(s.nodes.iter().filter(|n| n.online).all(|n| n.status.is_some()));
        assert_eq!(s.pending_approvals().len(), 1);
        assert!(s.live_sessions().any(|x| x.status() == "blocked"));
    }
}
