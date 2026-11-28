//! What the dashboard shows, read from the control plane's API in one go (`Snapshot`), and typed loosely: a field a
//! newer or older control plane leaves out is a default, never an error.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::control::{Control, Failed};

/// Where the fleet comes from.
pub enum Source {
    Control(Control),
    /// A made-up fleet (`fleetlayer-dashboard demo`).
    Demo(crate::demo::Demo),
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Node {
    pub name: String,
    pub machine: Option<String>,
    pub user: Option<String>,
    pub version: Option<String>,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
    pub online: bool,
    #[serde(skip)]
    pub status: Option<NodeStatus>,
}

impl Node {
    pub fn machine(&self) -> &str {
        self.machine.as_deref().unwrap_or(&self.name)
    }

    /// Something to look at: offline, unhealthy, or a plugin with a problem.
    pub fn problem(&self) -> Option<String> {
        if !self.online {
            return Some("offline".into());
        }
        self.status.as_ref().and_then(|s| s.problem.clone())
    }
}

/// A node's own word on how it is (the `node.status` reading).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct NodeStatus {
    pub healthy: bool,
    pub problem: Option<String>,
    pub version: Option<String>,
    pub os: Option<String>,
    pub plugins: Vec<PluginState>,
    #[serde(skip)]
    pub at: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PluginState {
    pub name: String,
    pub enabled: bool,
    pub state: String,
    pub since: Option<String>,
    pub reason: Option<String>,
    pub version: Option<String>,
    pub roles: Vec<String>,
    pub restarts: u32,
    pub problem: Option<String>,
    pub services: Vec<String>,
}

impl PluginState {
    /// `ok`, `warn` or `bad`, for its dot.
    pub fn health(&self) -> &'static str {
        if !self.enabled {
            return "off";
        }
        match self.state.as_str() {
            "running" if self.problem.is_none() => "ok",
            "running" | "starting" | "restarting" => "warn",
            _ => "bad",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Session {
    pub session_id: String,
    pub manager: Option<String>,
    pub harness_plugin: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub updated_at: Option<String>,
    pub meta: SessionMeta,
    pub ended: Option<Value>,
    pub node: String,
    pub summary: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionMeta {
    pub harness: Option<String>,
    pub tab: Option<String>,
    pub status: Option<String>,
    pub status_changed_at: Option<String>,
    pub running: Option<bool>,
    pub human_attached: Option<bool>,
    pub prompt: Option<Value>,
}

impl Session {
    pub fn name(&self) -> String {
        self.meta.tab.clone().or_else(|| self.title.clone()).unwrap_or_else(|| short_id(&self.session_id))
    }

    pub fn address(&self) -> String {
        format!("{}@{}", self.name(), self.node)
    }

    pub fn is_live(&self) -> bool {
        self.ended.as_ref().is_none_or(Value::is_null) && self.meta.running != Some(false)
    }

    /// `blocked` (waiting on a person), `working`, `idle`, `done` or `unknown`.
    pub fn status(&self) -> &str {
        if self.meta.prompt.as_ref().is_some_and(|p| !p.is_null()) {
            return "blocked";
        }
        self.meta.status.as_deref().unwrap_or("unknown")
    }

    pub fn sentence(&self) -> Option<&str> {
        self.summary.as_ref()?.get("sentence")?.as_str()
    }

    pub fn harness(&self) -> &str {
        self.meta.harness.as_deref().or(self.harness_plugin.as_deref()).unwrap_or("agent")
    }

    /// What it's blocked on, in a few words.
    pub fn prompt_line(&self) -> Option<String> {
        let p = self.meta.prompt.as_ref().filter(|p| !p.is_null())?;
        Some(match p["kind"].as_str() {
            Some("permission") => format!("asks to run {}", p["tool"]["name"].as_str().unwrap_or("a tool")),
            Some("plan") => "has a plan to approve".into(),
            _ => p["questions"][0]["question"].as_str().map(str::to_string).unwrap_or_else(|| "has a question".into()),
        })
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Event {
    pub id: i64,
    pub channel: String,
    pub kind: String,
    pub subject: Option<String>,
    pub summary: String,
    pub source: String,
    pub at: i64,
    pub deliveries: Value,
}

impl Event {
    pub fn delivered_to(&self) -> Vec<String> {
        match &self.deliveries {
            Value::Array(a) => a
                .iter()
                .filter(|d| d["ok"].as_bool() != Some(false))
                .filter_map(|d| d["target"].as_str().map(str::to_string))
                .collect(),
            _ => vec![],
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Approval {
    pub id: String,
    pub from: String,
    pub verb: String,
    pub to: String,
    pub status: String,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub detail: Option<String>,
    pub once: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WindowRole {
    pub role: String,
    pub state: String,
    pub ends_at: Option<i64>,
    pub id: Option<String>,
    pub longest_ms: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WindowRecord {
    pub id: String,
    pub role: String,
    pub requested_by: String,
    pub opened_at: i64,
    pub ends_at: i64,
    pub closed_at: Option<i64>,
    pub closed_by: Option<String>,
    pub problem: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Reading {
    pub kind: String,
    pub key: String,
    pub node: String,
    pub at: String,
    pub data: Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Tool {
    pub name: String,
    pub node: String,
    pub online: bool,
    pub description: Option<String>,
    pub allow: Vec<String>,
    pub not_offered: Option<String>,
}

/// Everything the pages show, read at one moment.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub nodes: Vec<Node>,
    pub sessions: Vec<Session>,
    pub events: Vec<Event>,
    pub approvals: Vec<Approval>,
    pub windows: Vec<WindowRole>,
    pub window_history: Vec<WindowRecord>,
    pub readings: Vec<Reading>,
    pub tools: Vec<Tool>,
    /// What couldn't be read, per part (`fleet`, `events`...), in words.
    pub errors: BTreeMap<&'static str, String>,
}

impl Snapshot {
    /// Machines, each with its nodes (one per Unix user), in name order.
    pub fn machines(&self) -> Vec<(String, Vec<&Node>)> {
        let mut m: BTreeMap<String, Vec<&Node>> = BTreeMap::new();
        for n in &self.nodes {
            m.entry(n.machine().to_string()).or_default().push(n);
        }
        m.into_iter().collect()
    }

    pub fn live_sessions(&self) -> impl Iterator<Item = &Session> {
        self.sessions.iter().filter(|s| s.is_live())
    }

    pub fn sessions_on(&self, node: &str) -> Vec<&Session> {
        self.live_sessions().filter(|s| s.node == node).collect()
    }

    pub fn tools_on(&self, node: &str) -> Vec<&Tool> {
        self.tools.iter().filter(|t| t.node == node).collect()
    }

    pub fn pending_approvals(&self) -> Vec<&Approval> {
        self.approvals.iter().filter(|a| a.status == "pending").collect()
    }

    pub fn open_windows(&self) -> Vec<&WindowRole> {
        self.windows.iter().filter(|w| w.state == "open").collect()
    }

    /// Plugins with a problem, as `(node, plugin, problem)`, from both `node.status` and `plugin.status`.
    pub fn plugin_problems(&self) -> Vec<(String, String, String)> {
        let mut out: BTreeMap<(String, String), String> = BTreeMap::new();
        for n in &self.nodes {
            if let Some(s) = &n.status {
                for p in &s.plugins {
                    if p.health() == "bad" || p.health() == "warn" {
                        let why = p.problem.clone().or_else(|| p.reason.clone()).unwrap_or_else(|| p.state.clone());
                        out.insert((n.name.clone(), p.name.clone()), why);
                    }
                }
            }
        }
        for r in self.readings.iter().filter(|r| r.kind == "plugin.status") {
            if let Some(p) = r.data["problem"].as_str() {
                out.entry((r.node.clone(), r.key.clone())).or_insert_with(|| p.to_string());
            }
        }
        out.into_iter().map(|((n, p), w)| (n, p, w)).collect()
    }

    fn attach_status(&mut self) {
        for r in self.readings.iter().filter(|r| r.kind == "node.status") {
            if let Some(n) = self.nodes.iter_mut().find(|n| n.name == r.key && n.name == r.node)
                && let Ok(mut s) = serde_json::from_value::<NodeStatus>(r.data.clone())
            {
                s.at = r.at.clone();
                n.status = Some(s);
            }
        }
    }

    /// Build from the API's answers (also what the demo hands over).
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        fleet: Result<Value, Failed>,
        events: Result<Value, Failed>,
        approvals: Result<Value, Failed>,
        windows: Result<Value, Failed>,
        readings: Result<Value, Failed>,
        tools: Result<Value, Failed>,
    ) -> Snapshot {
        let mut s = Snapshot::default();
        fn take<T: serde::de::DeserializeOwned>(
            errors: &mut BTreeMap<&'static str, String>,
            part: &'static str,
            r: &Result<Value, Failed>,
            key: &str,
        ) -> Vec<T> {
            match r {
                Ok(v) => serde_json::from_value(v[key].clone()).unwrap_or_else(|e| {
                    errors.insert(part, format!("{part}: {e}"));
                    vec![]
                }),
                // An older control plane without the endpoint: just nothing to show.
                Err(Failed::Missing(_)) if matches!(part, "readings" | "tools") => vec![],
                Err(e) => {
                    errors.insert(part, e.to_string());
                    vec![]
                }
            }
        }
        let e = &mut s.errors;
        s.nodes = take(e, "fleet", &fleet, "nodes");
        s.sessions = take(e, "fleet", &fleet, "sessions");
        s.events = take(e, "events", &events, "events");
        s.approvals = take(e, "approvals", &approvals, "approvals");
        s.windows = take(e, "windows", &windows, "roles");
        s.window_history = take(e, "windows", &windows, "windows");
        s.readings = take(e, "readings", &readings, "readings");
        s.tools = take(e, "tools", &tools, "tools");
        s.attach_status();
        s
    }
}

/// The newest snapshot, read again when it's older than a few seconds, so several people (and the live refresh) don't
/// each cost the control plane six calls.
#[derive(Default)]
pub struct Cache {
    last: Mutex<Option<(Instant, std::sync::Arc<Snapshot>)>>,
}

const FRESH: Duration = Duration::from_secs(3);

impl Cache {
    pub async fn get(&self, source: &Source) -> std::sync::Arc<Snapshot> {
        let mut last = self.last.lock().await;
        if let Some((at, s)) = last.as_ref()
            && at.elapsed() < FRESH
        {
            return s.clone();
        }
        let s = std::sync::Arc::new(read(source).await);
        *last = Some((Instant::now(), s.clone()));
        s
    }

    pub async fn forget(&self) {
        *self.last.lock().await = None;
    }
}

async fn read(source: &Source) -> Snapshot {
    match source {
        Source::Control(c) => {
            let (fleet, events, approvals, windows, readings, tools) = tokio::join!(
                c.get("/v1/fleet"),
                c.get("/v1/events"),
                c.get("/v1/approvals"),
                c.get("/v1/windows"),
                c.get("/v1/readings"),
                c.get("/v1/tools"),
            );
            Snapshot::from_parts(fleet, events, approvals, windows, readings, tools)
        }
        Source::Demo(d) => d.snapshot(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_node_gets_its_status_and_problems_come_from_both_readings() {
        let s = Snapshot::from_parts(
            Ok(json!({"nodes": [{"name": "laptop", "machine": "laptop", "online": true}], "sessions": [
                {"sessionId": "s1", "node": "laptop", "meta": {"tab": "api", "status": "working"}},
                {"sessionId": "s2", "node": "laptop", "meta": {"tab": "old"}, "ended": {"at": "x"}},
                {"sessionId": "s3", "node": "laptop", "meta": {"tab": "q", "status": "idle",
                    "prompt": {"kind": "permission", "tool": {"name": "Bash"}}}}]})),
            Ok(json!({"events": []})),
            Ok(
                json!({"approvals": [{"id": "a1", "status": "pending", "from": "x@laptop", "verb": "use_tool", "to": "browser@laptop"}]}),
            ),
            Ok(json!({"roles": [{"role": "ops", "state": "open", "longestMs": 1}], "windows": []})),
            Ok(json!({"readings": [
                {"kind": "node.status", "key": "laptop", "node": "laptop", "at": "t",
                 "data": {"healthy": false, "problem": "plugin herdr is restarting", "plugins": [
                    {"name": "herdr", "enabled": true, "state": "restarting", "reason": "exited"}]}},
                {"kind": "plugin.status", "key": "cookies", "node": "laptop", "at": "t", "data": {"problem": "no access"}}]})),
            Err(Failed::Missing("old".into())),
        );
        assert!(s.errors.is_empty(), "{:?}", s.errors);
        assert_eq!(s.nodes[0].status.as_ref().unwrap().plugins[0].health(), "warn");
        assert_eq!(s.plugin_problems().len(), 2);
        assert_eq!(s.live_sessions().count(), 2);
        assert_eq!(s.sessions[2].status(), "blocked");
        assert_eq!(s.sessions[2].prompt_line().as_deref(), Some("asks to run Bash"));
        assert_eq!(s.pending_approvals().len(), 1);
        assert_eq!(s.open_windows().len(), 1);
    }

    #[test]
    fn a_control_plane_that_refuses_says_so_per_part() {
        let s = Snapshot::from_parts(
            Err(Failed::Refused("a read or admin token is needed".into())),
            Ok(json!({"events": []})),
            Ok(json!({"approvals": []})),
            Ok(json!({"roles": [], "windows": []})),
            Ok(json!({"readings": []})),
            Ok(json!({"tools": []})),
        );
        assert_eq!(s.errors.get("fleet").map(String::as_str), Some("a read or admin token is needed"));
    }
}
