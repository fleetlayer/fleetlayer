//! fleetlayer-scheduler: sends a message to an agent session on a schedule (every few hours, on a cron expression,
//! or once at a moment), as itself through the node, so the access policy decides who it may reach
//! (`docs/plugins/scheduler.md`).
//!
//! Run with no arguments, it's a node plugin. Its config section in `node.yaml` (all optional):
//! - `settings`: the settings file with the entries (default `~/.config/fleetlayer/scheduler.yaml`).
//! - `state_dir`: where it keeps what fired when (default `~/.local/state/fleetlayer/scheduler`).
//! - `tz`: the time zone for cron entries without one (default the machine's).
//!
//! Commands: `list` (each entry, its next fire and its last result), `fire <name>` (send one now, to test it) and
//! `check [file]` (read a settings file and say what it would do).

mod engine;
mod runner;
mod schedule;
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
use fleetlayer_protocol::ext::{self, MessageSend, NodeMeta, PluginMeta, PluginStatusReport, Roles, meta_key};
use fleetlayer_protocol::rpc::RpcError;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::engine::{State, round};
use crate::runner::{FireRequest, Paths, Runner};

/// How long a send may take before its outcome is "no answer".
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Parser, Debug)]
#[command(name = "fleetlayer-scheduler", version, about = "Send messages to agent sessions on a schedule")]
struct Args {
    /// The scheduler's state directory (default ~/.local/state/fleetlayer/scheduler).
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Each entry: where it sends, when it fires next, and how its last fire went.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Send an entry's message now, to test it. Its schedule doesn't change.
    Fire { name: String },
    /// Read a settings file (default ~/.config/fleetlayer/scheduler.yaml) and print each entry's next fires.
    Check { file: Option<PathBuf> },
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    settings: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    tz: Option<String>,
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
    home.join(".config/fleetlayer/scheduler.yaml")
}

fn default_state_dir(home: &Path) -> PathBuf {
    home.join(".local/state/fleetlayer/scheduler")
}

#[derive(Default)]
struct Plugin {
    started: Mutex<bool>,
}

struct Shared(Arc<Plugin>);

struct ToNode(Peer);

#[async_trait]
impl runner::Node for ToNode {
    async fn send(&self, to: &str, text: &str) -> Result<Value, RpcError> {
        let m = MessageSend {
            to: to.to_string(),
            text: text.to_string(),
            untrusted: false,
            subkind: Some("scheduled".into()),
        };
        self.0.request_within(ext::method::MESSAGE_SEND, &m, SEND_TIMEOUT).await
    }

    fn status(&self, problem: Option<String>, detail: Value) {
        self.0.notify(ext::method::PLUGIN_STATUS, &PluginStatusReport { problem, detail });
    }
}

#[async_trait]
impl Handler for Shared {
    async fn request(&self, peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
        match method {
            acp::method::INITIALIZE => {
                let req: InitializeRequest = params(p)?;
                let node: NodeMeta = ext::get_meta(req.client_capabilities.meta.as_ref(), meta_key::NODE)
                    .map_err(RpcError::invalid_params)?
                    .ok_or_else(|| RpcError::refused("fleetlayer-scheduler runs under a Fleetlayer node"))?;
                let home = PathBuf::from(&node.home);
                let config: Config = serde_json::from_value(node.config.clone()).map_err(RpcError::invalid_params)?;
                let tz = match &config.tz {
                    Some(name) => TimeZone::get(name).map_err(|_| RpcError::invalid_params(format!("tz: '{name}'")))?,
                    None => TimeZone::system(),
                };
                let paths = Paths {
                    settings: config.settings.map_or_else(|| default_settings(&home), |p| expand(&home, p)),
                    state_dir: config.state_dir.map_or_else(|| default_state_dir(&home), |p| expand(&home, p)),
                };
                let runner = Runner::open(paths, tz).map_err(RpcError::internal)?;
                let mut started = self.0.started.lock().await;
                if !*started {
                    *started = true;
                    let peer2 = peer.clone();
                    tokio::spawn(async move {
                        // Start once the node has had our answer.
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        let closed = peer2.clone();
                        runner::run(Arc::new(runner), Arc::new(ToNode(peer2)), move || closed.is_closed()).await;
                    });
                }
                let meta = PluginMeta { protocol: fleetlayer_protocol::PROTOCOL, roles: Roles::default() };
                answer(&InitializeResponse {
                    protocol_version: fleetlayer_protocol::ACP_PROTOCOL,
                    agent_capabilities: Capabilities {
                        meta: Some(ext::meta_with(meta_key::PLUGIN, &meta)),
                        ..Default::default()
                    },
                    agent_info: Some(Implementation {
                        name: "fleetlayer-scheduler".into(),
                        title: Some("Scheduler".into()),
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let home = home();
    let state_dir = args.state_dir.clone().map_or_else(|| default_state_dir(&home), |p| expand(&home, p));
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
        Some(Command::List { json }) => list(&state_dir, json),
        Some(Command::Fire { name }) => fire(&state_dir, &name).await,
        Some(Command::Check { file }) => check(&file.unwrap_or_else(|| default_settings(&home))),
    }
}

fn read_state(state_dir: &Path) -> anyhow::Result<State> {
    let path = state_dir.join("state.json");
    let bytes = std::fs::read(&path).map_err(|e| {
        anyhow::anyhow!("{}: {e}. Is the scheduler plugin running on this node, as this user?", path.display())
    })?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn list(state_dir: &Path, as_json: bool) -> anyhow::Result<()> {
    let state = read_state(state_dir)?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&state)?);
        return Ok(());
    }
    let now = Timestamp::now();
    if let Some(at) = state.updated_at
        && now.duration_since(at).as_secs() > 120
    {
        println!("The scheduler last saved its state at {}: it may not be running.\n", round(at));
    }
    if let Some(f) = &state.settings_file {
        println!("Settings: {f}");
    }
    if let Some(p) = &state.problem {
        println!("Problem: {p}");
    }
    if state.entries.is_empty() {
        println!("No entries.");
    }
    for (name, st) in &state.entries {
        println!();
        println!("{name}{}", if st.enabled { "" } else { " (off)" });
        println!("  to:    {}", st.to.as_deref().unwrap_or("?"));
        println!("  when:  {}", st.schedule.as_deref().unwrap_or("?"));
        match st.next_fire {
            Some(t) if st.pending.is_some() => println!("  next:  {} (trying again: the session was busy)", round(t)),
            Some(t) => println!("  next:  {}", round(t)),
            None => println!("  next:  none"),
        }
        if let Some(f) = &st.last_fire {
            let err = f.error.as_deref().map(|e| format!(": {e}")).unwrap_or_default();
            println!("  last:  {} {}{err}", round(f.at), f.result.words());
        }
        if st.failures > 0 {
            println!("  failed fires in a row: {}", st.failures);
        }
        if st.missed > 0 {
            println!("  fires never sent (scheduler off): {}", st.missed);
        }
    }
    Ok(())
}

async fn fire(state_dir: &Path, name: &str) -> anyhow::Result<()> {
    let state = read_state(state_dir)?;
    if !state.entries.contains_key(name) {
        let names: Vec<&String> = state.entries.keys().collect();
        anyhow::bail!("no entry is named '{name}' (entries: {names:?})");
    }
    let id = format!("{}-{}", Timestamp::now().as_nanosecond(), std::process::id());
    let dir = state_dir.join("fire");
    std::fs::create_dir_all(&dir)?;
    let req = serde_json::to_vec(&FireRequest { id: id.clone(), name: name.to_string() })?;
    runner::write_atomic(&dir.join(format!("{id}.json")), &req)?;
    // The plugin takes requests every five seconds; a send takes up to 30.
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let Ok(state) = read_state(state_dir) else { continue };
        if let Some(r) = state.manual.iter().find(|r| r.id == id) {
            let err = r.error.as_deref().map(|e| format!(": {e}")).unwrap_or_default();
            println!("{name}: {}{err}", r.result.words());
            if r.result == engine::Outcome::Sent {
                return Ok(());
            }
            std::process::exit(1);
        }
    }
    let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
    anyhow::bail!("the scheduler didn't take the request within 40 seconds: is its plugin running on this node?")
}

fn check(file: &Path) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(file).map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
    let s = settings::parse(&text, &TimeZone::system()).map_err(|e| anyhow::anyhow!("{}:\n{e}", file.display()))?;
    let now = Timestamp::now();
    println!("{}: {} entries, alert after {} failed fires in a row", file.display(), s.entries.len(), s.alert_after);
    for e in &s.entries {
        let mut next = vec![];
        let mut t = now;
        while next.len() < 3 {
            match e.schedule.next_after(t, now) {
                Some(n) => {
                    next.push(round(n).to_string());
                    t = n;
                }
                None => break,
            }
        }
        println!();
        println!("{}{}", e.name, if e.enabled { "" } else { " (off)" });
        println!("  to:    {}", e.to);
        println!("  when:  {}", e.schedule.describe());
        println!("  next:  {}", if next.is_empty() { "never (in the past)".into() } else { next.join(", ") });
    }
    Ok(())
}
