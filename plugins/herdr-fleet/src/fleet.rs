//! The other machines' sessions, as herdr would show them: each node's sessions become workspaces of their own, with
//! IDs in herdr's form (`w1000:p3`) that stay the same across restarts (`ids.json`), and records and events shaped like
//! herdr's (`herdr api schema --json`), so a herdr client reads them as it reads its own.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The first workspace number given to another machine's sessions. This machine's own herdr numbers its workspaces from
/// 1, so the two never meet in practice; one that does is skipped (`Ids::workspace`).
pub const FIRST_WORKSPACE: u32 = 1000;

/// One running session on another node, from the control plane's fleet listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub node: String,
    pub session_id: String,
    pub tab: Option<String>,
    pub harness: String,
    pub cwd: Option<String>,
    /// herdr's `agent_status`: idle, done, working, blocked or unknown.
    pub status: String,
    pub changed_ms: i64,
    /// Which merged workspace it belongs to: its node and the workspace (or manager) it runs in there.
    pub workspace_key: String,
    /// Which merged pane it is: stable while the session runs in the same pane, across restarts of the harness.
    pub pane_key: String,
}

impl Remote {
    /// Its address in the fleet, as access control names it: `<tab>@<node>`, or the session ID for one with no tab.
    pub fn address(&self) -> String {
        format!("{}@{}", self.tab.as_deref().unwrap_or(&self.session_id), self.node)
    }
}

fn status_of(meta: &Value) -> String {
    match meta["status"].as_str() {
        Some(s @ ("idle" | "done" | "working" | "blocked")) => s.to_string(),
        _ => "unknown".into(),
    }
}

/// The running sessions of every other online node in a `{nodes, sessions}` fleet listing. This node's own are left out:
/// its herdr shows them itself, with everything herdr can do.
pub fn parse_fleet(fleet: &Value, me: &str) -> Vec<Remote> {
    let online: BTreeSet<&str> = fleet["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|n| n["online"].as_bool().unwrap_or(false))
        .filter_map(|n| n["name"].as_str())
        .collect();
    let mut out = vec![];
    for s in fleet["sessions"].as_array().into_iter().flatten() {
        let (Some(node), Some(id)) = (s["node"].as_str(), s["sessionId"].as_str()) else { continue };
        let meta = &s["meta"];
        if node == me || !online.contains(node) || !s["ended"].is_null() || meta["running"].as_bool() == Some(false) {
            continue;
        }
        let place = meta["workspace"].as_str().or(s["manager"].as_str()).unwrap_or("sessions");
        let workspace_key = format!("{node}/{place}");
        let pane_key = format!("{workspace_key}/{}", meta["address"].as_str().unwrap_or(id));
        let changed_ms = meta["statusChangedAt"]
            .as_str()
            .and_then(|t| t.parse::<Timestamp>().ok())
            .map(|t| t.as_millisecond())
            .unwrap_or(0);
        out.push(Remote {
            node: node.to_string(),
            session_id: id.to_string(),
            tab: meta["tab"].as_str().filter(|t| !t.is_empty()).map(str::to_string),
            harness: meta["harness"].as_str().unwrap_or("agent").to_string(),
            cwd: s["cwd"].as_str().map(str::to_string),
            status: status_of(meta),
            changed_ms,
            workspace_key,
            pane_key,
        });
    }
    out
}

/// The merged numbers given so far, kept in a file so a client's saved order and open views survive restarts.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Ids {
    #[serde(default)]
    workspaces: BTreeMap<String, u32>,
    #[serde(default)]
    panes: BTreeMap<String, u32>,
}

impl Ids {
    pub fn load(path: &Path) -> Ids {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, path)
    }

    /// The workspace number for a key, given once and kept.
    pub fn workspace(&mut self, key: &str) -> u32 {
        if let Some(n) = self.workspaces.get(key) {
            return *n;
        }
        let n = self.workspaces.values().max().map_or(FIRST_WORKSPACE, |m| m + 1).max(FIRST_WORKSPACE);
        self.workspaces.insert(key.to_string(), n);
        n
    }

    /// The pane number within its workspace, given once and kept: the tab number is the same.
    pub fn pane(&mut self, workspace_key: &str, pane_key: &str) -> u32 {
        if let Some(n) = self.panes.get(pane_key) {
            return *n;
        }
        let prefix = format!("{workspace_key}/");
        let n = self.panes.iter().filter(|(k, _)| k.starts_with(&prefix)).map(|(_, n)| *n).max().unwrap_or(0) + 1;
        self.panes.insert(pane_key.to_string(), n);
        n
    }
}

/// Counters herdr keeps per pane, which clients compare to see that something changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Seqs {
    pub state_change: u64,
    pub completion: u64,
    pub revision: u64,
}

/// One merged pane: a remote session with its merged numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub remote: Remote,
    pub workspace: u32,
    pub number: u32,
    pub seqs: Seqs,
}

impl Pane {
    pub fn workspace_id(&self) -> String {
        format!("w{}", self.workspace)
    }
    pub fn pane_id(&self) -> String {
        format!("w{}:p{}", self.workspace, self.number)
    }
    pub fn tab_id(&self) -> String {
        format!("w{}:t{}", self.workspace, self.number)
    }
    pub fn terminal_id(&self) -> String {
        format!("fleetlayer-{}-{}", self.workspace, self.number)
    }
    pub fn label(&self) -> String {
        self.remote.tab.clone().unwrap_or_else(|| self.remote.session_id.chars().take(8).collect())
    }

    pub fn agent_json(&self) -> Value {
        let r = &self.remote;
        json!({
            "terminal_id": self.terminal_id(),
            "agent": r.harness,
            "terminal_title": self.label(),
            "terminal_title_stripped": self.label(),
            "agent_status": r.status,
            "agent_session": {"source": "fleetlayer", "agent": r.harness, "kind": "id", "value": r.session_id},
            "workspace_id": self.workspace_id(),
            "tab_id": self.tab_id(),
            "tab_label": self.label(),
            "pane_id": self.pane_id(),
            "focused": false,
            "state_change_seq": self.seqs.state_change,
            "state_changed_at_unix_ms": r.changed_ms,
            "completion_seq": self.seqs.completion,
            "cwd": r.cwd,
            "foreground_cwd": r.cwd,
            "revision": self.seqs.revision,
            "_meta": {"fleetlayer.dev/session": {"node": r.node, "address": r.address()}},
        })
    }

    pub fn pane_json(&self) -> Value {
        json!({
            "pane_id": self.pane_id(),
            "terminal_id": self.terminal_id(),
            "workspace_id": self.workspace_id(),
            "tab_id": self.tab_id(),
            "tab_label": self.label(),
            "focused": false,
            "cwd": self.remote.cwd,
            "foreground_cwd": self.remote.cwd,
            "terminal_title": self.label(),
            "terminal_title_stripped": self.label(),
            "agent_status": self.remote.status,
            "scroll": {"offset_from_bottom": 0, "max_offset_from_bottom": 0, "viewport_rows": 40},
            "revision": self.seqs.revision,
        })
    }

    pub fn tab_json(&self, number: usize) -> Value {
        json!({
            "tab_id": self.tab_id(),
            "workspace_id": self.workspace_id(),
            "number": number,
            "label": self.label(),
            "focused": false,
            "pane_count": 1,
            "agent_status": self.remote.status,
        })
    }

    /// The tab's layout: one pane filling it.
    pub fn layout_json(&self) -> Value {
        let area = json!({"x": 0, "y": 0, "width": 120, "height": 40});
        json!({
            "workspace_id": self.workspace_id(),
            "tab_id": self.tab_id(),
            "zoomed": false,
            "area": area,
            "focused_pane_id": self.pane_id(),
            "panes": [{"pane_id": self.pane_id(), "focused": true, "rect": area}],
            "splits": [],
        })
    }
}

/// herdr's roll-up of several statuses: the one that most needs a person.
fn rollup<'a>(statuses: impl Iterator<Item = &'a str>) -> &'static str {
    let mut best = "unknown";
    let rank = |s: &str| match s {
        "blocked" => 4,
        "done" => 3,
        "working" => 2,
        "idle" => 1,
        _ => 0,
    };
    for s in statuses {
        if rank(s) > rank(best) {
            best = match s {
                "blocked" => "blocked",
                "done" => "done",
                "working" => "working",
                "idle" => "idle",
                _ => "unknown",
            };
        }
    }
    best
}

/// Something a target names: a merged pane (or its tab or terminal), or a whole merged workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    Pane(Box<Pane>),
    Workspace(u32),
}

/// Every remote session as merged panes, and the changes since the last listing as herdr events.
#[derive(Debug, Default)]
pub struct Model {
    ids: Ids,
    ids_path: Option<PathBuf>,
    /// By pane key.
    panes: BTreeMap<String, Pane>,
    /// Merged workspace number → (key, label).
    workspaces: BTreeMap<u32, (String, String)>,
}

impl Model {
    pub fn open(ids_path: PathBuf) -> Model {
        Model { ids: Ids::load(&ids_path), ids_path: Some(ids_path), ..Default::default() }
    }

    pub fn panes(&self) -> impl Iterator<Item = &Pane> {
        let mut v: Vec<&Pane> = self.panes.values().collect();
        v.sort_by_key(|p| (p.workspace, p.number));
        v.into_iter()
    }

    pub fn len(&self) -> usize {
        self.panes.len()
    }

    /// The merged pane, tab, terminal or workspace an ID names, if it's another machine's.
    pub fn find(&self, id: &str) -> Option<Found> {
        if let Some(rest) = id.strip_prefix("fleetlayer-") {
            let (w, n) = rest.split_once('-')?;
            let (w, n) = (w.parse::<u32>().ok()?, n.parse::<u32>().ok()?);
            return self
                .panes
                .values()
                .find(|p| p.workspace == w && p.number == n)
                .cloned()
                .map(|p| Found::Pane(Box::new(p)));
        }
        let rest = id.strip_prefix('w')?;
        let (w, inner) = match rest.split_once(':') {
            Some((w, inner)) => (w, Some(inner)),
            None => (rest, None),
        };
        let w: u32 = w.parse().ok()?;
        if !self.workspaces.contains_key(&w) {
            return None;
        }
        let Some(inner) = inner else { return Some(Found::Workspace(w)) };
        let n: u32 = inner.strip_prefix('p').or(inner.strip_prefix('t'))?.parse().ok()?;
        self.panes.values().find(|p| p.workspace == w && p.number == n).cloned().map(|p| Found::Pane(Box::new(p)))
    }

    /// The one merged pane whose tab is called `label`, for agent targets given by name.
    pub fn by_label(&self, label: &str) -> Option<Pane> {
        let mut hits = self.panes.values().filter(|p| p.remote.tab.as_deref() == Some(label));
        let first = hits.next()?;
        if hits.next().is_some() { None } else { Some(first.clone()) }
    }

    pub fn agents(&self) -> Vec<Value> {
        self.panes().map(Pane::agent_json).collect()
    }

    pub fn panes_json(&self, workspace: Option<u32>) -> Vec<Value> {
        self.panes().filter(|p| workspace.is_none_or(|w| p.workspace == w)).map(Pane::pane_json).collect()
    }

    pub fn tabs_json(&self, workspace: Option<u32>) -> Vec<Value> {
        let mut out = vec![];
        for w in self.workspaces.keys() {
            if workspace.is_some_and(|x| x != *w) {
                continue;
            }
            for (i, p) in self.panes().filter(|p| p.workspace == *w).enumerate() {
                out.push(p.tab_json(i + 1));
            }
        }
        out
    }

    pub fn layouts_json(&self) -> Vec<Value> {
        self.panes().map(Pane::layout_json).collect()
    }

    /// `local_count` is how many workspaces this machine's herdr has, so the numbers carry on after them.
    pub fn workspaces_json(&self, local_count: usize) -> Vec<Value> {
        self.workspaces
            .iter()
            .enumerate()
            .map(|(i, (w, (_, label)))| self.workspace_json(*w, label, local_count + i + 1))
            .collect()
    }

    pub fn workspace_info(&self, w: u32) -> Option<Value> {
        let (i, (_, (_, label))) = self.workspaces.iter().enumerate().find(|(_, (n, _))| **n == w)?;
        Some(self.workspace_json(w, label, i + 1))
    }

    fn workspace_json(&self, w: u32, label: &str, number: usize) -> Value {
        let panes: Vec<&Pane> = self.panes().filter(|p| p.workspace == w).collect();
        json!({
            "workspace_id": format!("w{w}"),
            "number": number,
            "label": label,
            "focused": false,
            "pane_count": panes.len(),
            "tab_count": panes.len(),
            "active_tab_id": panes.first().map(|p| p.tab_id()),
            "agent_status": rollup(panes.iter().map(|p| p.remote.status.as_str())),
        })
    }

    /// Take a new listing; answer the herdr events that turn the old view into the new one, in herdr's order.
    pub fn apply(&mut self, remotes: Vec<Remote>) -> Vec<Value> {
        let mut events = vec![];
        let mut ids_changed = false;
        // Workspaces: labelled by node, with the original workspace added when a node has more than one.
        let mut per_node: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for r in &remotes {
            per_node.entry(&r.node).or_default().insert(&r.workspace_key);
        }
        let mut workspaces = BTreeMap::new();
        for (node, keys) in &per_node {
            for key in keys {
                let before = self.ids.workspaces.len();
                let n = self.ids.workspace(key);
                ids_changed |= self.ids.workspaces.len() != before;
                let label = if keys.len() > 1 {
                    format!("{node} {}", key.rsplit('/').next().unwrap_or(key))
                } else {
                    node.to_string()
                };
                workspaces.insert(n, (key.to_string(), label));
            }
        }
        let mut panes = BTreeMap::new();
        for r in remotes {
            let workspace = self.ids.workspace(&r.workspace_key);
            let before = self.ids.panes.len();
            let number = self.ids.pane(&r.workspace_key, &r.pane_key);
            ids_changed |= self.ids.panes.len() != before;
            let old = self.panes.get(&r.pane_key);
            let mut seqs = old.map(|p| p.seqs).unwrap_or_default();
            if let Some(old) = old {
                if old.remote.status != r.status {
                    seqs.state_change += 1;
                    if old.remote.status == "working" && matches!(r.status.as_str(), "idle" | "done") {
                        seqs.completion += 1;
                    }
                }
                if old.remote != r {
                    seqs.revision += 1;
                }
            }
            panes.insert(r.pane_key.clone(), Pane { remote: r, workspace, number, seqs });
        }
        for (n, (_, label)) in &workspaces {
            if !self.workspaces.contains_key(n) {
                let ws = self.workspace_json_from(*n, label, &panes);
                events.push(lifecycle("workspace_created", json!({"workspace": ws})));
            }
        }
        let mut order: Vec<&Pane> = panes.values().collect();
        order.sort_by_key(|p| (p.workspace, p.number));
        for p in order {
            match self.panes.get(&p.remote.pane_key) {
                None => {
                    events.push(lifecycle("tab_created", json!({"tab": p.tab_json(p.number as usize)})));
                    events.push(lifecycle("pane_created", json!({"pane": p.pane_json()})));
                    events.push(lifecycle(
                        "pane_agent_detected",
                        json!({"pane_id": p.pane_id(), "workspace_id": p.workspace_id(), "tab_id": p.tab_id(),
                            "tab_label": p.label(), "agent": p.remote.harness}),
                    ));
                    events.push(status_event(p));
                }
                Some(old) => {
                    if old.remote.tab != p.remote.tab {
                        events.push(lifecycle(
                            "tab_renamed",
                            json!({"tab_id": p.tab_id(), "workspace_id": p.workspace_id(), "label": p.label()}),
                        ));
                    }
                    if old.remote.cwd != p.remote.cwd || old.remote.session_id != p.remote.session_id {
                        events.push(lifecycle("pane_updated", json!({"pane": p.pane_json()})));
                    }
                    if old.remote.status != p.remote.status {
                        events.push(status_event(p));
                    }
                }
            }
        }
        for (key, old) in &self.panes {
            if !panes.contains_key(key) {
                events.push(lifecycle(
                    "pane_agent_exited",
                    json!({"pane_id": old.pane_id(), "workspace_id": old.workspace_id(), "tab_id": old.tab_id(),
                        "tab_label": old.label(), "agent": old.remote.harness, "pane_exited": true,
                        "agent_session": {"source": "fleetlayer", "agent": old.remote.harness, "kind": "id",
                            "value": old.remote.session_id}}),
                ));
                events.push(lifecycle(
                    "pane_closed",
                    json!({"pane_id": old.pane_id(), "workspace_id": old.workspace_id()}),
                ));
                events.push(lifecycle(
                    "tab_closed",
                    json!({"tab_id": old.tab_id(), "workspace_id": old.workspace_id(), "label": old.label(),
                        "agents": [{"agent": old.remote.harness, "pane_id": old.pane_id(),
                            "agent_session": {"source": "fleetlayer", "agent": old.remote.harness, "kind": "id",
                                "value": old.remote.session_id}}]}),
                ));
            }
        }
        for n in self.workspaces.keys() {
            if !workspaces.contains_key(n) {
                events.push(lifecycle("workspace_closed", json!({"workspace_id": format!("w{n}")})));
            }
        }
        self.panes = panes;
        self.workspaces = workspaces;
        if ids_changed
            && let Some(path) = &self.ids_path
            && let Err(e) = self.ids.save(path)
        {
            tracing::warn!(path = %path.display(), "can't save the merged IDs: {e}");
        }
        events
    }

    fn workspace_json_from(&self, w: u32, label: &str, panes: &BTreeMap<String, Pane>) -> Value {
        let mine: Vec<&Pane> = panes.values().filter(|p| p.workspace == w).collect();
        json!({
            "workspace_id": format!("w{w}"),
            "number": w,
            "label": label,
            "focused": false,
            "pane_count": mine.len(),
            "tab_count": mine.len(),
            "active_tab_id": mine.first().map(|p| p.tab_id()),
            "agent_status": rollup(mine.iter().map(|p| p.remote.status.as_str())),
        })
    }
}

/// A lifecycle event in herdr's envelope: the snake-case name in both `event` and `data.type`.
pub fn lifecycle(name: &str, mut data: Value) -> Value {
    data["type"] = json!(name);
    json!({"event": name, "data": data})
}

/// A status change, in the envelope of herdr's `pane.agent_status_changed` subscription (dotted name, no `data.type`).
pub fn status_event(p: &Pane) -> Value {
    json!({"event": "pane.agent_status_changed", "data": {
        "pane_id": p.pane_id(), "workspace_id": p.workspace_id(), "tab_id": p.tab_id(), "tab_label": p.label(),
        "agent_status": p.remote.status, "agent": p.remote.harness}})
}

/// The subscription type an event answers to: `tab_closed` → `tab.closed`.
pub fn subscription_type(event: &Value) -> String {
    let name = event["event"].as_str().unwrap_or_default();
    if name.contains('.') {
        return name.to_string();
    }
    match name.split_once('_') {
        Some((kind, rest)) => format!("{kind}.{rest}"),
        None => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fleet() -> Value {
        json!({
            "nodes": [
                {"name": "here", "online": true}, {"name": "laptop", "online": true},
                {"name": "server", "online": true}, {"name": "asleep", "online": false}
            ],
            "sessions": [
                {"node": "here", "sessionId": "h1", "manager": "herdr", "cwd": "/h",
                 "meta": {"harness": "claude", "address": "w1:p1", "workspace": "w1", "tab": "mine", "status": "idle", "running": true}, "ended": null},
                {"node": "laptop", "sessionId": "l1", "manager": "herdr", "cwd": "/l",
                 "meta": {"harness": "claude", "address": "w1:p6", "workspace": "w1", "tab": "billing", "status": "blocked",
                          "statusChangedAt": "2026-11-27T04:31:46.673Z", "running": true}, "ended": null},
                {"node": "laptop", "sessionId": "l2", "manager": "herdr", "cwd": "/l2",
                 "meta": {"harness": "codex", "address": "w1:p7", "workspace": "w1", "tab": "docs", "status": "working", "running": true}, "ended": null},
                {"node": "laptop", "sessionId": "gone", "manager": "herdr",
                 "meta": {"harness": "claude", "address": "w1:p2", "workspace": "w1", "tab": "old", "running": false}, "ended": null},
                {"node": "laptop", "sessionId": "ended", "manager": "herdr",
                 "meta": {"harness": "claude", "address": "w1:p3", "workspace": "w1", "tab": "x"}, "ended": "2026-11-26T00:00:00Z"},
                {"node": "server", "sessionId": "s1", "manager": "rex", "cwd": "/srv",
                 "meta": {"harness": "claude", "tab": "deploy", "status": "strange"}, "ended": null},
                {"node": "asleep", "sessionId": "a1", "manager": "herdr", "meta": {"tab": "zz", "running": true}, "ended": null}
            ]
        })
    }

    #[test]
    fn only_other_online_nodes_running_sessions_are_merged() {
        let r = parse_fleet(&fleet(), "here");
        let ids: Vec<&str> = r.iter().map(|r| r.session_id.as_str()).collect();
        assert_eq!(ids, ["l1", "l2", "s1"], "this node's own, stopped, ended and offline nodes' sessions are left out");
        assert_eq!(r[0].changed_ms, 1795753906673);
        assert_eq!(r[2].status, "unknown", "a status herdr doesn't have reads as unknown");
        assert_eq!(r[2].workspace_key, "server/rex", "a session with no workspace goes in its manager's");
        assert_eq!(r[0].address(), "billing@laptop");
    }

    #[test]
    fn ids_stay_the_same_across_restarts_and_never_meet_the_local_herdrs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ids.json");
        let mut m = Model::open(path.clone());
        m.apply(parse_fleet(&fleet(), "here"));
        let ids: Vec<String> = m.panes().map(Pane::pane_id).collect();
        assert_eq!(ids, ["w1000:p1", "w1000:p2", "w1001:p1"]);

        // A restart, with the laptop's first session gone: the others keep their numbers.
        let mut m = Model::open(path);
        let mut f = fleet();
        f["sessions"][1]["meta"]["running"] = json!(false);
        m.apply(parse_fleet(&f, "here"));
        let ids: Vec<String> = m.panes().map(Pane::pane_id).collect();
        assert_eq!(ids, ["w1000:p2", "w1001:p1"]);
    }

    #[test]
    fn targets_find_merged_panes_tabs_terminals_and_workspaces_but_never_local_ones() {
        let mut m = Model::default();
        m.apply(parse_fleet(&fleet(), "here"));
        let billing = |f: Option<Found>| match f {
            Some(Found::Pane(p)) => p.remote.session_id,
            other => panic!("{other:?}"),
        };
        assert_eq!(billing(m.find("w1000:p1")), "l1");
        assert_eq!(billing(m.find("w1000:t1")), "l1");
        assert_eq!(billing(m.find("fleetlayer-1000-1")), "l1");
        assert_eq!(m.find("w1001"), Some(Found::Workspace(1001)));
        assert_eq!(m.find("w1:p1"), None, "this machine's herdr IDs stay its own");
        assert_eq!(m.find("w1000:p9"), None);
        assert_eq!(m.find("billing"), None);
        assert_eq!(m.by_label("docs").map(|p| p.pane_id()), Some("w1000:p2".into()));
    }

    #[test]
    fn records_look_like_herdrs() {
        let mut m = Model::default();
        m.apply(parse_fleet(&fleet(), "here"));
        let a = &m.agents()[0];
        for key in [
            "terminal_id",
            "agent",
            "agent_status",
            "agent_session",
            "workspace_id",
            "tab_id",
            "tab_label",
            "pane_id",
            "state_change_seq",
            "state_changed_at_unix_ms",
            "completion_seq",
            "cwd",
            "revision",
        ] {
            assert!(a.get(key).is_some(), "agent record lacks {key}");
        }
        assert_eq!(a["agent_session"]["value"], "l1");
        let ws = m.workspaces_json(2);
        assert_eq!(ws[0]["label"], "laptop");
        assert_eq!(ws[0]["number"], 3, "numbered after this machine's two workspaces");
        assert_eq!(ws[0]["agent_status"], "blocked", "the status most in need of a person");
        assert_eq!(m.tabs_json(Some(1000)).len(), 2);
        assert_eq!(m.panes_json(Some(1001)).len(), 1);
    }

    #[test]
    fn changes_become_herdr_events() {
        let mut m = Model::default();
        let first = m.apply(parse_fleet(&fleet(), "here"));
        let names: Vec<String> = first.iter().map(subscription_type).collect();
        assert_eq!(names.iter().filter(|n| *n == "workspace.created").count(), 2);
        assert_eq!(names.iter().filter(|n| *n == "tab.created").count(), 3);
        assert!(names.contains(&"pane.agent_status_changed".to_string()));

        let mut f = fleet();
        f["sessions"][2]["meta"]["status"] = json!("idle"); // docs finished
        f["sessions"][1]["meta"]["tab"] = json!("billing v2");
        f["sessions"][5]["ended"] = json!("2026-11-28T00:00:00Z"); // the server's only session
        let ev = m.apply(parse_fleet(&f, "here"));
        let names: Vec<String> = ev.iter().map(subscription_type).collect();
        assert_eq!(
            names,
            [
                "tab.renamed",
                "pane.agent_status_changed",
                "pane.agent_exited",
                "pane.closed",
                "tab.closed",
                "workspace.closed"
            ]
        );
        assert_eq!(ev[1]["data"]["agent_status"], "idle");
        assert!(ev[1]["data"].get("type").is_none(), "status events carry no data.type, like herdr's");
        assert_eq!(ev[4]["data"]["type"], "tab_closed");
        let docs = m.panes().find(|p| p.remote.session_id == "l2").unwrap();
        assert_eq!(docs.seqs, Seqs { state_change: 1, completion: 1, revision: 1 });
        assert!(m.apply(parse_fleet(&f, "here")).is_empty(), "nothing changed, nothing to say");
    }
}
