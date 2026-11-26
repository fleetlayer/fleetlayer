//! Readings on the node (`docs/protocol.md` section 9): plugins publish them, and the node sends them to the control
//! plane; plugins subscribe to kinds, and the node keeps one subscription for all of them with the control plane
//! (renewed after every reconnect) and passes each update to the plugins that asked for its kind.

use std::collections::{BTreeSet, HashMap};

use fleetlayer_protocol::ext;
use fleetlayer_protocol::link::method as l;
use fleetlayer_protocol::rpc::RpcError;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::Node;

#[derive(Default)]
pub struct Readings {
    /// The kinds each plugin subscribed to.
    subs: Mutex<HashMap<String, Vec<String>>>,
}

impl Readings {
    async fn kinds(&self) -> Vec<String> {
        let subs = self.subs.lock().await;
        let all: BTreeSet<String> = subs.values().flatten().cloned().collect();
        all.into_iter().collect()
    }
}

/// A plugin published readings (`{readings: [...]}`, or one reading).
pub async fn publish(node: &Node, p: &Value) {
    let mut list = match p.get("readings").and_then(Value::as_array) {
        Some(a) => a.clone(),
        None if p.get("kind").is_some() => vec![p.clone()],
        None => return,
    };
    for r in &mut list {
        r["node"] = json!(node.name);
    }
    node.link.send_or_hold(l::READINGS, json!({"readings": list})).await;
}

/// A plugin subscribes to `kinds`: answers the newest of each it may read, if the node is linked now.
pub async fn subscribe(node: &Node, plugin: &str, p: &Value) -> Result<Value, RpcError> {
    let kinds: Vec<String> = serde_json::from_value(p["kinds"].clone()).map_err(RpcError::invalid_params)?;
    node.readings.subs.lock().await.insert(plugin.to_string(), kinds.clone());
    let Some(peer) = node.link.peer().await else {
        // Sent when the link comes up.
        return Ok(json!({"readings": []}));
    };
    let all = node.readings.kinds().await;
    let r: Value = peer.request(l::READING_SUBSCRIBE, &json!({"kinds": all})).await?;
    let mine: Vec<Value> = r["readings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|x| kinds.iter().any(|k| x["kind"].as_str() == Some(k.as_str())))
        .cloned()
        .collect();
    Ok(json!({"readings": mine}))
}

/// After linking: renew the subscription, and pass the newest readings to the plugins that want them.
pub async fn resubscribe(node: &Node, peer: &fleetlayer_plugin::Peer) {
    let kinds = node.readings.kinds().await;
    if kinds.is_empty() {
        return;
    }
    match peer.request::<_, Value>(l::READING_SUBSCRIBE, &json!({"kinds": kinds})).await {
        Ok(r) => update(node, &r).await,
        Err(e) => tracing::warn!("renewing the readings subscription: {e}"),
    }
}

/// Readings from the control plane: to each plugin that subscribed to their kind.
pub async fn update(node: &Node, p: &Value) {
    let readings = p["readings"].as_array().cloned().unwrap_or_default();
    let subs = node.readings.subs.lock().await.clone();
    for (plugin, kinds) in subs {
        let mine: Vec<&Value> =
            readings.iter().filter(|r| kinds.iter().any(|k| r["kind"].as_str() == Some(k.as_str()))).collect();
        if mine.is_empty() {
            continue;
        }
        if let Some(h) = node.plugin(&plugin)
            && let Some(peer) = h.peer().await
        {
            peer.notify(ext::method::READING_UPDATE, &json!({"readings": mine}));
        }
    }
}
