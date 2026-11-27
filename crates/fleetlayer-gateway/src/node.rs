//! Talking to the node over its local socket, with the same JSON-RPC the CLI uses: the services its plugins announce,
//! its own health, and alert messages to a session.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fleetlayer_plugin::{Handler, Peer};
use fleetlayer_protocol::rpc::RpcError;
use serde_json::{Value, json};

use crate::registry::Announced;

struct NoRequests;

#[async_trait]
impl Handler for NoRequests {
    async fn request(&self, _peer: &Peer, method: &str, _params: Value) -> Result<Value, RpcError> {
        Err(RpcError::method_not_found(method))
    }
}

async fn call(socket: &Path, method: &str, params: Value) -> anyhow::Result<Value> {
    let stream = tokio::time::timeout(Duration::from_secs(5), tokio::net::UnixStream::connect(socket))
        .await
        .map_err(|_| anyhow::anyhow!("the node's socket {} didn't answer", socket.display()))?
        .map_err(|e| anyhow::anyhow!("can't reach the node at {} ({e})", socket.display()))?;
    let (r, w) = stream.into_split();
    let (peer, task) = fleetlayer_plugin::connect(r, w, Arc::new(NoRequests));
    let res = peer.request_within::<_, Value>(method, &params, Duration::from_secs(10)).await;
    task.abort();
    res.map_err(|e| anyhow::anyhow!("{method}: {}", e.message))
}

/// The services the node's plugins announced.
pub async fn services(socket: &Path) -> anyhow::Result<Vec<Announced>> {
    let v = call(socket, "node/services", json!({})).await?;
    Ok(serde_json::from_value(v["services"].clone())?)
}

/// The node's own judgement of its health: None when healthy, else why not.
pub async fn health(socket: &Path) -> Result<(), String> {
    match call(socket, "node/status", json!({})).await {
        Ok(v) => match v["health"].as_str() {
            Some(problem) => Err(problem.to_string()),
            None => Ok(()),
        },
        Err(e) => Err(e.to_string()),
    }
}

/// Send a Fleetlayer message to a session.
pub async fn send(socket: &Path, to: &str, text: &str) -> anyhow::Result<()> {
    call(socket, "_fleetlayer.dev/message/send", json!({"to": to, "text": text})).await.map(|_| ())
}
