//! JSON-RPC 2.0, as ACP uses it: UTF-8 JSON, one message per line, requests in both directions.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Any message on the wire. Which fields are set says what it is: see [`Message::kind`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Message {
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// What a [`Message`] is.
#[derive(Debug, PartialEq)]
pub enum Kind<'a> {
    Request { id: &'a Value, method: &'a str },
    Notification { method: &'a str },
    Response { id: &'a Value },
    Invalid,
}

impl Message {
    pub fn kind(&self) -> Kind<'_> {
        match (&self.id, &self.method) {
            (Some(id), Some(method)) => Kind::Request { id, method },
            (None, Some(method)) => Kind::Notification { method },
            (Some(id), None) if self.result.is_some() || self.error.is_some() => Kind::Response { id },
            _ => Kind::Invalid,
        }
    }

    pub fn request(id: i64, method: &str, params: Value) -> Message {
        Message { jsonrpc: "2.0".into(), id: Some(id.into()), method: Some(method.into()), params: Some(params), ..Default::default() }
    }

    pub fn notification(method: &str, params: Value) -> Message {
        Message { jsonrpc: "2.0".into(), method: Some(method.into()), params: Some(params), ..Default::default() }
    }

    pub fn response(id: Value, result: Result<Value, RpcError>) -> Message {
        let (result, error) = match result {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e)),
        };
        Message { jsonrpc: "2.0".into(), id: Some(id), result, error, ..Default::default() }
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for RpcError {}

/// Error codes: JSON-RPC's, ACP's and Fleetlayer's.
pub mod code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    /// ACP: the session isn't known.
    pub const UNKNOWN_SESSION: i64 = -32002;
    /// Fleetlayer: the session isn't on this machine, or isn't running.
    pub const NOT_HERE: i64 = -32010;
    /// Fleetlayer: understood, and won't do it. The message says why, in a sentence a person can read.
    pub const REFUSED: i64 = -32011;
    /// Fleetlayer: the other side went away before answering.
    pub const CLOSED: i64 = -32099;
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> RpcError {
        RpcError { code, message: message.into(), data: None }
    }
    pub fn method_not_found(method: &str) -> RpcError {
        RpcError::new(code::METHOD_NOT_FOUND, format!("unknown method {method}"))
    }
    pub fn invalid_params(e: impl std::fmt::Display) -> RpcError {
        RpcError::new(code::INVALID_PARAMS, e.to_string())
    }
    pub fn internal(e: impl std::fmt::Display) -> RpcError {
        RpcError::new(code::INTERNAL_ERROR, e.to_string())
    }
    pub fn not_here(message: impl Into<String>) -> RpcError {
        RpcError::new(code::NOT_HERE, message)
    }
    pub fn refused(message: impl Into<String>) -> RpcError {
        RpcError::new(code::REFUSED, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn messages_say_what_they_are() {
        let m: Message = serde_json::from_value(json!({"jsonrpc":"2.0","id":1,"method":"session/list","params":{}})).unwrap();
        assert_eq!(m.kind(), Kind::Request { id: &json!(1), method: "session/list" });
        let m: Message = serde_json::from_value(json!({"jsonrpc":"2.0","method":"session/update","params":{}})).unwrap();
        assert_eq!(m.kind(), Kind::Notification { method: "session/update" });
        let m: Message = serde_json::from_value(json!({"jsonrpc":"2.0","id":"a","result":null})).unwrap();
        // A null result deserializes as None: still a response, because it has an id and no method.
        assert_eq!(m.kind(), Kind::Invalid, "serde reads result: null as absent");
        let m: Message = serde_json::from_value(json!({"jsonrpc":"2.0","id":"a","result":{}})).unwrap();
        assert_eq!(m.kind(), Kind::Response { id: &json!("a") });
    }

    #[test]
    fn a_response_serializes_one_of_result_or_error() {
        let ok = serde_json::to_value(Message::response(json!(3), Ok(json!({})))).unwrap();
        assert_eq!(ok, json!({"jsonrpc":"2.0","id":3,"result":{}}));
        let err = serde_json::to_value(Message::response(json!(3), Err(RpcError::not_here("gone")))).unwrap();
        assert_eq!(err, json!({"jsonrpc":"2.0","id":3,"error":{"code":-32010,"message":"gone"}}));
    }
}
