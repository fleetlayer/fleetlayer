//! The Claude account rotator's coordinator, serving its API on `coordinator.sock` so the rotator plugin works with
//! the accounts this plugin manages. Moved from Quietdeck's usage module (`crates/quietdeck-node/src/usage`, as of
//! quietdeck 2c13f25), which ported it from the rotator's TypeScript coordinator; its API is the rotator's
//! `docs/coordinator-api.md` at [`API_VERSION`].
//!
//! On a node the slots hold setup tokens, which can't read usage or profiles, so the coordinator runs in **borrow
//! mode** (`Coordinator::borrowing`): nothing is polled, refreshed or signed in. Its accounts are the slots this plugin
//! manages (`slots::Layout::managed`), and its readings come from two places:
//!
//! - what sessions here report (`POST /readings`), which it publishes as `claude.usage` readings keyed by account UUID,
//!   so every other node and the hub see them;
//! - `claude.usage` readings from elsewhere (the hub's polls, with the per-model weeks, and other nodes' sessions),
//!   which it subscribes to and keeps as any reading: the newest whole reading wins.
//!
//! `data` is the coordinator's own reading shape, `{fiveHour?, sevenDay?, models?, at}` (each window
//! `{pct, resetsAt?}`, `at` in epoch ms), plus `source` (`session` or `poll`). The hub publishes its polls the same way.
//!
//! One coordinator runs per user, held by `coordinator.lock` in the rotator's directory. While a rotator's own
//! TypeScript coordinator holds it, this one asks it to shut down and takes the lock; while Quietdeck's node holds it,
//! this one waits (that node's Claude part has to be turned off where this plugin runs), and says so.

pub mod core;
pub mod deps;
pub mod model;
pub mod server;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fleetlayer_plugin::Peer;
use fleetlayer_protocol::ext;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::watch;

use self::core::{Coordinator, Info};
use self::deps::{Logger, RealDeps};
use self::model::{Account, Ms, Reading};
use crate::slots::{Layout, Managed};

/// What GET /hello answers as `name`: the rotator leaves a coordinator of a name it knows alone, whatever its version.
pub const NAME: &str = "fleetlayer-claude-auth-manager";
/// The coordinator API served: the rotator release whose `docs/coordinator-api.md` it follows.
pub const API_VERSION: &str = "0.7.1";
/// Quietdeck's node, which serves the same API.
const QUIETDECK: &str = "quietdeck-node";
/// The reading kind for an account's usage.
pub const USAGE_KIND: &str = "claude.usage";

/// Where an account's newest reading came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadingSource {
    /// Polled from the usage endpoint.
    Poll,
    /// Reported by a session here, from a response's rate-limit headers.
    Session,
    /// A reading from another node or the hub (borrow mode).
    Borrowed,
}

/// The coordinator a node runs.
pub type NodeCoordinator = Coordinator<RealDeps>;

/// The running coordinator, once it has the lock: what bundle applies, token checks and reading updates reach.
pub type Slot = Arc<Mutex<Option<NodeCoordinator>>>;

pub fn current(slot: &Slot) -> Option<NodeCoordinator> {
    slot.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The rotator's directory: `CLAUDE_ACCOUNT_ROTATOR_DIR`, else `~/.claude/account-rotator`.
pub fn rotator_dir(home: &Path) -> PathBuf {
    std::env::var_os("CLAUDE_ACCOUNT_ROTATOR_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude").join("account-rotator"))
}

/// The accounts the hub shares, as the coordinator lists them: the rotator's ID is the slot's number.
pub fn shared_accounts(managed: &[Managed]) -> Vec<Account> {
    managed
        .iter()
        .map(|m| Account {
            id: m.number.to_string(),
            slot: m.slot.clone(),
            account_uuid: (!m.account_uuid.is_empty()).then(|| m.account_uuid.clone()),
            email: (!m.email.is_empty()).then(|| m.email.clone()),
            ..Default::default()
        })
        .collect()
}

/// A session's reading of an account, as a `claude.usage` reading. None for an account with no UUID (a slot this
/// plugin doesn't manage), whose figures mean nothing elsewhere.
pub fn usage_reading(account: &Account, reading: &Reading) -> Option<ext::Reading> {
    let key = account.account_uuid.clone()?;
    let mut data = serde_json::to_value(reading).ok()?;
    data["source"] = json!("session");
    Some(ext::Reading {
        kind: USAGE_KIND.into(),
        key,
        at: jiff::Timestamp::from_millisecond(reading.at).ok()?,
        node: None,
        data,
    })
}

/// Takes `claude.usage` readings from elsewhere into the coordinator, skipping this node's own. Answers how many
/// were for an account here.
pub fn take_readings<D: core::Deps>(c: &Coordinator<D>, own_node: &str, readings: &[ext::Reading]) -> usize {
    readings
        .iter()
        .filter(|r| r.kind == USAGE_KIND && r.node.as_deref() != Some(own_node))
        .filter_map(|r| Some((r, serde_json::from_value::<Reading>(r.data.clone()).ok()?)))
        .filter(|(r, reading)| c.borrow_reading(&r.key, reading.clone(), ReadingSource::Borrowed))
        .count()
}

fn persisted(file: &Path, logger: &Logger) -> Option<Value> {
    let text = std::fs::read_to_string(file).ok()?;
    match serde_json::from_str(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            logger.log(&format!("could not read {}: {e}; starting afresh", file.display()));
            None
        }
    }
}

/// What the node coordinator needs.
pub struct NodeEnv {
    pub rotator_dir: PathBuf,
    pub home: PathBuf,
    pub claude: PathBuf,
    /// This node's name, to skip its own readings.
    pub node: String,
}

/// Runs the node's coordinator for as long as the plugin runs: takes the lock (asking a rotator's own coordinator to
/// step down, waiting while Quietdeck's node holds it), then serves the API in borrow mode.
pub async fn run_node(peer: Peer, env: NodeEnv, layout: Layout, applied: watch::Receiver<u64>, slot: Slot) {
    let _ = std::fs::create_dir_all(&env.rotator_dir);
    let lock = server::lock_path(&env.rotator_dir);
    let sock = server::socket_path(&env.rotator_dir);
    let mut said: Option<i32> = None;
    while !peer.is_closed() {
        let wait = match server::take_lock(&lock) {
            Ok(None) => {
                serve(&peer, &env, &layout, applied.clone(), &slot).await;
                let _ = std::fs::remove_file(&lock);
                Duration::from_secs(1)
            }
            Ok(Some(pid)) => {
                let name = match server::call(&sock, "GET", "/hello", None, Duration::from_secs(5)).await {
                    Ok((200, hello)) => hello["name"].as_str().map(str::to_string),
                    _ => None,
                };
                match name.as_deref() {
                    Some(QUIETDECK) | Some(NAME) => {
                        if said != Some(pid) {
                            let who = name.as_deref().unwrap_or_default();
                            tracing::warn!(
                                pid,
                                "{who} serves the coordinator API here, so this plugin can't; turn its Claude part off"
                            );
                            publish_status(&peer, false, &format!("{who} (pid {pid}) holds coordinator.lock"));
                            said = Some(pid);
                        }
                        Duration::from_secs(30)
                    }
                    _ => {
                        // A rotator's own coordinator (or one still starting): its sessions ask for one again within
                        // 2 s and would start another, so take the lock right after it stops.
                        if said != Some(pid) {
                            tracing::info!(pid, "asking the rotator's own coordinator to stop, to serve the API here");
                            said = Some(pid);
                        }
                        let _ = server::call(&sock, "POST", "/shutdown", None, Duration::from_secs(5)).await;
                        Duration::from_millis(250)
                    }
                }
            }
            Err(e) => {
                tracing::warn!("taking {}: {e}", lock.display());
                Duration::from_secs(5)
            }
        };
        tokio::time::sleep(wait).await;
    }
}

/// A `claude.coordinator` reading: whether this node serves the coordinator API, and why not.
fn publish_status(peer: &Peer, serving: bool, why: &str) {
    let mut data = json!({ "serving": serving, "apiVersion": API_VERSION });
    if !why.is_empty() {
        data["why"] = json!(why);
    }
    let reading = ext::Reading {
        kind: "claude.coordinator".into(),
        key: "coordinator".into(),
        at: jiff::Timestamp::now(),
        node: None,
        data,
    };
    peer.notify(ext::method::READING_PUBLISH, &ext::Readings { readings: vec![reading] });
}

/// Serves the API in borrow mode, holding the lock, until the plugin stops.
async fn serve(peer: &Peer, env: &NodeEnv, layout: &Layout, mut applied: watch::Receiver<u64>, slot: &Slot) {
    let dir = &env.rotator_dir;
    let logger = Logger::new(server::log_path(dir));
    let deps = RealDeps::new(env.home.clone(), env.claude.clone(), server::state_path(dir), logger.clone());
    let info = Info {
        version: API_VERSION.into(),
        pid: std::process::id(),
        started_at: jiff::Timestamp::now().as_millisecond(),
        is_managed: true,
    };
    let c = Coordinator::borrowing(deps, info, persisted(&server::state_path(dir), &logger).as_ref());
    c.set_shared_accounts(shared_accounts(&layout.managed()));
    let listener = match server::bind(dir) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("could not serve {}: {e}", server::socket_path(dir).display());
            publish_status(peer, false, &format!("could not open coordinator.sock: {e}"));
            return;
        }
    };
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(c.clone());
    c.start().await;
    logger.log(&format!(
        "{NAME} serving the coordinator API {API_VERSION} in borrow mode on {} (pid {})",
        server::socket_path(dir).display(),
        std::process::id()
    ));
    tracing::info!("serving the coordinator API {API_VERSION} on {}", server::socket_path(dir).display());
    publish_status(peer, true, "");

    let mut tasks = vec![];
    // The accounts, again after each bundle is applied.
    tasks.push(tokio::spawn({
        let (c, layout) = (c.clone(), layout.clone());
        async move {
            while applied.changed().await.is_ok() {
                c.set_shared_accounts(shared_accounts(&layout.managed()));
            }
        }
    }));
    // Readings from elsewhere: the latest of each now, then updates (`main.rs` passes those on).
    tasks.push(tokio::spawn({
        let (c, peer, node) = (c.clone(), peer.clone(), env.node.clone());
        async move {
            loop {
                let asked = peer
                    .request_within::<_, ext::Readings>(
                        ext::method::READING_SUBSCRIBE,
                        &json!({ "kinds": [USAGE_KIND] }),
                        Duration::from_secs(30),
                    )
                    .await;
                match asked {
                    Ok(latest) => {
                        let n = take_readings(&c, &node, &latest.readings);
                        tracing::info!(readings = latest.readings.len(), taken = n, "subscribed to claude.usage");
                        return;
                    }
                    Err(e) => {
                        tracing::warn!("subscribing to claude.usage: {e}; trying again in a minute");
                        tokio::time::sleep(Duration::from_secs(60)).await;
                    }
                }
            }
        }
    }));
    // What sessions here report, published for the rest of the fleet.
    tasks.push(tokio::spawn({
        let (c, peer) = (c.clone(), peer.clone());
        async move {
            let mut changes = c.changes();
            let mut sent: HashMap<String, Ms> = HashMap::new();
            loop {
                let readings: Vec<ext::Reading> = c
                    .session_readings(&sent)
                    .into_iter()
                    .filter_map(|(a, r)| {
                        sent.insert(a.id.clone(), r.at);
                        usage_reading(&a, &r)
                    })
                    .collect();
                if !readings.is_empty() {
                    peer.notify(ext::method::READING_PUBLISH, &ext::Readings { readings });
                }
                if changes.changed().await.is_err() {
                    return;
                }
                // A burst of reports goes out as one.
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }));

    let closed = {
        let peer = peer.clone();
        async move {
            while !peer.is_closed() {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    };
    if let Err(e) = server::serve(listener, c.clone(), closed).await {
        logger.log(&format!("the coordinator API stopped: {e}"));
    }
    for t in tasks {
        t.abort();
    }
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
    c.stop();
    c.deps().flush();
    let _ = std::fs::remove_file(server::socket_path(dir));
}
