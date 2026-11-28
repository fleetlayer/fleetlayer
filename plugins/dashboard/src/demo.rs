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
            node("workers", "agent-box", "agent", true, 2_000),
            node("reviewer", "agent-box", "reviewer", true, 6_000),
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
                "workers",
                "api-refactor",
                "claude",
                working_or(0, "idle"),
                3 * min,
                "Moving the billing API to the new client; 14 of 22 endpoints done, tests green so far.",
            ),
            session(
                "a7c2",
                "workers",
                "docs-site",
                "claude",
                "idle",
                25 * min,
                "Rewrote the quick-start guide and fixed the broken links the checker found; waiting for review.",
            ),
            session(
                "9be4",
                "workers",
                "release-notes",
                "claude",
                "done",
                2 * hour,
                "Drafted the 0.4 release notes from merged pull requests and opened them for review.",
            ),
            session(
                "44aa",
                "workers",
                "onboarding-flow",
                "claude",
                working_or(1, "idle"),
                6 * min,
                "Building the new sign-up flow: account step and team invite done, billing step next.",
            ),
            session(
                "c310",
                "reviewer",
                "pr-review",
                "claude",
                "working",
                min,
                "Reviewing the open pull requests: 2 approved, 1 sent back with a failing edge case.",
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
                "notes",
                "claude",
                "done",
                3 * hour,
                "Turned this week's planning notes into five issues and a short decision log.",
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
                "Rolled the gateway update out to the VPS and checked every service answers.",
            ),
        ];
        let mut blocked = session(
            "33c7",
            "workers",
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
                &["api-refactor@workers"],
            ),
            ev(
                base - 1,
                "ci",
                "workflow_run",
                "acme/web@8a1d044",
                "CI failed on fix/rounding (8a1d044): 2 tests failed",
                "github",
                4 * min,
                &["billing-bugfix@workers"],
            ),
            ev(
                base - 2,
                "pr",
                "pull_request_review",
                "acme/web#218",
                "Pull request #218 approved: Fix invoice rounding",
                "github",
                11 * min,
                &["billing-bugfix@workers"],
            ),
            ev(
                base - 3,
                "fleet",
                "node.online",
                "studio",
                "studio is back online after 12 minutes",
                "control-plane",
                18 * min,
                &["deploy@edge"],
            ),
            ev(
                base - 4,
                "schedule",
                "reminder",
                "build-queue",
                "Every 3 hours: check the build queue",
                "scheduler",
                22 * min,
                &["perf-bench@build"],
            ),
            ev(
                base - 5,
                "access",
                "refused",
                "gpu@build",
                "Refused: notes@laptop asked to use gpu@build",
                "policy",
                54 * min,
                &["notes@laptop"],
            ),
            ev(
                base - 6,
                "fleet",
                "node.updated",
                "build",
                "build updated to Fleetlayer 0.4.0 and restarted its plugins",
                "control-plane",
                2 * hour,
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
                    &["onboarding-flow@workers"],
                ),
            );
        }

        let approvals = json!({"approvals": [
            {"id": "ap_7d21", "from": "billing-bugfix@workers", "verb": "use_tool", "to": "browser@laptop", "status": "pending",
             "createdAt": now - 5 * min, "expiresAt": now + 55 * min,
             "detail": "Open the staging admin page to check the fixed invoice totals", "once": false},
        ]});
        let windows = json!({"roles": [
            {"role": "ops-admin", "state": "open", "endsAt": now + 38 * min, "id": "w_19", "longestMs": 2 * hour},
            {"role": "db-admin", "state": "off", "longestMs": hour},
        ], "windows": [
            {"id": "w_19", "role": "ops-admin", "requestedBy": "deploy@edge", "openedAt": now - 22 * min, "endsAt": now + 38 * min},
            {"id": "w_18", "role": "db-admin", "requestedBy": "billing-bugfix@workers", "openedAt": now - day - 3 * hour,
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
                plugin("cookies", "running", Some("The x.com cookies have expired: sign in to x.com again in Chrome"), &[], &[])]),
            status("studio", "macos", vec![plugin("herdr", "running", None, &[], sm), plugin("claude-code", "running", None, &[], h)]),
            status("workers", "linux", vec![plugin("herdr", "running", None, &[], sm), plugin("claude-code", "running", None, &[], h),
                plugin("summaries", "running", None, &[], &["observer"]), plugin("scheduler", "running", None, &[], &["event_source"]),
                plugin("pages", "running", None, &["pages"], &[]), plugin("1password", "running", None, &[], &["sync_target"])]),
            status("reviewer", "linux", vec![plugin("herdr", "running", None, &[], sm), plugin("claude-code", "running", None, &[], h),
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
            {"name": "browser", "node": "laptop", "online": true, "description": "Alex's signed-in Chrome", "allow": ["*@workers"]},
            {"name": "folders", "node": "laptop", "online": true, "description": "Screenshots and Downloads", "allow": ["*@workers", "*@studio"]},
            {"name": "gpu", "node": "build", "online": true, "description": "The build server's GPU runner", "allow": ["*@workers"]},
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
