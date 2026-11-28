//! fleetlayer-presence: whether the person is at their desk, on their phone, away or asleep, so agents skip
//! background work nobody will read and know when to expect an answer (`docs/plugins/presence.md`).
//!
//! Two parts, one program:
//! - **The authority** (run with no arguments): one per fleet, on an always-on node. It keeps what people said and
//!   planned in its state directory, hears their desks, and decides each person's state (`engine.rs`). Agents reach it
//!   through a tool its node shares, `fleetlayer-presence serve --caller {caller}`, so every call goes through the
//!   control plane's access policy and audit log; `fleetlayer presence` is the command line for it. Its config
//!   section in `node.yaml` (all optional): `settings` (default `~/.config/fleetlayer/presence.yaml`) and `state_dir`
//!   (default `~/.local/state/fleetlayer/presence`). Grants: `reading.subscribe` (desk reports), `reading.publish`
//!   (the `presence` reading) and `event.publish` (changes on the `presence` channel).
//! - **The desk source** (`args: [desk]`): on the person's own computer, reporting screen lock and idle time as a
//!   `presence.desk` reading (`desk.rs`). Config: `person` (required) and `command` (for systems other than macOS).
//!   Grant: `reading.publish`.
//!
//! Commands on the authority's machine: `show [--person <p>] [--json]` and `check [file]`.

mod authority;
mod desk;
mod engine;
mod serve;
mod settings;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use clap::{Parser, Subcommand};
use fleetlayer_plugin::{Handler, Peer, answer, params};
use fleetlayer_protocol::acp::{self, Capabilities, Implementation, InitializeRequest, InitializeResponse};
use fleetlayer_protocol::ext::{self, Event, NodeMeta, PluginMeta, PluginStatusReport, Reading, Roles, meta_key};
use fleetlayer_protocol::rpc::RpcError;
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::authority::{Authority, Paths};

/// The reading each person's state is published as, keyed by the person.
pub const KIND: &str = "presence";
/// The reading a desk source publishes, keyed by the person.
pub const DESK_KIND: &str = "presence.desk";
/// The channel changes are announced on.
pub const CHANNEL: &str = "presence";

#[derive(Parser, Debug)]
#[command(
    name = "fleetlayer-presence",
    version,
    about = "Whether the person is here, so agents skip work nobody reads"
)]
struct Args {
    /// The authority's state directory (default ~/.local/state/fleetlayer/presence).
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// The desk source plugin, on the person's own computer: `args: [desk]` in node.yaml.
    Desk,
    /// The MCP server the authority's node shares as a tool: `args: [serve, --caller, "{caller}"]`.
    Serve {
        /// `<tab>@<node>` of the caller, as the control plane recorded it (`-@<node>` for a script).
        #[arg(long, allow_hyphen_values = true)]
        caller: String,
    },
    /// Every person's state, from the authority running on this machine.
    Show {
        #[arg(long)]
        person: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Read a settings file (default ~/.config/fleetlayer/presence.yaml) and print who's set up.
    Check { file: Option<PathBuf> },
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorityConfig {
    settings: Option<PathBuf>,
    state_dir: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeskSection {
    person: String,
    command: Option<String>,
}

fn expand(home: &Path, p: PathBuf) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => p,
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn default_settings(home: &Path) -> PathBuf {
    home.join(".config/fleetlayer/presence.yaml")
}

fn default_state_dir(home: &Path) -> PathBuf {
    home.join(".local/state/fleetlayer/presence")
}

const NODE_TIMEOUT: Duration = Duration::from_secs(20);

struct ToNode(Peer);

#[async_trait]
impl authority::Node for ToNode {
    async fn publish(&self, event: Event) -> Result<(), RpcError> {
        self.0.request_within::<_, Value>(ext::method::EVENT_PUBLISH, &event, NODE_TIMEOUT).await.map(|_| ())
    }

    fn readings(&self, readings: Vec<Reading>) {
        if !readings.is_empty() {
            self.0.notify(ext::method::READING_PUBLISH, &json!({"readings": readings}));
        }
    }

    fn status(&self, problem: Option<String>, detail: Value) {
        self.0.notify(ext::method::PLUGIN_STATUS, &PluginStatusReport { problem, detail });
    }
}

fn initialized(name: &str, title: &str) -> Result<Value, RpcError> {
    let meta = PluginMeta { protocol: fleetlayer_protocol::PROTOCOL, roles: Roles::default() };
    answer(&InitializeResponse {
        protocol_version: fleetlayer_protocol::ACP_PROTOCOL,
        agent_capabilities: Capabilities { meta: Some(ext::meta_with(meta_key::PLUGIN, &meta)), ..Default::default() },
        agent_info: Some(Implementation {
            name: name.into(),
            title: Some(title.into()),
            version: env!("CARGO_PKG_VERSION").into(),
        }),
        auth_methods: vec![],
    })
}

fn node_meta(p: Value) -> Result<NodeMeta, RpcError> {
    let req: InitializeRequest = params(p)?;
    ext::get_meta(req.client_capabilities.meta.as_ref(), meta_key::NODE)
        .map_err(RpcError::invalid_params)?
        .ok_or_else(|| RpcError::refused("fleetlayer-presence runs under a Fleetlayer node"))
}

/// The authority plugin.
#[derive(Default)]
struct AuthorityPlugin {
    authority: Mutex<Option<Arc<Authority>>>,
}

struct AuthorityHandler(Arc<AuthorityPlugin>);

#[async_trait]
impl Handler for AuthorityHandler {
    async fn request(&self, peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
        match method {
            acp::method::INITIALIZE => {
                let node = node_meta(p)?;
                let home = PathBuf::from(&node.home);
                let config: AuthorityConfig = match &node.config {
                    Value::Null => AuthorityConfig::default(),
                    c => serde_json::from_value(c.clone()).map_err(RpcError::invalid_params)?,
                };
                let paths = Paths {
                    settings: config.settings.map_or_else(|| default_settings(&home), |p| expand(&home, p)),
                    state_dir: config.state_dir.map_or_else(|| default_state_dir(&home), |p| expand(&home, p)),
                };
                let mut slot = self.0.authority.lock().await;
                if slot.is_none() {
                    let a =
                        Authority::open(paths.clone(), Arc::new(ToNode(peer.clone()))).map_err(RpcError::internal)?;
                    let listener = bind(&paths.socket()).map_err(RpcError::internal)?;
                    *slot = Some(a.clone());
                    start(a, listener, peer.clone());
                }
                initialized("fleetlayer-presence", "Presence")
            }
            ext::method::PING => Ok(json!({})),
            _ => Err(RpcError::method_not_found(method)),
        }
    }

    async fn notification(&self, _peer: &Peer, method: &str, p: Value) {
        if method != ext::method::READING_UPDATE {
            return;
        }
        let Some(a) = self.0.authority.lock().await.clone() else { return };
        if let Ok(r) = serde_json::from_value::<ext::Readings>(p) {
            a.desk_readings(&r.readings).await;
        }
    }
}

/// The socket the tool's server and the admin commands use: only this Unix user may connect.
fn bind(path: &Path) -> anyhow::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        anyhow::bail!(
            "{} is in use: is another fleetlayer-presence running with this state directory?",
            path.display()
        );
    }
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

fn start(a: Arc<Authority>, listener: tokio::net::UnixListener, peer: Peer) {
    tokio::spawn(a.clone().serve(listener));
    let (a2, p2) = (a.clone(), peer.clone());
    tokio::spawn(async move {
        // Once the node has had our answer: hear the desks, then decide every few seconds.
        tokio::time::sleep(Duration::from_millis(200)).await;
        subscribe(&a2, &p2).await;
        while !p2.is_closed() {
            a2.round().await;
            tokio::time::sleep(authority::ROUND).await;
        }
    });
    // A round that never finishes is reported from here.
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            if peer.is_closed() {
                return;
            }
            if let Some(t) = a.last_round().await
                && Timestamp::now().duration_since(t).as_secs() > 180
            {
                let problem = format!("its last round finished at {t}: it may be stuck");
                peer.notify(
                    ext::method::PLUGIN_STATUS,
                    &PluginStatusReport { problem: Some(problem), detail: json!({}) },
                );
            }
        }
    });
}

/// Ask for desk reports from every node (the node renews it after each reconnect, and passes the latest at once).
async fn subscribe(a: &Authority, peer: &Peer) {
    match peer.request::<_, ext::Readings>(ext::method::READING_SUBSCRIBE, &json!({"kinds": [DESK_KIND]})).await {
        Ok(r) => {
            a.set_subscribe_problem(None).await;
            a.desk_readings(&r.readings).await;
        }
        Err(e) => {
            tracing::warn!("subscribing to desk reports: {}", e.message);
            a.set_subscribe_problem(Some(format!("can't hear desk reports: {} (grant reading.subscribe)", e.message)))
                .await;
        }
    }
}

/// The desk source plugin.
#[derive(Default)]
struct DeskPlugin {
    started: Mutex<bool>,
}

struct DeskHandler(Arc<DeskPlugin>);

#[async_trait]
impl Handler for DeskHandler {
    async fn request(&self, peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
        match method {
            acp::method::INITIALIZE => {
                let node = node_meta(p)?;
                let section: DeskSection = serde_json::from_value(node.config.clone()).map_err(|e| {
                    RpcError::invalid_params(format!("the desk source's config needs person: <handle> ({e})"))
                })?;
                if !settings::valid_name(&section.person) {
                    return Err(RpcError::invalid_params("person: 1 to 64 letters, digits and - _ ."));
                }
                let mut started = self.0.started.lock().await;
                if !*started {
                    *started = true;
                    let peer = peer.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        let person = section.person.clone();
                        let config = desk::DeskConfig { person: section.person, command: section.command };
                        let (p1, p2, p3) = (peer.clone(), peer.clone(), peer.clone());
                        desk::run_source(
                            config,
                            move || p1.is_closed(),
                            move |data| {
                                let r = Reading {
                                    kind: DESK_KIND.into(),
                                    key: person.clone(),
                                    at: Timestamp::now(),
                                    node: None,
                                    data,
                                };
                                p2.notify(ext::method::READING_PUBLISH, &json!({"readings": [r]}));
                            },
                            move |problem, detail| {
                                p3.notify(ext::method::PLUGIN_STATUS, &PluginStatusReport { problem, detail });
                            },
                        )
                        .await;
                    });
                }
                initialized("fleetlayer-presence", "Presence desk source")
            }
            ext::method::PING => Ok(json!({})),
            _ => Err(RpcError::method_not_found(method)),
        }
    }
}

fn logs() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let home = home();
    let state_dir = args.state_dir.clone().map_or_else(|| default_state_dir(&home), |p| expand(&home, p));
    let socket = state_dir.join("presence.sock");
    match args.command {
        None => {
            logs();
            fleetlayer_plugin::serve_stdio(Arc::new(AuthorityHandler(Arc::new(AuthorityPlugin::default())))).await;
            Ok(())
        }
        Some(Command::Desk) => {
            logs();
            fleetlayer_plugin::serve_stdio(Arc::new(DeskHandler(Arc::new(DeskPlugin::default())))).await;
            Ok(())
        }
        Some(Command::Serve { caller }) => serve::run(socket, caller).await,
        Some(Command::Show { person, json }) => {
            let v = authority::ask(&socket, "get", None, json!({"person": person})).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                for p in v["people"].as_array().into_iter().flatten() {
                    println!("{}", p["line"].as_str().unwrap_or(""));
                }
            }
            Ok(())
        }
        Some(Command::Check { file }) => check(&file.unwrap_or_else(|| default_settings(&home))),
    }
}

fn check(file: &Path) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(file).map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
    let s = settings::parse(&text).map_err(|e| anyhow::anyhow!("{}:\n{e}", file.display()))?;
    println!("{}: {} people", file.display(), s.people.len());
    for p in s.people.values() {
        let night = p
            .night
            .map_or("no night".to_string(), |(a, b)| format!("night {}-{}", a.strftime("%H:%M"), b.strftime("%H:%M")));
        let desks = if p.desks.is_empty() { "no desks".to_string() } else { format!("desks {}", p.desks.join(", ")) };
        println!(
            "  {}: {}, {night}, {desks}, away after {} idle, a look at the phone counts for {}",
            p.name,
            p.tz_name,
            settings::words(p.away_after),
            settings::words(p.seen_for)
        );
    }
    Ok(())
}
