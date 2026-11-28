//! The node's link to the control plane: JSON-RPC over one WebSocket (`/v1/link`, the node's token as a bearer token).
//! `docs/link.md` describes it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ext::{DeliveryKind, From};

pub mod method {
    /// Node → control plane, first: `Hello` → `Welcome`.
    pub const HELLO: &str = "link/hello";
    /// Node → control plane (notifications): the node's whole session list, one session, a session gone.
    pub const SESSIONS: &str = "link/sessions";
    pub const SESSION: &str = "link/session";
    pub const SESSION_GONE: &str = "link/session_gone";
    /// Node → control plane: a message to a session on another node. `SendRequest` → `Delivered`.
    pub const SEND: &str = "link/send";
    /// Node → control plane: publish an event. `Event` → `PublishResult`.
    pub const PUBLISH: &str = "link/publish";
    pub const SUBSCRIBE: &str = "link/subscribe";
    pub const UNSUBSCRIBE: &str = "link/unsubscribe";
    pub const SUBSCRIPTIONS: &str = "link/subscriptions";
    /// Node → control plane: the events one of its sessions missed (delivery failed: offline, busy) since it last
    /// read them, or with `all` every event it was sent; `peek` leaves them unread. `EventsRequest` → `{events}`.
    pub const EVENTS: &str = "link/events";
    /// Node → control plane: every node and session in the fleet.
    pub const FLEET: &str = "link/fleet";
    /// Node → control plane (notification): `SummaryPush` without the node (the link knows it).
    pub const SUMMARY: &str = "link/summary";
    /// Node → control plane: search summaries. `{q, limit?}` → `{summaries: [...]}`.
    pub const SUMMARIES_SEARCH: &str = "link/summaries_search";
    /// Control plane → node: a session's catch-up from its summaries plugin. `{sessionId}` → `Catchup`.
    pub const CATCHUP: &str = "link/catchup";
    /// Node → control plane: open a stream to a tool another node shares. `ToolOpen` → `ToolOpened`.
    pub const TOOL_OPEN: &str = "link/tool_open";
    /// Control plane → node: start a shared tool for a caller and connect it to stream `streamId`. `ToolServe` → `{}`.
    pub const TOOL_SERVE: &str = "link/tool_serve";
    /// Node → control plane: may the caller of one of this node's tool streams make this tool call (the MCP gate,
    /// `docs/access-control.md`)? Decided as verb `call` on each target, audited, and filed for approval when an `ask:`
    /// rule matches. `ToolCall` → `ToolCallDecision`.
    pub const TOOL_CALL: &str = "link/tool_call";
    /// Node → control plane: open a stream to run a command another node shares (`docs/guides/shared-commands.md`).
    /// `CommandOpen` → `ToolOpened`.
    pub const COMMAND_OPEN: &str = "link/command_open";
    /// Control plane → node: take one run of a shared command from stream `streamId`. `CommandServe` → `{}`.
    pub const COMMAND_SERVE: &str = "link/command_serve";
    /// Node → control plane: may the caller of one of this node's command streams run it with these arguments?
    /// Decided as verb `run` on `command:<name>@<node>`, audited with the run's detail, and filed for approval when an
    /// `ask:` rule matches. `ToolCall` (no targets) → `ToolCallDecision`.
    pub const COMMAND_CALL: &str = "link/command_call";
    /// Node → control plane: open a stream to a session's terminal on another node. `TerminalOpen` → `ToolOpened`.
    pub const TERMINAL_OPEN: &str = "link/terminal_open";
    /// Control plane → node: start streaming one of its sessions' terminals into stream `streamId`. `TerminalServe` →
    /// `{}`.
    pub const TERMINAL_SERVE: &str = "link/terminal_serve";
    /// Node → control plane: sealed bundles for other nodes. `SyncPublishSealed` → `{results: [TargetResult]}`.
    pub const SYNC_PUBLISH: &str = "link/sync_publish";
    /// Control plane → node: a sealed bundle for this node. `SyncDeliver` → `{applied}`.
    pub const SYNC_DELIVER: &str = "link/sync_deliver";
    /// Control plane → node (notification): a target that missed a bundle is back; publish it again for it.
    /// `{bundle, node}`.
    pub const SYNC_WANTED: &str = "link/sync_wanted";
    /// Node → control plane (notification): readings its plugins published. `{readings}`.
    pub const READINGS: &str = "link/readings";
    /// Node → control plane: the reading kinds this node's plugins want, replacing its earlier list. `{kinds}` →
    /// `{readings}` (the newest of each it may read).
    pub const READING_SUBSCRIBE: &str = "link/reading_subscribe";
    /// Node → control plane: the newest readings of `kinds` this node may read, without changing its subscription
    /// (`fleetlayer readings`, `fleetlayer-claude-accounts`). `{kinds}` → `{readings}`.
    pub const READING_LATEST: &str = "link/reading_latest";
    /// Control plane → node (notification): new readings of kinds it subscribed to. `{readings}`.
    pub const READING_UPDATE: &str = "link/reading_update";
    /// Control plane → node: deliver to one of its sessions. `DeliverRequest` → `Delivered`.
    pub const DELIVER: &str = "link/deliver";
    /// Node → control plane: may this person or agent do this to something a tenant owns (a service's data, a page, an
    /// app)? Answered only if the node's `-@<node>` is a member of that tenant. `AccessCheck` → `AccessDecision`.
    pub const ACCESS_CHECK: &str = "link/access_check";
    /// Node → control plane (notification): the pages spaces its plugins host, `{spaces: [name]}`, replacing its earlier
    /// list. Sent when it links and when the list changes.
    pub const PAGES_SPACES: &str = "link/pages_spaces";
    /// Node → control plane: push a page into a space another node hosts (`fleetlayer pages push`), after `write` on
    /// `page:<space>/<name>`. `PagesOpen` → `ToolOpened`.
    pub const PAGES_OPEN: &str = "link/pages_open";
    /// Control plane → node: receive a page push into one of its spaces on stream `streamId`. `PagesServe` → `{}`.
    pub const PAGES_SERVE: &str = "link/pages_serve";
    /// Node → control plane: start or resume a session on another node, after `start` on `<tab>@<node>`.
    /// `SessionStart` → `SessionStarted`.
    pub const SESSION_START: &str = "link/session_start";
    /// Control plane → node: start or resume one of its sessions for a caller. `SessionServe` → `SessionStarted`.
    pub const SESSION_SERVE: &str = "link/session_serve";
    /// Node → control plane: close a session on another node, after `close` on `<session>@<node>`.
    /// `SessionActRequest` (action `close`) → `SessionActed`.
    pub const SESSION_CLOSE: &str = "link/session_close";
    /// Node → control plane: answer a session's blocked prompt on another node, after `answer` on `<session>@<node>`.
    /// `SessionActRequest` (action `answer`) → `SessionActed`.
    pub const SESSION_ANSWER: &str = "link/session_answer";
    /// Control plane → node: close or answer one of its sessions for a caller. `SessionActServe` → `SessionActed`.
    pub const SESSION_ACT_SERVE: &str = "link/session_act_serve";
    /// Node → control plane: part of a session's transcript on another node, after `read` on `<session>@<node>`.
    /// `TranscriptRead` → `TranscriptChunk`.
    pub const TRANSCRIPT_READ: &str = "link/transcript_read";
    /// Control plane → node: part of one of its sessions' transcripts, for a caller. `TranscriptServe` →
    /// `TranscriptChunk`.
    pub const TRANSCRIPT_SERVE: &str = "link/transcript_serve";
    /// Node → control plane: start a session that takes over another one's work, after `start` on the new session
    /// (and `read` on the old one, when the caller is someone else). The control plane keeps the note and links old
    /// and new in the fleet. `SessionHandoff` → `HandoffStarted`.
    pub const SESSION_HANDOFF: &str = "link/session_handoff";
    /// Node → control plane: search every transcript on another node for a text. Only hits in sessions the caller
    /// may read without asking come back. `TranscriptSearch` → `TranscriptHits`.
    pub const TRANSCRIPT_SEARCH: &str = "link/transcript_search";
    /// Control plane → node: search its transcripts for a caller. `TranscriptSearchServe` → `TranscriptHits`.
    pub const TRANSCRIPT_SEARCH_SERVE: &str = "link/transcript_search_serve";
    /// Node → control plane: one handoff, for the session that took the work over, or a caller with `read` on the
    /// session it came from. `HandoffGet` → `Handoff`.
    pub const HANDOFF_GET: &str = "link/handoff_get";
    /// Node → control plane (notification): the tools this node shares, `Tools`, replacing its earlier list. Sent when
    /// it links and when an opt-in changes. Names, descriptions and folder names only: never paths or secrets.
    pub const TOOLS: &str = "link/tools";
    /// Node → control plane: what one of its sessions may use (`fleetlayer me`). `MeRequest` → `Me`. Read-only: it
    /// files no approval request and counts as nobody's first use.
    pub const ME: &str = "link/me";
    /// Node → control plane: ask for a role's window (`fleetlayer role activate`). `RoleActivate` → `RoleActivated`.
    /// Files an approval for the person; the window opens when they approve it.
    pub const ROLE_ACTIVATE: &str = "link/role_activate";
    /// Node → control plane: close a role's window early (`fleetlayer role deactivate`). `RoleDeactivate` →
    /// `RoleActivated`.
    pub const ROLE_DEACTIVATE: &str = "link/role_deactivate";
    /// Node → control plane: the roles with windows and their state (`fleetlayer role list`). `RoleWindowsRequest` →
    /// `RoleWindows`.
    pub const ROLE_WINDOWS: &str = "link/role_windows";
    /// Control plane → node (notification): a window opened or closed. `WindowNotice`. The role's own nodes close its
    /// sessions, remove what was delivered to them and run their hook, then confirm with `link/window_closed`; nodes
    /// that watch the role (`windows:` in their config) run their hooks.
    pub const WINDOW: &str = "link/window";
    /// Node → control plane: a role's node finished closing a window (or says why it couldn't). `WindowClosed` → `{}`.
    pub const WINDOW_CLOSED: &str = "link/window_closed";
    /// Control plane → node (notification), as it stops after `fleetlayer move control`: it now answers at
    /// `Moved.url`. The node checks that address holds `Moved.proof` before sending its token there (never from
    /// `wss://` to `ws://`), links there, and saves it to node.yaml once the move is confirmed; until then, the old
    /// address stays its fallback.
    pub const MOVED: &str = "link/moved";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub version: String,
    pub machine: String,
    pub user: String,
    /// The node's age X25519 public key (`age1…`), for secrets sealed to it. The control plane keeps the first one it
    /// sees and refuses a different one until the node is enrolled again.
    #[serde(default, rename = "publicKey", skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    /// Roles whose windows this node watches (its config's `windows:`), to run hooks when one opens or closes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<String>,
}

/// `link/moved`: the control plane's new link URL (`wss://…/v1/link`), and the proof the new address must hold
/// (`GET <its web address>/v1/move-proof?nonce=<proof>`) before a node sends its token there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Moved {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    /// The name the control plane knows this node by (from its token).
    pub node: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionsPush {
    pub sessions: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionPush {
    pub session: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionGone {
    pub session_id: String,
}

/// A message for a session on another node, rendered by the sending node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendRequest {
    /// `<name>@<node>`.
    pub to: String,
    pub delivery_id: String,
    pub from: From,
    pub text: String,
    /// Why, in one line, when the sender gave a reason (`--why`): access rules can require one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
    /// The sender says the text is outside data; the control plane marks it untrusted (as it does for senders the
    /// policy calls untrusted).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub untrusted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subkind: Option<String>,
}

/// Deliver to the session `name` names on the receiving node. `text` is rendered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliverRequest {
    pub name: String,
    pub delivery_id: String,
    pub kind: DeliveryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subkind: Option<String>,
    #[serde(default)]
    pub untrusted: bool,
    pub from: From,
    pub text: String,
}

/// A session asks for a tool on another node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolOpen {
    pub tool: String,
    /// The node that shares it.
    pub node: String,
    /// The calling session's name (its tab), or `-` for a shell.
    pub caller: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolOpened {
    /// Connect to `/v1/stream/<streamId>?side=use` with the node token.
    pub stream_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOpen {
    /// The session's name (its tab) or ID on `node`.
    pub session: String,
    pub node: String,
    /// The calling session's name, or `-` for a shell.
    pub caller: String,
    pub mode: crate::ext::TerminalMode,
    pub cols: u16,
    pub rows: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalServe {
    pub stream_id: String,
    pub session: String,
    pub mode: crate::ext::TerminalMode,
    pub cols: u16,
    pub rows: u16,
    /// `<tab>@<node>` of the caller.
    pub caller: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolServe {
    pub stream_id: String,
    pub tool: String,
    /// `<tab>@<node>` of the caller.
    pub caller: String,
}

/// A session asks to run a command another node shares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandOpen {
    pub command: String,
    /// The node that shares it.
    pub node: String,
    /// The calling session's name (its tab), or `-` for a shell.
    pub caller: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandServe {
    pub stream_id: String,
    pub command: String,
    /// `<tab>@<node>` of the caller.
    pub caller: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
}

/// One command a node shares, as `link/tools` carries it: its name, what it does and what it takes, never the program
/// it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CommandInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub params: Vec<ParamInfo>,
    /// The node's own `who:` patterns: both they and the access policy must let a caller in.
    #[serde(default)]
    pub who: Vec<String>,
    /// The longest a run may take, in seconds.
    #[serde(default)]
    pub timeout: u64,
}

/// One parameter of a shared command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ParamInfo {
    pub name: String,
    /// `choice`, `text`, `pattern` or `file`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// For a choice: the values it takes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    /// For a pattern: the regular expression a value must match whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// For text: the longest value, in characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<usize>,
    /// For a file: the largest, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    /// For a file: the extension its name must end with (`.sh`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension: Option<String>,
    /// The value used when the caller gives none. A parameter without one must be given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

/// `link/tool_call`: the node serving a tool stream asks about one call its caller made. The caller and the tool come
/// from the stream (the control plane recorded them when it was opened), never from the asking node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub stream_id: String,
    /// What the call does, each decided as `tool:<tool>/<target>@<node>`: the MCP tool's name, and with argument
    /// rules more parts (`call/tools.google.gmail.send`). Every one must be allowed.
    pub targets: Vec<String>,
    /// A hash of the call's arguments: an approval `for: once` holds for exactly these arguments.
    pub args_hash: String,
    /// What the person sees when asked: the call, scrubbed of delivered secrets and cut short. Never in the audit log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
    /// What the audit log keeps of a shared command's run (`link/command_call`): the program and arguments, files by
    /// name, size and hash, never their contents. The detail can show more (a script whole); a tool call has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit: Option<String>,
    /// While waiting for the person: only look at this approval request (the stream's caller's), deciding and
    /// recording nothing. Answers `pending`, `denied`, or `ready` once approved, when the node asks again in full.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peek: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallDecision {
    /// `allowed`, `pending` (waiting for the person: `approval` names the request), `denied`, or for a peek `ready`.
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<String>,
    /// One line for the agent and the log.
    pub reason: String,
}

/// One bundle, sealed to each target node's key by the publishing node. The control plane relays each copy and keeps
/// none of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncPublishSealed {
    pub bundle: String,
    pub version: u64,
    pub copies: Vec<SealedCopy>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedCopy {
    pub node: String,
    /// An ASCII-armored age file holding the items as JSON, readable only with that node's key.
    pub sealed: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncDeliver {
    pub bundle: String,
    pub version: u64,
    /// The publishing node.
    pub from: String,
    pub sealed: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetResult {
    pub target: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventsRequest {
    /// `<tab>@<node>`: a session of the asking node.
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Every event sent to it, delivered or not, not only the missed ones.
    #[serde(default)]
    pub all: bool,
    /// Leave them unread.
    #[serde(default)]
    pub peek: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishResult {
    /// The event's dedupe key was seen before, so it went nowhere.
    pub duplicate: bool,
    pub deliveries: Vec<TargetResult>,
}

/// `link/access_check` and `_fleetlayer.dev/access/check`: a service asking whether someone may do something
/// (`docs/sso.md`, "How a service asks").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AccessCheck {
    /// The tenant that owns what's asked about: its ID or its slug (the sign-in's `fleetlayer.dev/tenant`). Empty: the
    /// asking node's own tenant (the egress plugin asking about `host:…`).
    #[serde(default)]
    pub tenant: String,
    /// `person:<handle>`, a person's ID (the sign-in's `sub`), or an agent (`<tab>@<node>`).
    pub who: String,
    /// A verb (`read`, `write`, `sign_in`, `admin`...).
    pub can: String,
    /// `service:<name>[/<owner>/…]`, `page:<space>/<path>`, `app:<space>/<name>` or `host:<name>[:<port>]`.
    pub to: String,
    /// The asker's one-line reason, recorded with the decision (and needed where a `justify:` rule applies): an agent
    /// asking the egress plugin for a host says why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// Only look: never file an approval request (an approved one in use is renewed). A node asks about an agent on
    /// another node (`-@<other>`, `<tab>@<other>`) only this way; requests are filed only in a node's own name.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub peek: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AccessDecision {
    pub allowed: bool,
    /// The rules behind it.
    pub rules: Vec<String>,
    /// One line for a person to read.
    pub reason: String,
    /// Waiting for the person: the approval request's ID (from v0.4.0; older control planes say it only in `reason`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<String>,
    /// The person refused it: the approval request's ID (from v0.4.0, answered to a `peek`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
    /// A `peek` found it needs the person and nobody has asked yet (from v0.4.0).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub not_asked: bool,
}

/// `link/pages_open`: a session asks to push a page into a space.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PagesOpen {
    pub space: String,
    /// The page's short name (`signing-flow-qa`); its folder is `<date>-<name>`.
    pub name: String,
    /// The calling session's name (its tab), or `-` for a shell.
    pub caller: String,
    /// A new dated folder even when one with this name exists.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub new: bool,
    /// The pushing machine's date (`YYYY-MM-DD`), for a new folder's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
}

/// `link/pages_serve`: the hosting node receives the push on stream `streamId`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PagesServe {
    pub stream_id: String,
    pub space: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub new: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// `<tab>@<node>` of the caller.
    pub caller: String,
}

/// What a session start or resume asks for, wherever it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StartSpec {
    /// The tab's name. Required to start; a resume defaults to the session's old tab, else `resumed <id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<String>,
    /// Absolute. Required to start; a resume defaults to where the session worked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// `claude` (default) or `codex`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    /// The first prompt, typed in once the agent takes input. From another node it arrives rendered as a message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Resume this session ID instead of starting a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    /// The model, as the harness names it (`opus`, `claude-sonnet-5-5`, `gpt-6`). Default: the harness's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// How hard the model thinks (`low`, `medium`, `high`, `xhigh`, `max`, as the harness accepts). Default: the
    /// harness's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// `link/session_start`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStart {
    pub node: String,
    #[serde(flatten)]
    pub spec: StartSpec,
    /// The calling session's name (its tab), or `-` for a shell.
    pub caller: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
}

/// `link/session_serve`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionServe {
    #[serde(flatten)]
    pub spec: StartSpec,
    /// `<tab>@<node>` of the caller.
    pub caller: String,
}

/// A session that started (or resumed) and takes input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStarted {
    pub node: String,
    pub tab: String,
    pub address: String,
    /// The harness's session ID; null until the agent reports one.
    pub session_id: Option<String>,
    /// The session started, but its first prompt couldn't be typed in: why.
    pub prompt_error: Option<String>,
}

/// `link/transcript_search`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptSearch {
    pub node: String,
    #[serde(flatten)]
    pub query: TranscriptQuery,
    /// The calling session's name (its tab), or `-` for a shell.
    pub caller: String,
}

/// `link/transcript_search_serve`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptSearchServe {
    #[serde(flatten)]
    pub query: TranscriptQuery,
    /// `<name>@<node>` of the caller.
    pub caller: String,
}

/// What to look for: `text`, found whatever its case, in every transcript on the node (subagents' too), newest
/// first. At most `limit` hits (default 50, at most [`TRANSCRIPT_SEARCH_MAX`]), and at most `perSession` in one
/// transcript (default 3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptQuery {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_session: Option<u32>,
}

/// The most hits a transcript search returns.
pub const TRANSCRIPT_SEARCH_MAX: u32 = 500;

/// What a node's transcripts hold of a text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptHits {
    pub node: String,
    pub hits: Vec<TranscriptHit>,
    /// Transcripts looked through.
    pub searched: u64,
    /// The search stopped early (the limit, or its time ran out) with transcripts it didn't look through.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
    /// Always 0: hits in sessions the caller may not read are left out without saying how many (a count would tell
    /// which sessions hold the text). Kept for older readers.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub hidden: u64,
}

/// One line of a transcript that holds the text, with the text around it (secrets delivered to the node redacted).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptHit {
    pub session_id: String,
    /// A subagent's transcript, under `sessionId`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// The session's tab, when the node knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<String>,
    /// Where the line starts in its transcript (`fleetlayer transcript --offset`).
    pub offset: u64,
    /// The line's `type` and `timestamp`, when it has them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    pub excerpt: String,
}

/// `link/session_handoff`: start `start` as the session taking over `from`'s work. The node has already put the
/// handoff's instructions (its ID, where the note and the old transcript are) in the first prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHandoff {
    /// Its ID, made by the node (`h` and 16 hex digits); the control plane refuses one it already has.
    pub id: String,
    /// The session whose work moves: `<tab or session ID>@<node>`.
    pub from: String,
    /// What the old session (or whoever hands off) says the new one needs to know.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(flatten)]
    pub start: SessionStart,
}

/// A handoff whose new session started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffStarted {
    pub handoff: String,
    #[serde(flatten)]
    pub started: SessionStarted,
}

/// `link/handoff_get`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffGet {
    pub id: String,
    /// The calling session's name (its tab), or `-` for a shell.
    pub caller: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
}

/// One session's work moved to another: `from` and `to` are `<tab>@<node>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Handoff {
    pub id: String,
    pub from: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_session_id: Option<String>,
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// `<name>@<node>` of whoever handed off.
    pub by: String,
    /// RFC 3339.
    pub created_at: String,
}

/// `link/tools`: what a node shares, for `fleetlayer me`. The control plane keeps it in memory only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Tools {
    pub tools: Vec<ToolInfo>,
    /// The commands it shares (`commands:` in node.yaml).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<CommandInfo>,
}

/// One tool a node shares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
    pub name: String,
    /// One line from node.yaml's `description:`, for agents choosing a tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The node's own `allow:` patterns: both they and the access policy must let a caller in.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Why it isn't offered now (off its networks), or None when it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_offered: Option<String>,
    /// For a fleetlayer-shared-folders tool: its folders and what each allows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<FolderInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FolderInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub upload: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delete: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub trusted: bool,
}

/// `link/me`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeRequest {
    /// The calling session's name (its tab), or `-` for a shell.
    pub caller: String,
}

/// What a session may use, as the control plane sees it now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Me {
    /// `<tab>@<node>`.
    pub address: String,
    /// Its messages are delivered as outside data (the policy's `untrusted:`).
    pub untrusted: bool,
    /// Verbs it must give a reason for (`--why`).
    #[serde(default)]
    pub justify: Vec<String>,
    /// Running sessions on other nodes it may message.
    #[serde(default)]
    pub message: Vec<String>,
    /// Of the running sessions on other nodes it can see, how many it may not message.
    #[serde(default)]
    pub message_refused: usize,
    /// Shared tools it may use or could ask for. Refused ones are left out.
    #[serde(default)]
    pub tools: Vec<MeTool>,
    /// Commands on other nodes it may run, or could ask to (`fleetlayer run`). Refused ones are left out.
    #[serde(default)]
    pub commands: Vec<MeCommand>,
    /// Pages spaces it may push to, or could ask for.
    #[serde(default)]
    pub pages: Vec<MeTarget>,
    /// Nodes it may start or resume sessions on (`fleetlayer session start --on`), decided for a new tab (`start` on `-@<node>`): its own
    /// node is always there (`allowed`), others say `allowed` or `ask`. Refused ones are left out.
    #[serde(default)]
    pub start: Vec<MeTarget>,
    /// Nodes it can see that can't say what they share, one sentence each: `laptop is too old to say (v0.3.8)`.
    #[serde(default)]
    pub unknown: Vec<String>,
    /// Its subscriptions: `channel` or `channel (subject)`.
    #[serde(default)]
    pub subscriptions: Vec<String>,
    /// The windowed roles it is one of the agents of, or may ask a window for, and their state.
    #[serde(default)]
    pub windows: Vec<WindowInfo>,
}

/// Ask for a role's window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleActivate {
    pub role: String,
    /// How long: `1h`, `12h`, `1d`.
    #[serde(rename = "for")]
    pub hold: String,
    /// The session or shell asking (`<tab>` or `-`), on the asking node.
    pub caller: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
}

/// Close a role's window early.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleDeactivate {
    pub role: String,
    pub caller: String,
}

/// What became of a request for a window, or a request to close one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RoleActivated {
    pub role: String,
    /// `pending` (waiting for the person), `open` (open already), `closing` (closed early: its nodes are cleaning up)
    /// or `off`.
    pub state: String,
    /// The approval request, while pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<String>,
    /// When the open window ends (milliseconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ends_at: Option<i64>,
    /// One sentence for the person or agent.
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RoleWindowsRequest {
    pub caller: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RoleWindows {
    pub windows: Vec<WindowInfo>,
}

/// A windowed role and its state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WindowInfo {
    pub role: String,
    /// `open`, `pending` (asked, waiting for the person) or `off`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ends_at: Option<i64>,
    /// The open window's ID (its approval), or the pending request's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The longest window the person may approve (milliseconds).
    pub longest_ms: i64,
    /// The asker is one of the role's agents.
    #[serde(default)]
    pub member: bool,
}

/// A window opened or closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowNotice {
    /// The window's ID (the approval that opened it).
    pub id: String,
    pub role: String,
    pub open: bool,
    /// When it ends, or ended (milliseconds). A node closes the window at this time by itself, linked or not.
    pub ends_at: i64,
    /// The receiving node is one of the role's: it closes the role's sessions, removes what was delivered to it, and
    /// confirms. False for a node that only watches the role.
    pub member: bool,
}

/// A role's node finished closing a window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowClosed {
    pub id: String,
    /// What didn't work (a hook that failed, a session that wouldn't close); None when everything did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// A target and the decision on it, without asking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MeTarget {
    /// `browser@laptop`, or a pages space's name.
    pub name: String,
    pub node: String,
    /// `allowed`, `ask` (the first use asks the person), `pending` (asked, not answered) or `refused` (the person
    /// said no, within the hour).
    pub state: String,
    /// The approval's or request's ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<String>,
    /// Until when an approval holds (milliseconds since 1970).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    /// The node is offline now.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub offline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MeTool {
    #[serde(flatten)]
    pub target: MeTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_offered: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folders: Vec<FolderInfo>,
}

/// A shared command in `fleetlayer me` and `fleetlayer commands`: `name` is `<command>@<node>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MeCommand {
    #[serde(flatten)]
    pub target: MeTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub params: Vec<ParamInfo>,
    #[serde(default)]
    pub timeout: u64,
}

/// Something done to a running session for a caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SessionAction {
    /// Close its tab or pane.
    Close,
    /// Answer its blocked prompt `prompt` (the `id` of the session's `meta.prompt`), one answer per question.
    Answer { prompt: String, answers: Vec<crate::ext::PromptAnswer> },
}

impl SessionAction {
    /// The access policy's verb for it.
    pub fn verb(&self) -> &'static str {
        match self {
            SessionAction::Close => "close",
            SessionAction::Answer { .. } => "answer",
        }
    }
}

/// `link/session_close` and `link/session_answer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionActRequest {
    pub node: String,
    /// Its tab or session ID on `node`.
    pub session: String,
    #[serde(flatten)]
    pub action: SessionAction,
    /// The calling session's name (its tab), `plugin:<name>` for a plugin, or `-` for a shell.
    pub caller: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
}

/// `link/session_act_serve`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionActServe {
    pub session: String,
    #[serde(flatten)]
    pub action: SessionAction,
    /// `<name>@<node>` of the caller.
    pub caller: String,
}

/// What was closed or answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionActed {
    pub node: String,
    pub session_id: String,
    pub tab: Option<String>,
    /// `closed` or `answered`.
    pub done: String,
}

/// Which part of a transcript to read: from byte `offset`, or the last `tail` bytes (starting at a line; when no whole
/// line starts inside them, the last whole line), ending at byte `before` (the end of the file when absent), at most
/// `max` bytes (capped at [`TRANSCRIPT_CHUNK_MAX`]). Neither offset nor tail: from the start. A forward read returns
/// the first lines that fit; a tail returns the newest. Only whole lines come back. A line longer than `line_max` (or
/// than `max`) comes back cut, with a marker, never dropped: see [`TRANSCRIPT_CUT_KEY`]. With `types`, only lines of
/// those kinds come back (the line's top-level `type`, or `tool_result` for a user line carrying tool results); the
/// rest are passed over and `next` moves past them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptRange {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_max: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub types: Option<Vec<String>>,
    /// One of the session's subagents' transcripts instead of its own (Claude Code's `agent-<id>.jsonl`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

/// The most a transcript read returns at once.
pub const TRANSCRIPT_CHUNK_MAX: u64 = 1 << 20;

/// The most of a transcript one tail read looks through (the newest lines in it that fit come back).
pub const TRANSCRIPT_TAIL_MAX: u64 = 256 << 20;

/// A line cut to fit carries this top-level key: `{"bytes": <its length in the file>}`. A JSON line keeps its shape,
/// with its longest strings cut short and ending `…[cut: 1.4 MB]`; a line too big to parse, or that can't be made
/// small enough that way, becomes `{"type": <its type, when found>, "_fleetlayerCut": {"bytes": …, "head": <its
/// first bytes as text>}}`.
pub const TRANSCRIPT_CUT_KEY: &str = "_fleetlayerCut";

/// `link/transcript_read`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptRead {
    pub node: String,
    /// Its tab or session ID on `node`.
    pub session: String,
    #[serde(flatten)]
    pub range: TranscriptRange,
    /// The calling session's name (its tab), `plugin:<name>` for a plugin, or `-` for a shell.
    pub caller: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub justification: Option<String>,
}

/// `link/transcript_serve`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptServe {
    pub session: String,
    #[serde(flatten)]
    pub range: TranscriptRange,
    /// `<name>@<node>` of the caller.
    pub caller: String,
}

/// Part of a transcript: `data` (base64) is the lines from `offset`, with every secret delivered to the serving node
/// redacted; `next` is where the following read starts (redaction, cut lines and skipped ones change the length, so it
/// isn't `offset` plus the data's); `size` is the whole file's size now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptChunk {
    pub session_id: String,
    /// The harness's transcript format: `claude-jsonl`.
    pub format: String,
    pub size: u64,
    pub offset: u64,
    pub next: u64,
    pub data: String,
    /// How many of the lines in `data` were cut to fit.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cut: u64,
    /// How many lines between `offset` and `next` were passed over by `types`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub skipped: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}
