//! Fleetlayer's extensions to ACP: method names (all start `_fleetlayer.dev/`), `_meta` keys (all start
//! `fleetlayer.dev/`) and their payloads. `docs/protocol.md` is the specification.

use jiff::Timestamp;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::acp::Meta;

pub mod method {
    pub const PING: &str = "_fleetlayer.dev/ping";
    pub const LOG: &str = "_fleetlayer.dev/log";
    pub const SESSION_OPEN: &str = "_fleetlayer.dev/session/open";
    pub const SESSION_DELIVER: &str = "_fleetlayer.dev/session/deliver";
    pub const SESSION_ENDED: &str = "_fleetlayer.dev/session/ended";
    pub const SESSION_RENAMED: &str = "_fleetlayer.dev/session/renamed";
    pub const SESSIONS_CHANGED: &str = "_fleetlayer.dev/sessions/changed";
    pub const SESSIONS_QUERY: &str = "_fleetlayer.dev/sessions/query";
    /// Plugin → node: follow this node's sessions. Answers the current list; changes follow as
    /// `SESSIONS_UPDATE` notifications.
    pub const SESSIONS_SUBSCRIBE: &str = "_fleetlayer.dev/sessions/subscribe";
    pub const SESSIONS_UPDATE: &str = "_fleetlayer.dev/sessions/update";
    /// Plugin → node: a summary event (`crate::summary::SummaryEvent`), for the control plane.
    pub const SUMMARY_PUBLISH: &str = "_fleetlayer.dev/summary/publish";
    /// Node → summaries plugin: a session's catch-up. `{sessionId}` → `crate::summary::Catchup`.
    pub const SUMMARY_CATCHUP: &str = "_fleetlayer.dev/summary/catchup";
    /// Node → summaries plugin: import claude-summarizer's history. `{dir}` → `{records}`.
    pub const SUMMARY_IMPORT: &str = "_fleetlayer.dev/summary/import";
    pub const HARNESS_TRANSCRIPT: &str = "_fleetlayer.dev/harness/transcript";
    /// Node → session manager: the command that streams one of its sessions' terminals in the terminal stream format
    /// (`docs/protocol.md`, "Terminals"). `TerminalCommandRequest` → `TerminalCommand`.
    pub const TERMINAL_COMMAND: &str = "_fleetlayer.dev/terminal/command";
    pub const MESSAGE_SEND: &str = "_fleetlayer.dev/message/send";
    pub const EVENT_PUBLISH: &str = "_fleetlayer.dev/event/publish";
    pub const EVENT_HEARTBEAT: &str = "_fleetlayer.dev/event/heartbeat";
    pub const ACTION_RUN: &str = "_fleetlayer.dev/action/run";
    pub const ACTION_PROGRESS: &str = "_fleetlayer.dev/action/progress";
    pub const SYNC_PUBLISH: &str = "_fleetlayer.dev/sync/publish";
    pub const SYNC_APPLY: &str = "_fleetlayer.dev/sync/apply";
    pub const READING_PUBLISH: &str = "_fleetlayer.dev/reading/publish";
    pub const READING_SUBSCRIBE: &str = "_fleetlayer.dev/reading/subscribe";
    pub const READING_UPDATE: &str = "_fleetlayer.dev/reading/update";
    /// Plugin → node (notification): this plugin serves HTTP here (`ServiceListening`), sent after it has bound.
    /// Idempotent: resending replaces the entry of that name; the node drops a plugin's entries when it exits.
    pub const SERVICE_LISTENING: &str = "_fleetlayer.dev/service/listening";
}

pub mod meta_key {
    pub const NODE: &str = "fleetlayer.dev/node";
    pub const PLUGIN: &str = "fleetlayer.dev/plugin";
    pub const SESSION: &str = "fleetlayer.dev/session";
}

/// Read a typed value from a `_meta` object. `Ok(None)` when the key isn't there.
pub fn get_meta<T: DeserializeOwned>(meta: Option<&Meta>, key: &str) -> Result<Option<T>, serde_json::Error> {
    match meta.and_then(|m| m.get(key)) {
        Some(v) => serde_json::from_value(v.clone()).map(Some),
        None => Ok(None),
    }
}

/// A `_meta` object holding one Fleetlayer key.
pub fn meta_with<T: Serialize>(key: &str, value: &T) -> Meta {
    let mut m = Meta::new();
    m.insert(key.into(), serde_json::to_value(value).unwrap_or(Value::Null));
    m
}

// ------------------------------------------------------------ initialize

/// `clientCapabilities._meta["fleetlayer.dev/node"]`: the node introducing itself to a plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeMeta {
    pub protocol: u32,
    /// The name the control plane knows this node by.
    pub node: String,
    /// The host's name, the same for every node on it.
    pub machine: String,
    pub user: String,
    pub home: String,
    /// The plugin's own section of the node's config, untouched.
    #[serde(default)]
    pub config: Value,
}

/// `agentCapabilities._meta["fleetlayer.dev/plugin"]`: the roles a plugin takes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginMeta {
    pub protocol: u32,
    pub roles: Roles,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct Roles {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_manager: Option<SessionManagerRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_source: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<ActionRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_target: Option<SyncRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_source: Option<SyncRole>,
    /// It follows sessions and reports on them (summaries), with no other role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observer: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionManagerRole {
    #[serde(default)]
    pub open: bool,
    #[serde(default)]
    pub close: bool,
    #[serde(default)]
    pub interrupt: bool,
    #[serde(default)]
    pub deliver: bool,
    /// It streams changes; without this the node polls `session/list`.
    #[serde(default)]
    pub events: bool,
    /// Messages the manager types into sessions by itself (herdr's auto-continue), which summaries mustn't count as
    /// the person's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub own_messages: Vec<String>,
    /// It answers `TERMINAL_COMMAND`: its sessions' terminals can be watched and driven.
    #[serde(default)]
    pub terminal: bool,
}

/// Watching a terminal, or driving it too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TerminalMode {
    #[default]
    Observe,
    Control,
}

impl TerminalMode {
    /// The access policy's verb: `read` to watch, `attach` to type.
    pub fn verb(self) -> &'static str {
        match self {
            TerminalMode::Observe => "read",
            TerminalMode::Control => "attach",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalCommandRequest {
    pub session_id: String,
    pub mode: TerminalMode,
    pub cols: u16,
    pub rows: u16,
}

/// A command the node runs to stream a terminal: it writes the stream format on stdout and reads it on stdin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TerminalCommand {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    /// Variables to remove from the node's environment for it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_remove: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HarnessRole {
    /// The harnesses it speaks for (`claude`).
    pub harnesses: Vec<String>,
    #[serde(default)]
    pub deliver: bool,
    #[serde(default)]
    pub transcripts: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ActionRole {
    pub types: Vec<ActionType>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionType {
    #[serde(rename = "type")]
    pub kind: String,
    /// A JSON Schema for the action's input.
    pub input: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SyncRole {
    pub bundles: Vec<String>,
}

// ------------------------------------------------------------ sessions

/// An agent's status, as session managers report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Waiting for input, and the person has seen its last turn.
    Idle,
    /// Waiting for input, with a finished turn nobody has looked at.
    Done,
    Working,
    /// Waiting on a person: a permission prompt or a question.
    Blocked,
    #[default]
    Unknown,
}

impl Status {
    pub fn is_idle(self) -> bool {
        matches!(self, Status::Idle | Status::Done)
    }
}

/// `_meta["fleetlayer.dev/session"]` on a session's list entry or on a `session_info_update`. In an update, only the
/// fields that changed are set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// False when `sessionId` is `@<address>`: the harness's own ID isn't known yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub named: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_changed_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_attached: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opens_in: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_command: Option<String>,
    /// A harness plugin can deliver into it through the agent's own channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliverable: Option<bool>,
}

impl SessionMeta {
    /// Lay `update`'s set fields over these.
    pub fn merge(&mut self, update: &SessionMeta) {
        macro_rules! take {
            ($($f:ident),*) => { $( if update.$f.is_some() { self.$f = update.$f.clone(); } )* };
        }
        take!(
            harness,
            address,
            named,
            workspace,
            tab,
            status,
            status_changed_at,
            running,
            human_attached,
            open_url,
            opens_in,
            resume_command,
            deliverable
        );
    }
}

/// The `sessionId` of a session the harness hasn't named yet.
pub fn unnamed_session_id(address: &str) -> String {
    format!("@{address}")
}

/// `_fleetlayer.dev/session/ended`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEnded {
    pub session_id: String,
    pub at: Timestamp,
    pub how: How,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum How {
    /// Its tab closed: a person closed it, or the last process in it exited.
    TabClosed,
    /// The agent's process exited; `terminal_closed` is false when it went back to a shell and the tab stayed.
    #[serde(rename_all = "camelCase")]
    AgentExited { final_status: Option<Status>, terminal_closed: bool },
}

/// `_fleetlayer.dev/service/listening`: a plugin's HTTP service, for the gateway (`<name>.<domain>`) and for config
/// emitted for a server's proxy and monitor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceListening {
    /// One DNS label: lower-case letters, digits and hyphens (`pages`).
    pub name: String,
    /// Where it really listens, after binding: `http://127.0.0.1:43117`.
    pub url: String,
    /// A path that answers 2xx while the service does its job (`/healthz`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
    /// More host patterns it serves, relative to the gateway's domain, `*` standing for part of one label:
    /// `*--*.apps` for pages' apps (`recipes--personal.apps.localhost`). The Host header reaches the service intact.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
}

impl ServiceListening {
    /// Why this announcement can't be used, if it can't.
    pub fn problem(&self) -> Option<String> {
        let label = |s: &str, star: bool| {
            !s.is_empty()
                && s.len() <= 63
                && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || (star && b == b'*'))
        };
        if !label(&self.name, false) || self.name.starts_with('-') || self.name.ends_with('-') {
            return Some(format!("service name {:?} isn't one DNS label", self.name));
        }
        if !self.url.starts_with("http://") {
            return Some(format!("service {} url {:?} isn't http://", self.name, self.url));
        }
        if let Some(h) = &self.health
            && !h.starts_with('/')
        {
            return Some(format!("service {} health {h:?} isn't a path", self.name));
        }
        if let Some(h) = self.hosts.iter().find(|h| !h.split('.').all(|l| label(l, true))) {
            return Some(format!("service {} host pattern {h:?} is malformed", self.name));
        }
        None
    }
}

/// `_fleetlayer.dev/session/renamed`: an unnamed session (`@<address>`) got its harness's ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRenamed {
    pub from: String,
    pub to: String,
}

/// `_fleetlayer.dev/session/open`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenSession {
    pub label: String,
    /// Absolute, and it exists.
    pub cwd: String,
    pub harness: String,
    /// The agent's own arguments, after its program.
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenedSession {
    pub address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

// ------------------------------------------------------------ delivery

/// `_fleetlayer.dev/session/deliver`: `text` is fully rendered by the node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Deliver {
    pub session_id: String,
    pub address: String,
    pub delivery_id: String,
    pub kind: DeliveryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subkind: Option<String>,
    #[serde(default)]
    pub untrusted: bool,
    pub from: From,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryKind {
    Message,
    Event,
    Wake,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct From {
    /// The sender as the agent should see it.
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    HarnessChannel,
    Typed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivered {
    pub via: Via,
}

// ------------------------------------------------------------ events, readings, sync

/// `_fleetlayer.dev/event/publish`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub channel: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
    pub summary: String,
    #[serde(default)]
    pub body: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    pub kind: String,
    pub key: String,
    pub at: Timestamp,
    /// The node it came from; set by the node, ignored when a plugin publishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Readings {
    pub readings: Vec<Reading>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncItem {
    pub name: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncTarget {
    pub node: String,
}

/// `_fleetlayer.dev/sync/publish`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncPublish {
    pub bundle: String,
    pub version: u64,
    pub secret: bool,
    pub targets: Vec<SyncTarget>,
    pub items: Vec<SyncItem>,
}

/// `_fleetlayer.dev/sync/apply`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncApply {
    pub bundle: String,
    pub version: u64,
    pub items: Vec<SyncItem>,
}

/// `_fleetlayer.dev/message/send`, from a plugin or the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSend {
    /// `<tab>@<node>`, `<tab>` (this node), or a session ID.
    pub to: String,
    pub text: String,
}

#[cfg(test)]
mod tests {

    #[test]
    fn service_announcements_are_checked() {
        let ok: ServiceListening = serde_json::from_str(
            r#"{"name":"pages","url":"http://127.0.0.1:4000","health":"/healthz","hosts":["*--*.apps"]}"#,
        )
        .unwrap();
        assert_eq!(ok.problem(), None);
        let minimal: ServiceListening = serde_json::from_str(r#"{"name":"hub","url":"http://127.0.0.1:1"}"#).unwrap();
        assert_eq!(minimal.problem(), None);
        assert_eq!(serde_json::to_string(&minimal).unwrap(), r#"{"name":"hub","url":"http://127.0.0.1:1"}"#);
        for bad in [
            ServiceListening { name: "Pages".into(), ..ok.clone() },
            ServiceListening { name: "a.b".into(), ..ok.clone() },
            ServiceListening { url: "https://x".into(), ..ok.clone() },
            ServiceListening { health: Some("healthz".into()), ..ok.clone() },
            ServiceListening { hosts: vec!["a..b".into()], ..ok.clone() },
        ] {
            assert!(bad.problem().is_some(), "{bad:?}");
        }
    }
    use super::*;

    #[test]
    fn terminal_modes_map_to_policy_verbs_and_read_as_snake_case() {
        assert_eq!(TerminalMode::Observe.verb(), "read");
        assert_eq!(TerminalMode::Control.verb(), "attach");
        let r: TerminalCommandRequest =
            serde_json::from_value(serde_json::json!({"sessionId":"a","mode":"control","cols":80,"rows":24})).unwrap();
        assert_eq!(r.mode, TerminalMode::Control);
    }
    use serde_json::json;

    #[test]
    fn session_meta_updates_merge_only_what_they_set() {
        let mut m: SessionMeta =
            serde_json::from_value(json!({"harness":"claude","tab":"planning","status":"working","running":true}))
                .unwrap();
        m.merge(&serde_json::from_value(json!({"status":"idle"})).unwrap());
        assert_eq!(m.status, Some(Status::Idle));
        assert_eq!(m.tab.as_deref(), Some("planning"));
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            json!({"harness":"claude","tab":"planning","status":"idle","running":true})
        );
    }

    #[test]
    fn how_a_session_ended_serializes_as_the_spec_says() {
        let h = How::AgentExited { final_status: Some(Status::Done), terminal_closed: false };
        assert_eq!(
            serde_json::to_value(h).unwrap(),
            json!({"kind":"agent_exited","finalStatus":"done","terminalClosed":false})
        );
        assert_eq!(serde_json::to_value(How::TabClosed).unwrap(), json!({"kind":"tab_closed"}));
    }

    #[test]
    fn plugin_roles_read_from_the_initialize_answer() {
        let p: PluginMeta = serde_json::from_value(json!({"protocol":1,"roles":{
            "session_manager":{"open":true,"close":true,"interrupt":true,"deliver":true,"events":true}}}))
        .unwrap();
        assert!(p.roles.session_manager.unwrap().events);
        assert!(p.roles.harness.is_none());
    }
}
