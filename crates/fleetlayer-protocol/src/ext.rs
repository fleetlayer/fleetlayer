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
    /// Plugin → node: every node and running session in the fleet that this node may see, as the control plane has
    /// them, without summaries (light enough to poll). `{}` → `{nodes, sessions}`; grant `fleet.query`.
    pub const FLEET_QUERY: &str = "_fleetlayer.dev/fleet/query";
    /// Plugin → node: part of a session's transcript, here or on another node (`read` on it). `{to, offset?, tail?,
    /// max?, before?, lineMax?, types?}` (`link::TranscriptRange`) → `link::TranscriptChunk`; grant `transcript.read`.
    pub const TRANSCRIPT_READ: &str = "_fleetlayer.dev/transcript/read";
    /// Plugin → node: a summary event (`crate::summary::SummaryEvent`), for the control plane.
    pub const SUMMARY_PUBLISH: &str = "_fleetlayer.dev/summary/publish";
    /// Node → summaries plugin: a session's catch-up. `{sessionId}` → `crate::summary::Catchup`.
    pub const SUMMARY_CATCHUP: &str = "_fleetlayer.dev/summary/catchup";
    /// Node → summaries plugin: import claude-summarizer's history. `{dir}` → `{records}`.
    pub const SUMMARY_IMPORT: &str = "_fleetlayer.dev/summary/import";
    pub const HARNESS_TRANSCRIPT: &str = "_fleetlayer.dev/harness/transcript";
    /// Node → harness plugin: every transcript file it knows on this machine, its sessions' and their subagents'.
    /// `{}` → `{transcripts: [TranscriptFile]}`. Asked only of a plugin whose harness role says `list`.
    pub const HARNESS_TRANSCRIPTS: &str = "_fleetlayer.dev/harness/transcripts";
    /// Node → harness plugin, before a session of its harness starts in `cwd`: get the folder ready (Claude Code: mark
    /// it trusted). `HarnessPrepare` → `{}`.
    pub const HARNESS_PREPARE: &str = "_fleetlayer.dev/harness/prepare";
    /// Node → harness plugin: where a stored session worked, for resuming it by its ID alone. `{sessionId}` →
    /// `Located`; `-32002` when it doesn't know the session.
    pub const HARNESS_LOCATE: &str = "_fleetlayer.dev/harness/locate";
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
    /// Node to a `sync_target` plugin: remove everything you wrote from every bundle (a role's window ended).
    pub const SYNC_FORGET: &str = "_fleetlayer.dev/sync/forget";
    pub const READING_PUBLISH: &str = "_fleetlayer.dev/reading/publish";
    /// Plugin → node (notification): whether it's doing its job. `PluginStatusReport`; the node shows `problem` in its
    /// status and health and publishes the report as a `plugin.status` reading keyed by the plugin's name.
    pub const PLUGIN_STATUS: &str = "_fleetlayer.dev/plugin/status";
    /// Plugin → node: answer a session's blocked prompt, here or on another node (grant `session.answer`).
    /// `{to, prompt, answers, why?}` → `link::SessionActed`.
    pub const SESSION_ANSWER: &str = "_fleetlayer.dev/session/answer";
    /// Plugin → node: close a session, here or on another node (grant `session.close`). `{to, why?}` →
    /// `link::SessionActed`.
    pub const SESSION_CLOSE: &str = "_fleetlayer.dev/session/close";
    /// Node → session manager: press keys in a session, or type literal text (no Enter unless a key says so).
    /// `SessionKeys` → `{}`.
    pub const SESSION_KEYS: &str = "_fleetlayer.dev/session/keys";
    /// Node → session manager: the last `lines` lines of a session's screen as plain text. `{sessionId, lines}` →
    /// `{text}`.
    pub const SESSION_SCREEN: &str = "_fleetlayer.dev/session/screen";
    pub const READING_SUBSCRIBE: &str = "_fleetlayer.dev/reading/subscribe";
    pub const READING_UPDATE: &str = "_fleetlayer.dev/reading/update";
    /// Plugin → node: may this person or agent do this? `link::AccessCheck` → `link::AccessDecision`, decided by the
    /// control plane. Needs the `access.check` grant.
    pub const ACCESS_CHECK: &str = "_fleetlayer.dev/access/check";
    /// Plugin → node (notification): this plugin serves HTTP here (`ServiceListening`), sent after it has bound.
    /// Idempotent: resending replaces the entry of that name; the node drops a plugin's entries when it exits.
    pub const SERVICE_LISTENING: &str = "_fleetlayer.dev/service/listening";
    /// Plugin → node (notification): the pages spaces this plugin hosts, `{spaces: [name]}`, replacing its earlier list,
    /// so `fleetlayer pages push` from other machines reaches them. The node drops them when the plugin exits.
    pub const PAGES_SPACES: &str = "_fleetlayer.dev/pages/spaces";
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
    /// It answers `SESSION_KEYS` (press keys and type text in a session) and `SESSION_SCREEN` (read its screen).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keys: bool,
}

/// Watching a terminal, typing into it beside whoever has it open, or driving it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TerminalMode {
    /// Watch only. The caller may still ask for scrollback (`terminal.scrollback`); nothing else reaches it.
    #[default]
    Observe,
    /// Watch and type (`terminal.keys`, `terminal.paste`), never resizing the terminal or taking it from the person's
    /// own window: herdr's `observe --input`. For a phone or a chat beside the person's terminal.
    Input,
    /// Drive it: raw input, and the terminal takes the caller's size.
    Control,
}

impl TerminalMode {
    /// The access policy's verb: `read` to watch, `attach` to type.
    pub fn verb(self) -> &'static str {
        match self {
            TerminalMode::Observe => "read",
            TerminalMode::Input | TerminalMode::Control => "attach",
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
    /// It answers `_fleetlayer.dev/harness/prepare`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prepare: bool,
    /// It answers `_fleetlayer.dev/harness/locate`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub locate: bool,
    /// It answers `_fleetlayer.dev/harness/transcripts`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub list: bool,
}

/// One transcript file a harness plugin knows: a session's own, or one of its subagents' (`agentId`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptFile {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub path: String,
    /// `claude-jsonl`, `codex-jsonl`.
    pub format: String,
}

/// `_fleetlayer.dev/harness/prepare`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessPrepare {
    pub harness: String,
    /// Absolute.
    pub cwd: String,
}

/// `_fleetlayer.dev/harness/locate`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Located {
    pub cwd: String,
    pub harness: String,
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
    /// What the agent is waiting for the person to answer (a question, a permission prompt, a plan). In an update,
    /// `null` clears it and absent leaves it as it was.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    pub prompt: Option<Option<BlockedPrompt>>,
    /// Background work still running when the agent last stopped (best effort: from the harness's own report). `[]`:
    /// none; absent: not known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<Vec<BackgroundTask>>,
}

/// A prompt an agent is blocked on, as its harness reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockedPrompt {
    /// Stable while it shows, new for every prompt (the harness's tool call ID when it has one).
    pub id: String,
    pub kind: PromptKind,
    /// When it appeared.
    pub at: Timestamp,
    /// `hook` (the harness said so), or `screen` (read off the terminal: less sure).
    pub source: String,
    /// For `question`: each question, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<PromptQuestion>,
    /// For `permission`: the tool it asks to run, and its input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<PromptTool>,
    /// For `plan`: the plan's text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// For `permission` and `plan`: the menu's options, as the terminal shows them (`optionsFrom: "screen"`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<PromptOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options_from: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptKind {
    Question,
    Permission,
    Plan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptQuestion {
    #[serde(default)]
    pub header: String,
    pub question: String,
    #[serde(default)]
    pub multi_select: bool,
    pub options: Vec<PromptOption>,
    /// The option number that takes free text ("Type something"), when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptOption {
    pub n: u32,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptTool {
    pub name: String,
    #[serde(default)]
    pub input: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTask {
    /// The harness's own word for it (`bash`, `agent`, `monitor`...).
    pub kind: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// Absent → None; `null` → Some(None); a value → Some(Some(v)).
mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<T: Serialize, S: Serializer>(v: &Option<Option<T>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(inner) => inner.serialize(s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
        Option::<T>::deserialize(d).map(Some)
    }
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
            deliverable,
            background
        );
        // A prompt cleared (`null`) is gone, not unknown.
        if let Some(p) = &update.prompt {
            self.prompt = p.clone().map(Some);
        }
    }

    /// The prompt it's blocked on now, if any.
    pub fn blocked_prompt(&self) -> Option<&BlockedPrompt> {
        self.prompt.as_ref().and_then(Option::as_ref)
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
    /// The session started, but its first prompt couldn't be typed in: why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_error: Option<String>,
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

/// One question's answer to a blocked prompt: the option number, several for a multiple choice, or free text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PromptAnswer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pick: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub picks: Vec<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// `_fleetlayer.dev/session/keys`: each step is a key (`1`, `enter`, `down`, `space`, `tab`, `esc`) or literal text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionKeys {
    pub session_id: String,
    pub steps: Vec<KeyStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyStep {
    Key(String),
    Text(String),
}

/// `_fleetlayer.dev/plugin/status`: a running plugin's own word on whether it does its job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PluginStatusReport {
    /// One line for a person, when it can't do its job now; null when it can.
    #[serde(default)]
    pub problem: Option<String>,
    /// A small object of numbers and times for monitors (at most [`PLUGIN_STATUS_DETAIL_MAX`] bytes as JSON).
    #[serde(default)]
    pub detail: Value,
}

/// The most `PluginStatusReport::detail` a node passes on, as JSON bytes; more is replaced by `{"truncated": true}`.
pub const PLUGIN_STATUS_DETAIL_MAX: usize = 4096;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Readings {
    pub readings: Vec<Reading>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncItem {
    pub name: String,
    pub content: String,
    /// Unix permission bits, for a plugin that keeps them (the files plugin keeps the executable bit). Absent: the
    /// target's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// A symbolic link's target (relative, inside the bundle's folder), for a plugin that carries links; `content` is
    /// then empty. A node older than this field ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

impl SyncItem {
    /// An item of text with no mode or link.
    pub fn text(name: impl Into<String>, content: impl Into<String>) -> Self {
        Self { name: name.into(), content: content.into(), mode: None, link: None }
    }
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
    /// The node that published it. A plugin applying secrets checks it against the source it trusts, since any node
    /// can run a plugin of the same name and publish a bundle of the same name. Older nodes leave it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// `_fleetlayer.dev/message/send`, from a plugin or the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSend {
    /// `<tab>@<node>`, `<tab>` (this node), or a session ID.
    pub to: String,
    pub text: String,
    /// The text is outside data (an email, a web page): the recipient gets it marked untrusted.
    #[serde(default)]
    pub untrusted: bool,
    /// A finer kind for the recipient's harness (`forward`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subkind: Option<String>,
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
        assert_eq!(TerminalMode::Input.verb(), "attach", "typing is attaching, even without a resize");
        assert_eq!(serde_json::to_value(TerminalMode::Input).unwrap(), "input");
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
