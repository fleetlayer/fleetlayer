//! The part of the Agent Client Protocol (v1) Fleetlayer uses. Field names follow ACP's schema; Fleetlayer's own
//! fields travel in `_meta` (see [`crate::ext`]).

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const SESSION_LIST: &str = "session/list";
    pub const SESSION_UPDATE: &str = "session/update";
    pub const SESSION_CLOSE: &str = "session/close";
    pub const SESSION_CANCEL: &str = "session/cancel";
}

pub type Meta = Map<String, Value>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Implementation {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub version: String,
}

/// Capabilities are open objects; Fleetlayer reads and writes only their `_meta`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Capabilities {
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeRequest {
    pub protocol_version: u16,
    #[serde(default)]
    pub client_capabilities: Capabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_info: Option<Implementation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    pub protocol_version: u16,
    #[serde(default)]
    pub agent_capabilities: Capabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_info: Option<Implementation>,
    #[serde(default)]
    pub auth_methods: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsResponse {
    pub sessions: Vec<SessionInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<Timestamp>,
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

/// `session/update`, agent to client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotification {
    pub session_id: String,
    pub update: SessionUpdate,
}

/// The `session/update` variants Fleetlayer reads. Every other variant is kept as it came.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SessionUpdate {
    Info(SessionInfoUpdate),
    Other(Value),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoUpdate {
    /// Always `session_info_update`.
    pub session_update: SessionInfoTag,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<Timestamp>,
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionInfoTag {
    #[serde(rename = "session_info_update")]
    SessionInfoUpdate,
}

/// `session/close` and `session/cancel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRef {
    pub session_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_session_info_update_parses_and_others_are_kept() {
        let n: SessionNotification = serde_json::from_value(json!({"sessionId":"s1","update":{
            "sessionUpdate":"session_info_update","title":"planning","_meta":{"fleetlayer.dev/session":{"status":"idle"}}}}))
        .unwrap();
        let SessionUpdate::Info(u) = n.update else { panic!("not an info update") };
        assert_eq!(u.title.as_deref(), Some("planning"));
        assert!(u.meta.unwrap().contains_key("fleetlayer.dev/session"));

        let n: SessionNotification = serde_json::from_value(json!({"sessionId":"s1","update":{
            "sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}))
        .unwrap();
        assert!(matches!(n.update, SessionUpdate::Other(_)));
    }
}
