//! fleetlayer-leases: leases for agents across a fleet. A lease is a claim on something for a while: one of a build
//! server's slots, or an approved claim that another service acts on (a mail filter that's created when the lease
//! starts and removed when it ends). It ends when its holder releases it, at its TTL, or when its holder dies
//! (`docs/plugins/leases.md`).
//!
//! One authority keeps every lease: this program as a plugin on an always-on node, with its state in one directory.
//! Agents reach it through a tool the same node shares, `fleetlayer-leases serve --caller {caller}`, so every call
//! goes through the control plane's access policy and audit log; `fleetlayer lease` is the command line for it.
//!
//! Run with no arguments, it's the node plugin. Its config section in `node.yaml` (all optional):
//! - `settings`: the kinds of lease (default `~/.config/fleetlayer/leases.yaml`).
//! - `state_dir`: its state (default `~/.local/state/fleetlayer/leases`).
//! - `name`: its name in node.yaml (default `leases`): it asks the access policy as `plugin:<name>@<node>`.
//!
//! Commands: `serve --caller <who>` (the MCP server the node shares), and on the authority's machine `list`, `show
//! <id>`, `release <id>` and `check [file]`.

mod engine;
mod runner;
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
use fleetlayer_protocol::ext::{
    self, Event, MessageSend, NodeMeta, PluginMeta, PluginStatusReport, Reading, Roles, meta_key,
};
use fleetlayer_protocol::link::{AccessCheck, AccessDecision};
use fleetlayer_protocol::rpc::RpcError;
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::runner::{Authority, Paths};

#[derive(Parser, Debug)]
#[command(name = "fleetlayer-leases", version, about = "Leases and counted slots for agents across a fleet")]
struct Args {
    /// The authority's state directory (default ~/.local/state/fleetlayer/leases).
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// The MCP server the authority's node shares as a tool: `args: [serve, --caller, "{caller}"]`.
    Serve {
        /// `<tab>@<node>` of the caller, as the control plane recorded it (`-@<node>` for a script).
        #[arg(long, allow_hyphen_values = true)]
        caller: String,
    },
    /// Every kind, who holds and who waits, and the latest leases that ended.
    List {
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// One lease.
    Show { id: u64 },
    /// End a lease, whoever holds it (the authority's admin).
    Release { id: u64 },
    /// Read a settings file (default ~/.config/fleetlayer/leases.yaml) and print its kinds.
    Check { file: Option<PathBuf> },
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    settings: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    name: Option<String>,
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
    home.join(".config/fleetlayer/leases.yaml")
}

pub fn default_state_dir(home: &Path) -> PathBuf {
    home.join(".local/state/fleetlayer/leases")
}

struct ToNode(Peer);

const NODE_TIMEOUT: Duration = Duration::from_secs(20);

#[async_trait]
impl runner::Node for ToNode {
    async fn fleet(&self) -> Result<Value, RpcError> {
        self.0.request_within(ext::method::FLEET_QUERY, &json!({}), NODE_TIMEOUT).await
    }

    async fn access(&self, who: &str, to: &str, why: &str, peek: bool) -> Result<AccessDecision, RpcError> {
        let req = AccessCheck {
            tenant: String::new(),
            who: who.into(),
            can: "write".into(),
            to: to.into(),
            why: Some(why.into()),
            peek,
        };
        self.0.request_within(ext::method::ACCESS_CHECK, &req, NODE_TIMEOUT).await
    }

    async fn publish(&self, event: Event) -> Result<(), RpcError> {
        self.0.request_within::<_, Value>(ext::method::EVENT_PUBLISH, &event, NODE_TIMEOUT).await.map(|_| ())
    }

    async fn send(&self, to: &str, text: &str) -> Result<(), RpcError> {
        let m = MessageSend { to: to.into(), text: text.into(), untrusted: false, subkind: Some("lease".into()) };
        self.0.request_within::<_, Value>(ext::method::MESSAGE_SEND, &m, NODE_TIMEOUT).await.map(|_| ())
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

#[derive(Default)]
struct Plugin {
    started: Mutex<bool>,
}

struct Shared(Arc<Plugin>);

#[async_trait]
impl Handler for Shared {
    async fn request(&self, peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
        match method {
            acp::method::INITIALIZE => {
                let req: InitializeRequest = params(p)?;
                let node: NodeMeta = ext::get_meta(req.client_capabilities.meta.as_ref(), meta_key::NODE)
                    .map_err(RpcError::invalid_params)?
                    .ok_or_else(|| RpcError::refused("fleetlayer-leases runs under a Fleetlayer node"))?;
                let home = PathBuf::from(&node.home);
                // No `config:` section in node.yaml comes as null: every setting has a default.
                let config: Config = match &node.config {
                    Value::Null => Config::default(),
                    c => serde_json::from_value(c.clone()).map_err(RpcError::invalid_params)?,
                };
                let paths = Paths {
                    settings: config.settings.map_or_else(|| default_settings(&home), |p| expand(&home, p)),
                    state_dir: config.state_dir.map_or_else(|| default_state_dir(&home), |p| expand(&home, p)),
                };
                let me = format!("plugin:{}@{}", config.name.as_deref().unwrap_or("leases"), node.node);
                let mut started = self.0.started.lock().await;
                if !*started {
                    let authority = Authority::open(paths.clone(), me, Arc::new(ToNode(peer.clone())))
                        .map_err(RpcError::internal)?;
                    let listener = bind(&paths.socket()).map_err(RpcError::internal)?;
                    *started = true;
                    start(authority, listener, peer.clone());
                }
                let meta = PluginMeta { protocol: fleetlayer_protocol::PROTOCOL, roles: Roles::default() };
                answer(&InitializeResponse {
                    protocol_version: fleetlayer_protocol::ACP_PROTOCOL,
                    agent_capabilities: Capabilities {
                        meta: Some(ext::meta_with(meta_key::PLUGIN, &meta)),
                        ..Default::default()
                    },
                    agent_info: Some(Implementation {
                        name: "fleetlayer-leases".into(),
                        title: Some("Leases".into()),
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

/// The socket the shim and the admin commands use: only this Unix user may connect.
fn bind(path: &Path) -> anyhow::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    // A socket left by a plugin that stopped: nothing listens on it, so it's safe to replace.
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        anyhow::bail!("{} is in use: is another fleetlayer-leases running with this state directory?", path.display());
    }
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

fn start(authority: Arc<Authority>, listener: tokio::net::UnixListener, peer: Peer) {
    tokio::spawn(authority.clone().serve(listener));
    let a = authority.clone();
    let closed = peer.clone();
    tokio::spawn(async move {
        // Start once the node has had our answer.
        tokio::time::sleep(Duration::from_millis(200)).await;
        while !closed.is_closed() {
            a.round().await;
            tokio::time::sleep(runner::ROUND).await;
        }
    });
    // A round that never finishes (a lock held, a call that hangs) is reported from here.
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            if peer.is_closed() {
                return;
            }
            let last = *authority.last_round.lock().await;
            if let Some(t) = last
                && Timestamp::now().duration_since(t).as_secs() > 180
            {
                authority.stuck(t);
            }
        }
    });
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let home = home();
    let state_dir = args.state_dir.clone().map_or_else(|| default_state_dir(&home), |p| expand(&home, p));
    let socket = state_dir.join("leases.sock");
    match args.command {
        None => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
                )
                .init();
            fleetlayer_plugin::serve_stdio(Arc::new(Shared(Arc::new(Plugin::default())))).await;
            Ok(())
        }
        Some(Command::Serve { caller }) => serve::run(socket, caller).await,
        Some(Command::List { kind, json }) => {
            let v = serve::ask(&socket, "list", None, json!({"kind": kind, "all": json}), |_| {}).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print!("{}", list_text(&v));
            }
            Ok(())
        }
        Some(Command::Show { id }) => {
            let v = serve::ask(&socket, "show", None, json!({"id": id}), |_| {}).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        Some(Command::Release { id }) => {
            serve::ask(&socket, "release", None, json!({"id": id}), |_| {}).await?;
            println!("Released lease #{id}.");
            Ok(())
        }
        Some(Command::Check { file }) => check(&file.unwrap_or_else(|| default_settings(&home))),
    }
}

/// `list` for a person or an agent to read; `fleetlayer lease list` prints the same.
pub fn list_text(v: &Value) -> String {
    use std::fmt::Write;
    let mut o = String::new();
    let kinds = v["kinds"].as_array().cloned().unwrap_or_default();
    if kinds.is_empty() {
        let _ = writeln!(o, "No kinds of lease are set up.");
    }
    let line = |l: &Value| -> String {
        let since = l["startedAt"].as_str().or(l["requestedAt"].as_str()).unwrap_or("?");
        let payload = l.get("payload").filter(|p| !p.is_null()).map(|p| format!(" {p}")).unwrap_or_default();
        format!(
            "#{} {} since {}: {}{payload}",
            l["id"],
            l["holder"].as_str().unwrap_or("?"),
            since,
            l["reason"].as_str().unwrap_or("")
        )
    };
    for k in &kinds {
        let slots = match k["slots"].as_u64() {
            Some(n) => format!("{} of {n} slots held", k["held"].as_array().map_or(0, Vec::len)),
            None => format!("{} held", k["held"].as_array().map_or(0, Vec::len)),
        };
        let approval = if k["approval"] == true { ", each payload approved once" } else { "" };
        let _ = writeln!(
            o,
            "{}: {slots} (ttl {}, idle {}{approval})",
            k["name"].as_str().unwrap_or("?"),
            k["ttl"].as_str().unwrap_or("?"),
            k["idle"].as_str().unwrap_or("?")
        );
        for (what, key) in [("held", "held"), ("waiting", "waiting"), ("asking", "asking")] {
            for l in k[key].as_array().into_iter().flatten() {
                let extra = match key {
                    "held" => format!(" (until {})", l["expiresAt"].as_str().unwrap_or("?")),
                    "asking" => l["approval"].as_str().map(|a| format!(" ({a})")).unwrap_or_default(),
                    _ => String::new(),
                };
                let _ = writeln!(o, "  {what:<8} {}{extra}", line(l));
            }
        }
    }
    let ended = v["ended"].as_array().cloned().unwrap_or_default();
    if !ended.is_empty() {
        let _ = writeln!(o, "Ended lately:");
        for e in &ended {
            let _ = writeln!(
                o,
                "  #{} {} {} at {}: {}",
                e["id"],
                e["kind"].as_str().unwrap_or("?"),
                e["holder"].as_str().unwrap_or("?"),
                e["endedAt"].as_str().unwrap_or("?"),
                e["why"].as_str().unwrap_or("?").replace('_', " ")
            );
        }
    }
    o
}

fn check(file: &Path) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(file).map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
    let s = settings::parse(&text).map_err(|e| anyhow::anyhow!("{}:\n{e}", file.display()))?;
    println!("{}: {} kinds", file.display(), s.kinds.len());
    for k in s.kinds.values() {
        let slots = k.slots.map_or("no limit".to_string(), |n| format!("{n} slots"));
        let mut extras = vec![];
        if k.approval {
            extras.push("approval per payload".to_string());
        }
        if let Some(c) = &k.events {
            extras.push(format!("events on channel {c}"));
        }
        if let Some(m) = k.max_per_holder {
            extras.push(format!("at most {m} per holder"));
        }
        println!(
            "  {}: {slots}, ttl {} (at most {}), idle {}{}",
            k.name,
            engine::words(k.ttl),
            engine::words(k.max_ttl),
            engine::words(k.idle),
            if extras.is_empty() { String::new() } else { format!(", {}", extras.join(", ")) }
        );
    }
    Ok(())
}
