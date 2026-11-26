//! Sync and sealed secrets (`docs/protocol.md` section 8). A `sync_source` plugin publishes a bundle through this
//! node: the node seals the items to each target node's age key and sends the copies through the control plane,
//! which relays them and keeps none. On a target, the node opens its copy and hands the items to the local plugin that
//! applies that bundle (`sync_target`), and the result flows back. Status travels, content never does.
//!
//! The source pins each target's key the first time it sees it (`~/.local/state/fleetlayer/known-keys.json`) and
//! refuses to seal to a changed key until a person removes the old pin. The target applies only versions newer than
//! the last it applied (`~/.local/state/fleetlayer/applied.json`). The node keeps the last publication of each
//! bundle in memory, so when a target that was offline comes back (`link/sync_wanted`), it seals a copy for it again.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use fleetlayer_protocol::ext::{self, SyncApply, SyncItem, SyncPublish};
use fleetlayer_protocol::link::{self as l, SealedCopy, SyncDeliver, SyncPublishSealed};
use fleetlayer_protocol::rpc::RpcError;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::Node;

#[derive(Default)]
pub struct Sync {
    /// The last publication of each bundle, for targets that come back.
    last: Mutex<HashMap<String, SyncPublish>>,
    lock: Mutex<()>,
}

fn state_dir(node: &Node) -> PathBuf {
    node.home.join(".local/state/fleetlayer")
}

fn read_map(path: &std::path::Path) -> HashMap<String, Value> {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write_map(path: &std::path::Path, map: &HashMap<String, Value>) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(map).unwrap_or_default())?;
    std::fs::rename(tmp, path)
}

/// Seal `items` to an age recipient, armored.
pub fn seal(recipient: &str, items: &[SyncItem]) -> anyhow::Result<String> {
    let r = age::x25519::Recipient::from_str(recipient).map_err(|e| anyhow::anyhow!("bad key {recipient}: {e}"))?;
    let plain = serde_json::to_vec(items)?;
    let enc = age::Encryptor::with_recipients(std::iter::once(&r as &dyn age::Recipient))?;
    let mut out = vec![];
    let armor = age::armor::ArmoredWriter::wrap_output(&mut out, age::armor::Format::AsciiArmor)?;
    let mut w = enc.wrap_output(armor)?;
    w.write_all(&plain)?;
    w.finish()?.finish()?;
    Ok(String::from_utf8(out)?)
}

/// Open a sealed copy with this node's identity.
pub fn open(identity: &age::x25519::Identity, sealed: &str) -> anyhow::Result<Vec<SyncItem>> {
    let dec = age::Decryptor::new(age::armor::ArmoredReader::new(sealed.as_bytes()))?;
    let mut r = dec.decrypt(std::iter::once(identity as &dyn age::Identity))?;
    let mut plain = vec![];
    r.read_to_end(&mut plain)?;
    Ok(serde_json::from_slice(&plain)?)
}

/// A bundle a plugin may publish: it must start with the plugin's name.
fn owns(plugin: &str, bundle: &str) -> bool {
    bundle.strip_prefix(plugin).is_some_and(|rest| rest.starts_with('/'))
}

/// A `sync_source` plugin publishes a bundle.
pub async fn publish(node: &Arc<Node>, plugin: &str, p: SyncPublish) -> Result<Value, RpcError> {
    if !owns(plugin, &p.bundle) {
        return Err(RpcError::refused(format!("plugin {plugin} may publish only bundles named {plugin}/…")));
    }
    if !p.secret {
        return Err(RpcError::refused("only secret (sealed) bundles are supported so far"));
    }
    node.sync.last.lock().await.insert(p.bundle.clone(), p.clone());
    let targets: Vec<String> = p.targets.iter().map(|t| t.node.clone()).collect();
    send(node, &p, &targets).await
}

/// Seal `p` for `targets` and send it.
async fn send(node: &Arc<Node>, p: &SyncPublish, targets: &[String]) -> Result<Value, RpcError> {
    let peer =
        node.link.peer().await.ok_or_else(|| RpcError::not_here("this node isn't linked to a control plane now"))?;
    let fleet: Value = peer.request(l::method::FLEET, &json!({})).await?;
    let keys: HashMap<String, String> = fleet["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| Some((n["name"].as_str()?.to_string(), n["publicKey"].as_str()?.to_string())))
        .collect();
    let mut copies = vec![];
    let mut results = vec![];
    {
        let _guard = node.sync.lock.lock().await;
        let pins_path = state_dir(node).join("known-keys.json");
        let mut pins = read_map(&pins_path);
        for t in targets {
            let Some(key) = keys.get(t) else {
                results.push(json!({"target": t, "ok": false, "error": format!("node {t} has no key yet (it hasn't linked since keys arrived)")}));
                continue;
            };
            match pins.get(t).and_then(Value::as_str) {
                Some(pinned) if pinned != key => {
                    tracing::error!(target = %t, "refusing to seal: its key changed since it was pinned");
                    results.push(json!({"target": t, "ok": false, "error": format!(
                        "node {t}'s key changed ({pinned} → {key}); remove its pin in {} if that's expected", pins_path.display())}));
                    continue;
                }
                Some(_) => {}
                None => {
                    tracing::info!(target = %t, fingerprint = %crate::keys::fingerprint(key), "pinned a node's key");
                    pins.insert(t.clone(), json!(key));
                }
            }
            match seal(key, &p.items) {
                Ok(sealed) => copies.push(SealedCopy { node: t.clone(), sealed }),
                Err(e) => results.push(json!({"target": t, "ok": false, "error": e.to_string()})),
            }
        }
        write_map(&pins_path, &pins).map_err(RpcError::internal)?;
    }
    if !copies.is_empty() {
        let r: Value = peer
            .request_within(
                l::method::SYNC_PUBLISH,
                &SyncPublishSealed { bundle: p.bundle.clone(), version: p.version, copies },
                Duration::from_secs(60),
            )
            .await?;
        results.extend(r["results"].as_array().cloned().unwrap_or_default());
    }
    Ok(json!({"results": results}))
}

/// A target that missed a bundle is back.
pub async fn wanted(node: &Arc<Node>, bundle: &str, target: &str) {
    let last = node.sync.last.lock().await.get(bundle).cloned();
    if let Some(p) = last
        && p.targets.iter().any(|t| t.node == target)
    {
        match send(node, &p, &[target.to_string()]).await {
            Ok(r) => tracing::info!(bundle, target, result = %r, "sent a bundle again to a node that came back"),
            Err(e) => tracing::warn!(bundle, target, "sending a bundle again: {e}"),
        }
    }
}

/// The control plane brings a sealed copy for this node.
pub async fn deliver(node: &Arc<Node>, d: SyncDeliver) -> Result<Value, RpcError> {
    let items = open(&node.identity, &d.sealed).map_err(|e| RpcError::refused(format!("opening {}: {e}", d.bundle)))?;
    let _guard = node.sync.lock.lock().await;
    let applied_path = state_dir(node).join("applied.json");
    let mut applied = read_map(&applied_path);
    if applied.get(&d.bundle).and_then(Value::as_u64).is_some_and(|v| v >= d.version) {
        return Ok(json!({"applied": false, "reason": "an equal or newer version is already applied"}));
    }
    let mut target = None;
    for p in &node.plugins {
        if p.roles().await.sync_target.is_some_and(|r| r.bundles.contains(&d.bundle)) {
            target = Some(p.clone());
            break;
        }
    }
    let plugin =
        target.ok_or_else(|| RpcError::not_here(format!("no plugin on {} applies {}", node.name, d.bundle)))?;
    let peer =
        plugin.peer().await.ok_or_else(|| RpcError::not_here(format!("plugin {} isn't running", plugin.name())))?;
    let r: Value = peer
        .request_within(
            ext::method::SYNC_APPLY,
            &SyncApply { bundle: d.bundle.clone(), version: d.version, items },
            Duration::from_secs(60),
        )
        .await?;
    if r["applied"].as_bool() == Some(true) {
        applied.insert(d.bundle.clone(), json!(d.version));
        write_map(&applied_path, &applied).map_err(RpcError::internal)?;
    }
    tracing::info!(bundle = %d.bundle, version = d.version, from = %d.from, applied = %r["applied"], "applied a bundle");
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_copy_opens_only_with_its_key() {
        let a = age::x25519::Identity::generate();
        let b = age::x25519::Identity::generate();
        let items = vec![SyncItem { name: "accounts.json".into(), content: "{\"token\":\"secret\"}".into() }];
        let sealed = seal(&a.to_public().to_string(), &items).unwrap();
        assert!(sealed.starts_with("-----BEGIN AGE ENCRYPTED FILE-----"));
        assert!(!sealed.contains("secret"));
        assert_eq!(open(&a, &sealed).unwrap(), items);
        assert!(open(&b, &sealed).is_err());
    }

    #[test]
    fn plugins_publish_only_their_own_bundles() {
        assert!(owns("claude-auth-manager", "claude-auth-manager/tokens"));
        assert!(!owns("claude-auth-manager", "other/tokens"));
        assert!(!owns("claude", "claude-auth-manager/tokens"));
    }
}
