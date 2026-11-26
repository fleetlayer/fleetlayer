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
    /// Node → control plane: every node and session in the fleet.
    pub const FLEET: &str = "link/fleet";
    /// Node → control plane (notification): `SummaryPush` without the node (the link knows it).
    pub const SUMMARY: &str = "link/summary";
    /// Node → control plane: search summaries. `{q, limit?}` → `{summaries: [...]}`.
    pub const SUMMARIES_SEARCH: &str = "link/summaries_search";
    /// Control plane → node: deliver to one of its sessions. `DeliverRequest` → `Delivered`.
    pub const DELIVER: &str = "link/deliver";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub version: String,
    pub machine: String,
    pub user: String,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetResult {
    pub target: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishResult {
    /// The event's dedupe key was seen before, so it went nowhere.
    pub duplicate: bool,
    pub deliveries: Vec<TargetResult>,
}
