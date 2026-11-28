//! fleetlayer-matrix: Matrix rooms as Fleetlayer channels (`docs/plugins/matrix.md`).
//!
//! Run with no arguments, it's a node plugin (an event source): it signs in as a bot, reads the rooms its config
//! names, and publishes each new message as an event on the room's channel, so the sessions subscribed to it hear it
//! as outside data. `fleetlayer-matrix mcp` is the MCP server a node shares as a tool, through which agents post to the
//! rooms and read their latest messages. `fleetlayer-matrix check` signs in and says what the bot can reach.

mod config;
mod mcp;
mod outbox;
mod reader;
mod render;
mod socket;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use clap::{Parser, Subcommand};
use fleetlayer_plugin::{Handler, Peer, answer, params};
use fleetlayer_protocol::acp::{self, Capabilities, Implementation, InitializeRequest, InitializeResponse};
use fleetlayer_protocol::ext::{self, Event, NodeMeta, PluginMeta, PluginStatusReport, Roles, meta_key};
use fleetlayer_protocol::rpc::{RpcError, code};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::config::Config;
use crate::reader::Shared;

/// How long the node gets to pass an event to the control plane.
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(30);
const STATUS_EVERY: Duration = Duration::from_secs(30);

#[derive(Parser, Debug)]
#[command(name = "fleetlayer-matrix", version, about = "Matrix rooms as Fleetlayer channels")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Serve MCP on stdio for one agent: post to the rooms, read their latest messages (a node's `tools:` entry).
    Mcp {
        /// The plugin's state directory (default ~/.local/state/fleetlayer/matrix).
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Sign in (or reuse the device), then say which rooms the bot reaches. Stop the plugin first.
    Check {
        /// The node's config (default $FLEETLAYER_CONFIG or ~/.config/fleetlayer/node.yaml).
        #[arg(long)]
        node_config: Option<PathBuf>,
        /// The plugin entry's name there (default: the one whose command is fleetlayer-matrix).
        #[arg(long)]
        plugin: Option<String>,
    },
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

#[derive(Default)]
struct Plugin {
    started: Mutex<bool>,
}

struct Shim(Arc<Plugin>);

/// Hand the outbox to the node, oldest first, until the plugin stops. An event leaves the outbox only once the node
/// answers that it was published (or was a repeat), so a node or control plane that's away loses nothing.
async fn publish_loop<F, Fut>(shared: Arc<Shared>, publish: F)
where
    F: Fn(Event) -> Fut,
    Fut: std::future::Future<Output = Result<Value, RpcError>>,
{
    let mut backoff = Duration::from_secs(2);
    loop {
        let next = shared.outbox.lock().await.first().map(|w| w.event.clone());
        let Some(event) = next else {
            backoff = Duration::from_secs(2);
            // A missed notification costs at most this long.
            let _ = tokio::time::timeout(Duration::from_secs(30), shared.ready.notified()).await;
            continue;
        };
        match publish(event.clone()).await {
            Ok(r) => {
                let duplicate = r["duplicate"].as_bool() == Some(true);
                tracing::info!(channel = %event.channel, key = ?event.dedupe_key, duplicate, "published");
                shared.outbox.lock().await.pop();
                let mut h = shared.health.lock().await;
                h.published += 1;
                h.publish_error = None;
                backoff = Duration::from_secs(2);
            }
            // The node found the event malformed: it never will be accepted, so it mustn't hold up the rest.
            Err(e) if e.code == code::INVALID_PARAMS => {
                tracing::error!(error = %e.message, key = ?event.dedupe_key, "the node refused an event as malformed; dropping it");
                shared.outbox.lock().await.pop();
            }
            Err(e) => {
                tracing::info!(error = %e.message, "the event wasn't published yet");
                shared.health.lock().await.publish_error = Some(e.message);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

async fn status_loop(shared: Arc<Shared>, peer: Peer) {
    while !peer.is_closed() {
        let (problem, detail) = shared.status().await;
        peer.notify(ext::method::PLUGIN_STATUS, &PluginStatusReport { problem, detail });
        tokio::time::sleep(STATUS_EVERY).await;
    }
}

fn start(cfg: Config, node: &NodeMeta, peer: Peer) -> Result<(), RpcError> {
    let rooms = cfg.check().map_err(|e| RpcError::invalid_params(format!("the matrix plugin's config: {e}")))?;
    let home = PathBuf::from(&node.home);
    let dir = cfg.state_dir(&home);
    let reply_with = cfg.tool.as_ref().map(|t| format!("{t}@{}", node.node));
    let shared = Arc::new(Shared::new(cfg, rooms, node.node.clone(), dir, reply_with));
    tokio::spawn(reader::run(shared.clone()));
    {
        let shared = shared.clone();
        tokio::spawn(async move {
            // The reader makes the directory; give it a moment.
            tokio::time::sleep(Duration::from_millis(500)).await;
            loop {
                if let Err(e) = socket::serve(shared.clone()).await {
                    tracing::error!(error = %e, "the post socket stopped: starting it again in 30s");
                }
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
    }
    {
        let (shared, peer) = (shared.clone(), peer.clone());
        tokio::spawn(publish_loop(shared, move |e| {
            let peer = peer.clone();
            async move { peer.request_within::<_, Value>(ext::method::EVENT_PUBLISH, &e, PUBLISH_TIMEOUT).await }
        }));
    }
    tokio::spawn(status_loop(shared, peer));
    Ok(())
}

#[async_trait]
impl Handler for Shim {
    async fn request(&self, peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
        match method {
            acp::method::INITIALIZE => {
                let req: InitializeRequest = params(p)?;
                let node: NodeMeta = ext::get_meta(req.client_capabilities.meta.as_ref(), meta_key::NODE)
                    .map_err(RpcError::invalid_params)?
                    .ok_or_else(|| RpcError::refused("fleetlayer-matrix runs under a Fleetlayer node"))?;
                let cfg: Config = serde_json::from_value(node.config.clone())
                    .map_err(|e| RpcError::invalid_params(format!("the matrix plugin's config: {e}")))?;
                let mut started = self.0.started.lock().await;
                if !*started {
                    start(cfg, &node, peer.clone())?;
                    *started = true;
                }
                let roles = Roles { event_source: Some(json!({"source": "matrix"})), ..Default::default() };
                let meta = PluginMeta { protocol: fleetlayer_protocol::PROTOCOL, roles };
                answer(&InitializeResponse {
                    protocol_version: fleetlayer_protocol::ACP_PROTOCOL,
                    agent_capabilities: Capabilities {
                        meta: Some(ext::meta_with(meta_key::PLUGIN, &meta)),
                        ..Default::default()
                    },
                    agent_info: Some(Implementation {
                        name: "fleetlayer-matrix".into(),
                        title: Some("Matrix".into()),
                        version: env!("CARGO_PKG_VERSION").into(),
                    }),
                    auth_methods: vec![],
                })
            }
            ext::method::PING => Ok(json!({})),
            _ => Err(RpcError::method_not_found(method)),
        }
    }
}

/// The plugin's config from the node's config file.
fn node_plugin_config(path: &Path, name: Option<&str>) -> anyhow::Result<Config> {
    let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    let v: Value = yaml_serde::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    let entry = v["plugins"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| match name {
            Some(n) => p["name"] == n,
            None => p["command"].as_str().is_some_and(|c| c.ends_with("fleetlayer-matrix")),
        })
        .ok_or_else(|| anyhow::anyhow!("{} has no fleetlayer-matrix plugin entry", path.display()))?;
    Ok(serde_json::from_value(entry["config"].clone())?)
}

/// The log filter when `RUST_LOG` isn't set. matrix-sdk tries to upload room keys to a key backup after every sync
/// whether or not the device has a backup key, and warns each time it has none (every 30 seconds): the plugin never
/// sets up key backup, so that warning is silenced here and `reader::sign_in` says it once instead.
pub const LOG_FILTER: &str = "info,matrix_sdk_crypto::backups=error";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| LOG_FILTER.into()))
        .init();
    let home = home();
    match args.command {
        None => {
            fleetlayer_plugin::serve_stdio(Arc::new(Shim(Arc::new(Plugin::default())))).await;
            Ok(())
        }
        Some(Command::Mcp { state_dir }) => {
            let dir = state_dir.unwrap_or_else(|| config::default_state_dir(&home));
            mcp::serve(&dir).await
        }
        Some(Command::Check { node_config, plugin }) => {
            let path = node_config
                .or_else(|| std::env::var_os("FLEETLAYER_CONFIG").map(PathBuf::from))
                .unwrap_or_else(|| home.join(".config/fleetlayer/node.yaml"));
            let cfg = node_plugin_config(&path, plugin.as_deref())?;
            let dir = cfg.state_dir(&home);
            reader::check(&cfg, "check", &dir).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        serde_json::from_value(json!({
            "homeserver": "https://matrix.example.com",
            "user": "@fleetlayer:example.com",
            "password_file": "/p",
            "rooms": [{"room": "#alerts:example.com"}]
        }))
        .unwrap()
    }

    fn ev(id: &str) -> Event {
        Event {
            channel: "alerts".into(),
            kind: "matrix.message".into(),
            subject: None,
            at: jiff::Timestamp::now(),
            dedupe_key: Some(format!("matrix:{id}")),
            summary: "x".into(),
            body: Value::Null,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn events_leave_the_outbox_only_once_published_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let rooms = cfg().check().unwrap();
        let shared = Arc::new(Shared::new(cfg(), rooms, "server".into(), dir.path().into(), None));
        shared.outbox.lock().await.push(vec![ev("$1"), ev("$2"), ev("$3")]);
        let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let c = calls.clone();
        let task = tokio::spawn(publish_loop(shared.clone(), move |e| {
            let c = c.clone();
            async move {
                let key = e.dedupe_key.clone().unwrap();
                let n = {
                    let mut c = c.lock().unwrap();
                    c.push(key.clone());
                    c.len()
                };
                match (n, key.as_str()) {
                    (1, _) => Err(RpcError::not_here("server isn't linked to a control plane now")),
                    (_, "matrix:$2") => Err(RpcError::invalid_params("bad")),
                    _ => Ok(json!({"duplicate": false})),
                }
            }
        }));
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if shared.outbox.lock().await.len() == 0 {
                break;
            }
        }
        task.abort();
        assert_eq!(
            *calls.lock().unwrap(),
            ["matrix:$1", "matrix:$1", "matrix:$2", "matrix:$3"],
            "kept and tried again while the node was away; a malformed one doesn't hold up the rest"
        );
        assert_eq!(shared.outbox.lock().await.len(), 0);
        assert_eq!(shared.health.lock().await.published, 2);
        assert_eq!(outbox::Outbox::load(dir.path()).len(), 0, "and the file agrees");
    }

    #[tokio::test]
    async fn a_new_plugin_reports_no_problem_until_the_sync_is_late() {
        let dir = tempfile::tempdir().unwrap();
        let rooms = cfg().check().unwrap();
        let shared = Shared::new(cfg(), rooms, "server".into(), dir.path().into(), None);
        let (problem, detail) = shared.status().await;
        assert_eq!(problem, None, "it's only starting");
        assert_eq!(detail["waiting"], 0);
        shared.health.lock().await.started = Some(std::time::Instant::now() - Duration::from_secs(600));
        shared.health.lock().await.sync_error = Some("connection refused".into());
        let (problem, _) = shared.status().await;
        assert_eq!(problem.as_deref(), Some("Matrix hasn't synced since it started: connection refused"));
    }

    #[tokio::test]
    async fn a_room_the_bot_isnt_in_is_a_problem() {
        let dir = tempfile::tempdir().unwrap();
        let rooms = cfg().check().unwrap();
        let shared = Shared::new(cfg(), rooms, "server".into(), dir.path().into(), None);
        {
            let mut h = shared.health.lock().await;
            h.signed_in = true;
            h.last_sync = Some(jiff::Timestamp::now());
            h.last_sync_at = Some(std::time::Instant::now());
            h.rooms.insert("#alerts:example.com".into(), "left".into());
        }
        let (problem, _) = shared.status().await;
        assert!(problem.unwrap().contains("@fleetlayer:example.com isn't in #alerts:example.com (left)"));
    }

    #[test]
    fn the_config_is_checked_all_at_once() {
        let bad: Config = serde_json::from_value(json!({
            "homeserver": "matrix.example.com",
            "user": "fleetlayer",
            "password_file": "/p",
            "rooms": [{"room": "!abc:example.com"}, {"room": "alerts"}, {"room": "#a:x", "routes": [{"match": "(", "subject": "s"}]}],
            "heartbeats": [{"room": "#a:x", "ping_url_file": "/f"}]
        }))
        .unwrap();
        let e = bad.check().unwrap_err();
        for part in ["homeserver", "user must be", "needs a channel", "isn't a room ID", "route 1", "also in rooms"] {
            assert!(e.contains(part), "{part} in {e}");
        }
        let two: Config = serde_json::from_value(json!({
            "homeserver": "https://m", "user": "@b:m", "password_file": "/p",
            "rooms": [{"room": "#alerts:m"}, {"room": "#alerts:other"}]
        }))
        .unwrap();
        assert!(two.check().unwrap_err().contains("two rooms publish on channel alerts"));
    }

    #[test]
    fn check_finds_its_entry_in_node_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("node.yaml");
        std::fs::write(
            &p,
            "plugins:\n  - name: herdr\n    command: fleetlayer-herdr\n  - name: matrix\n    command: /usr/local/bin/fleetlayer-matrix\n    config:\n      homeserver: https://m\n      user: '@b:m'\n      password_file: ~/pw\n      rooms: [{room: '#alerts:m'}]\n",
        )
        .unwrap();
        let cfg = node_plugin_config(&p, None).unwrap();
        assert_eq!(cfg.rooms[0].room, "#alerts:m");
        assert!(node_plugin_config(&p, Some("nope")).is_err());
    }

    #[test]
    fn the_default_log_filter_silences_only_the_key_backup_warning() {
        let f = tracing_subscriber::EnvFilter::try_new(LOG_FILTER).expect("the default filter parses");
        let shown = f.to_string();
        assert!(shown.contains("matrix_sdk_crypto::backups=error"), "{shown}");
        assert!(shown.starts_with("info") || shown.contains(",info") || shown.contains("info,"), "{shown}");
    }
}
