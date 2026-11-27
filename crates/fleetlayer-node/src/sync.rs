//! Sync and sealed secrets (`docs/protocol.md` section 8). A `sync_source` plugin publishes a bundle through this
//! node: the node seals the items to each target node's age key and sends the copies through the control plane,
//! which relays them and keeps none. On a target, the node opens its copy and hands the items to the local plugin that
//! applies that bundle (`sync_target`), and the result flows back. Status travels, content never does.
//!
//! The source pins each target's key the first time it sees it (`~/.local/state/fleetlayer/known-keys.json`) and
//! refuses to seal to a changed key until a person removes the old pin. The target applies only versions newer than
//! the last it applied (`~/.local/state/fleetlayer/applied.json`). The node keeps the last publication of each
//! bundle in memory, so when a target that was offline comes back (`link/sync_wanted`), it seals a copy for it again.
//!
//! The source also keeps how the latest version went for each target. A target that's waiting for the person's
//! approval, whose key changed or that failed gets it again with backoff (a minute, doubling to an hour), at once when
//! the control plane says it's wanted (the approval came, the target came back) and when this node's own link comes
//! back. One refused by access control isn't retried on a timer (that would ask the person again each time a denial
//! lapses). Each change is published as a `sync.status` reading, saying what to do, and anything only a person can fix
//! that lasts over an hour fails the node's heartbeat. A target that's merely offline is the control plane's to track
//! (`sync_pending`): it isn't retried from here and doesn't fail the heartbeat.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    /// How the latest version of each bundle went for each target, by (bundle, target).
    deliveries: Mutex<HashMap<(String, String), Delivery>>,
}

/// What became of a version sent to a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Got {
    Applied,
    /// Offline: the control plane sends for it when it's back.
    Offline,
    /// Waiting for the person to approve the request with this ID.
    Approval(String),
    /// Access control refused it (a rule, or the person refused the request).
    Refused(String),
    /// The target's key changed since it was pinned.
    KeyChanged(String),
    /// Anything else (not linked, a plugin error): tried again.
    Failed(String),
}

impl Got {
    /// Read a target's result.
    fn of(ok: bool, error: Option<&str>) -> Got {
        let Some(e) = error.filter(|_| !ok) else { return Got::Applied };
        if e.contains("is offline") {
            Got::Offline
        } else if e.contains("key changed") {
            Got::KeyChanged(e.into())
        } else if e.contains("waiting for the person to approve request") {
            Got::Approval(approval_id(e).unwrap_or_default())
        } else if e.contains("may not") || e.contains("refused request") || e.contains("only with a reason") {
            Got::Refused(e.into())
        } else {
            Got::Failed(e.into())
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Got::Applied => "applied",
            Got::Offline => "offline",
            Got::Approval(_) => "waiting_for_approval",
            Got::Refused(_) => "refused",
            Got::KeyChanged(_) => "key_changed",
            Got::Failed(_) => "failed",
        }
    }

    /// Something only a person can fix, which fails the heartbeat when it lasts.
    fn needs_person(&self) -> bool {
        matches!(self, Got::Approval(_) | Got::Refused(_) | Got::KeyChanged(_))
    }

    /// Tried again on a timer. Not a refusal: retrying one would ask the person again every time it lapses (a denial
    /// stands an hour); the next version, or changing the rules and restarting the plugin, sends it again.
    fn retried(&self) -> bool {
        !matches!(self, Got::Applied | Got::Offline | Got::Refused(_))
    }

    /// The error, and what to do about it.
    fn explain(&self, bundle: &str, target: &str) -> (Option<String>, Option<String>) {
        match self {
            Got::Applied | Got::Offline => (None, None),
            Got::Approval(id) => (
                Some(format!("{bundle} for {target} is waiting for the person's approval (request {id})")),
                Some(format!("approve it with `fleetlayer-control approvals approve {id}` (or in the hub)")),
            ),
            Got::Refused(e) => {
                (Some(e.clone()), Some("change the access rules for sync, or the publishing plugin's targets".into()))
            }
            Got::KeyChanged(e) => (
                Some(e.clone()),
                Some(format!(
                    "the target's key changed: if {target} was re-enrolled, remove its pin from known-keys.json"
                )),
            ),
            Got::Failed(e) => (Some(e.clone()), None),
        }
    }
}

/// `ap_…` in an error.
fn approval_id(e: &str) -> Option<String> {
    let at = e.find("ap_")?;
    Some(e[at..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect())
}

#[derive(Debug, Clone)]
pub struct Delivery {
    pub version: u64,
    pub got: Got,
    /// When it came to this state.
    pub since: jiff::Timestamp,
    tries: u32,
    next: Option<Instant>,
}

/// How long before trying a target again after `tries` failures: a minute, doubling to an hour.
fn backoff(tries: u32) -> Duration {
    Duration::from_secs((60u64 << tries.min(6)).min(3600))
}

impl Sync {
    /// Record how `version` of `bundle` went for each target; the readings to publish for those that changed.
    async fn record(&self, bundle: &str, version: u64, results: &[(String, Got)]) -> Vec<Value> {
        let mut out = vec![];
        let mut d = self.deliveries.lock().await;
        for (target, got) in results {
            let key = (bundle.to_string(), target.clone());
            let prev = d.get(&key).cloned();
            if prev.as_ref().is_some_and(|p| p.version > version) {
                continue;
            }
            let same = prev.as_ref().is_some_and(|p| p.got == *got);
            let tries = if got.retried() { prev.as_ref().filter(|_| same).map_or(0, |p| p.tries + 1) } else { 0 };
            let since = prev.as_ref().filter(|_| same).map_or_else(jiff::Timestamp::now, |p| p.since);
            let next = got.retried().then(|| Instant::now() + backoff(tries));
            let changed = !same || prev.as_ref().is_some_and(|p| p.version != version);
            d.insert(key, Delivery { version, got: got.clone(), since, tries, next });
            match got {
                Got::Applied | Got::Offline => tracing::info!(bundle, target, version, state = got.name(), "sync"),
                _ => {
                    tracing::warn!(bundle, target, version, state = got.name(), error = ?got.explain(bundle, target).0, "sync didn't reach a target; trying again")
                }
            }
            if changed {
                let (error, action) = got.explain(bundle, target);
                out.push(
                    json!({"kind": "sync.status", "key": format!("{bundle}@{target}"), "at": jiff::Timestamp::now(),
                    "data": {"bundle": bundle, "target": target, "version": version, "ok": *got == Got::Applied,
                             "state": got.name(), "error": error, "action": action, "since": since}}),
                );
            }
        }
        out
    }

    /// Targets due for another try: (bundle, target).
    async fn due(&self, now: Instant) -> Vec<(String, String)> {
        let d = self.deliveries.lock().await;
        d.iter().filter(|(_, v)| v.next.is_some_and(|n| n <= now)).map(|(k, _)| k.clone()).collect()
    }

    /// Try every target that hasn't got its version again at once (the link came back).
    async fn all_due_now(&self) {
        let now = Instant::now();
        for v in self.deliveries.lock().await.values_mut() {
            if v.next.is_some() {
                v.next = Some(now);
            }
        }
    }

    /// Problems a person must fix that have lasted over `longer_than`, for the heartbeat.
    pub async fn problems(&self, longer_than: Duration) -> Vec<String> {
        let cutoff = jiff::Timestamp::now() - jiff::SignedDuration::try_from(longer_than).unwrap_or_default();
        let d = self.deliveries.lock().await;
        let mut out: Vec<String> = d
            .iter()
            .filter(|(_, v)| v.got.needs_person() && v.since < cutoff)
            .map(|((b, t), v)| {
                let (error, action) = v.got.explain(b, t);
                format!(
                    "sync {b} to {t}: {}{}",
                    error.unwrap_or_default(),
                    action.map(|a| format!("; {a}")).unwrap_or_default()
                )
            })
            .collect();
        out.sort();
        out
    }
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
    send_and_record(node, &p, &targets).await
}

/// Send, and record how it went for each target (a failure to send at all counts for every target).
async fn send_and_record(node: &Arc<Node>, p: &SyncPublish, targets: &[String]) -> Result<Value, RpcError> {
    let r = send(node, p, targets).await;
    let results: Vec<(String, Got)> = match &r {
        Ok(v) => v["results"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| {
                Some((x["target"].as_str()?.to_string(), Got::of(x["ok"].as_bool() == Some(true), x["error"].as_str())))
            })
            .collect(),
        Err(e) => targets.iter().map(|t| (t.clone(), Got::Failed(e.message.clone()))).collect(),
    };
    let readings = node.sync.record(&p.bundle, p.version, &results).await;
    if !readings.is_empty() {
        crate::readings::publish(node, &json!({"readings": readings})).await;
    }
    r
}

/// Send again to targets that haven't got their bundle's latest version, as each comes due. Runs for the node's life.
pub async fn retry(node: Arc<Node>) {
    loop {
        tokio::time::sleep(Duration::from_secs(20)).await;
        for (bundle, target) in node.sync.due(Instant::now()).await {
            let last = node.sync.last.lock().await.get(&bundle).cloned();
            match last {
                Some(p) if p.targets.iter().any(|t| t.node == target) => {
                    let _ = send_and_record(&node, &p, std::slice::from_ref(&target)).await;
                }
                // No longer published to it (the plugin restarted with other targets): forget it.
                _ => {
                    node.sync.deliveries.lock().await.remove(&(bundle, target));
                }
            }
        }
    }
}

/// The link to the control plane is back: try every target that's owed a version.
pub async fn relinked(node: &Node) {
    node.sync.all_due_now().await;
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
        match send_and_record(node, &p, &[target.to_string()]).await {
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
            &SyncApply { bundle: d.bundle.clone(), version: d.version, items, from: Some(d.from.clone()) },
            Duration::from_secs(60),
        )
        .await?;
    if r["applied"].as_bool() == Some(true) {
        applied.insert(d.bundle.clone(), json!(d.version));
        write_map(&applied_path, &applied).map_err(RpcError::internal)?;
    }
    // A plugin that delivers secrets lists them in the registry as it applies: redact them from now on.
    node.secrets.refresh();
    tracing::info!(bundle = %d.bundle, version = d.version, from = %d.from, applied = %r["applied"], "applied a bundle");
    Ok(r)
}

/// Remove everything delivered to this node: each `sync_target` plugin removes the files it wrote and its registry
/// entry, the registry's leftovers go, and the bundles count as never applied, so a later delivery of the same version
/// is applied again. The values stay redacted until the node restarts, since its sessions saw them. Answers how many
/// bundles the plugins forgot; an error names the plugins that couldn't (not running, too old to know the request).
pub async fn forget_all(node: &Arc<Node>) -> anyhow::Result<usize> {
    node.secrets.remember_all();
    let _guard = node.sync.lock.lock().await;
    let mut count = 0;
    let mut failed = vec![];
    let mut forgotten: Vec<String> = vec![];
    for p in &node.plugins {
        let Some(role) = p.roles().await.sync_target else { continue };
        let r = match p.peer().await {
            None => Err(format!("{} isn't running", p.name())),
            Some(peer) => peer
                .request_within::<_, Value>(ext::method::SYNC_FORGET, &json!({}), Duration::from_secs(60))
                .await
                .map_err(|e| format!("{}: {}", p.name(), e.message)),
        };
        match r {
            Ok(v) if v["forgotten"].as_bool() == Some(true) => {
                count += role.bundles.len();
                forgotten.extend(role.bundles);
            }
            Ok(v) => failed.push(format!("{}: {}", p.name(), v["error"].as_str().unwrap_or("didn't forget"))),
            Err(e) => failed.push(e),
        }
    }
    let applied_path = state_dir(node).join("applied.json");
    let mut applied = read_map(&applied_path);
    applied.retain(|b, _| !forgotten.contains(b));
    write_map(&applied_path, &applied)?;
    // Whatever a plugin left in the registry (one that failed, or a source that no longer runs).
    let dir = fleetlayer_secrets::registry::dir(&node.home);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            if let Err(err) = std::fs::remove_file(e.path()) {
                failed.push(format!("removing {}: {err}", e.path().display()));
            }
        }
    }
    node.secrets.refresh();
    tracing::warn!(bundles = count, failed = failed.len(), "forgot everything delivered to this node");
    if failed.is_empty() { Ok(count) } else { Err(anyhow::anyhow!("{}", failed.join("; "))) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_copy_opens_only_with_its_key() {
        let a = age::x25519::Identity::generate();
        let b = age::x25519::Identity::generate();
        let items = vec![SyncItem::text("accounts.json", "{\"token\":\"secret\"}")];
        let sealed = seal(&a.to_public().to_string(), &items).unwrap();
        assert!(sealed.starts_with("-----BEGIN AGE ENCRYPTED FILE-----"));
        assert!(!sealed.contains("secret"));
        assert_eq!(open(&a, &sealed).unwrap(), items);
        assert!(open(&b, &sealed).is_err());
    }

    #[test]
    fn results_are_read_into_what_to_do() {
        assert_eq!(Got::of(true, None), Got::Applied);
        assert_eq!(Got::of(false, Some("node laptop is offline; it gets this version when it's back")), Got::Offline);
        let e = "-@laptop needs its person's approval to sync sync:file-sync/skills@server: waiting for the person to \
                 approve request ap_01JABC. Try again once they've approved it";
        assert_eq!(Got::of(false, Some(e)), Got::Approval("ap_01JABC".into()));
        let (_, action) = Got::of(false, Some(e)).explain("file-sync/skills", "server");
        assert!(action.unwrap().contains("fleetlayer-control approvals approve ap_01JABC"));
        let refused = Got::of(false, Some("-@a may not sync sync:x/y@b (access control: …)"));
        assert!(matches!(refused, Got::Refused(_)) && !refused.retried() && refused.needs_person());
        assert!(matches!(Got::of(false, Some("node b's key changed (k1 → k2); remove …")), Got::KeyChanged(_)));
        assert!(matches!(Got::of(false, Some("plugin files isn't running")), Got::Failed(_)));
    }

    #[tokio::test]
    async fn a_refused_target_is_retried_with_backoff_reported_once_and_cleared_when_it_applies() {
        let s = Sync::default();
        let wait = Got::Approval("ap_1".into());
        let r = s.record("file-sync/skills", 1, &[("server".into(), wait.clone()), ("pi".into(), Got::Applied)]).await;
        assert_eq!(r.len(), 2, "each target's first state is a reading");
        assert_eq!(r[0]["data"]["state"], "waiting_for_approval");
        assert!(r[0]["data"]["action"].as_str().unwrap().contains("approve ap_1"));
        assert!(s.due(Instant::now()).await.is_empty(), "not before its backoff");
        assert_eq!(s.due(Instant::now() + backoff(0)).await, [("file-sync/skills".to_string(), "server".to_string())]);
        // The same refusal again: no new reading, a longer wait.
        assert!(s.record("file-sync/skills", 1, &[("server".into(), wait.clone())]).await.is_empty());
        assert!(s.due(Instant::now() + backoff(0)).await.is_empty());
        assert!(backoff(1) > backoff(0) && backoff(30) == Duration::from_secs(3600));
        // The link comes back: due at once.
        s.all_due_now().await;
        assert_eq!(s.due(Instant::now()).await.len(), 1);
        // Not yet over an hour: no heartbeat problem; then there is one.
        assert!(s.problems(Duration::from_secs(3600)).await.is_empty());
        assert_eq!(s.problems(Duration::ZERO).await.len(), 1);
        // Applied: a reading, nothing due, no problem.
        let r = s.record("file-sync/skills", 1, &[("server".into(), Got::Applied)]).await;
        assert_eq!(r[0]["data"]["ok"], true);
        assert!(s.due(Instant::now() + Duration::from_secs(7200)).await.is_empty());
        assert!(s.problems(Duration::ZERO).await.is_empty());
        // An offline target is the control plane's to track: never retried here.
        s.record("file-sync/skills", 2, &[("pi".into(), Got::Offline)]).await;
        assert!(s.due(Instant::now() + Duration::from_secs(7200)).await.is_empty());
        assert!(s.problems(Duration::ZERO).await.is_empty());
    }

    #[test]
    fn plugins_publish_only_their_own_bundles() {
        assert!(owns("claude-auth-manager", "claude-auth-manager/tokens"));
        assert!(!owns("claude-auth-manager", "other/tokens"));
        assert!(!owns("claude", "claude-auth-manager/tokens"));
    }
}
