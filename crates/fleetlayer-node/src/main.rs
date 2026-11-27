//! fleetlayer-node: the daemon on each machine (or each Unix user on a shared machine). It runs the plugins its config
//! enables, keeps a registry of the sessions they report, delivers messages into sessions, and serves the `fleetlayer`
//! CLI on a local socket. Linking to the control plane comes next.

mod config;
mod deliver;
mod heartbeat;
mod keys;
mod link;
mod local;
mod plugins;
mod readings;
mod service;
mod sessions;
mod sync;
mod terminals;
mod tools;
mod update;

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};

use crate::config::NodeConfig;
use crate::plugins::PluginHandle;

#[derive(Parser)]
#[command(name = "fleetlayer-node", version, about = "Fleetlayer's node daemon")]
struct Cli {
    /// The config file. Default: ~/.config/fleetlayer/node.yaml, or $FLEETLAYER_CONFIG.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon (the default).
    Run,
    /// Print the config as the node reads it, defaults filled in.
    Config,
    /// Run the node as a service: a launchd agent on macOS, a systemd user service on Linux. Run it again after
    /// moving the binary or changing PATH.
    Install {
        /// Write the service file without starting it.
        #[arg(long)]
        no_start: bool,
    },
    /// Stop the service and remove its file.
    Uninstall,
}

/// Everything the node's tasks share.
pub struct Node {
    pub name: String,
    pub machine: String,
    pub user: String,
    pub home: PathBuf,
    pub plugins: Vec<Arc<PluginHandle>>,
    pub registry: sessions::Registry,
    pub deliveries: deliver::Deliveries,
    pub link: link::Link,
    pub typing_fallback: bool,
    pub has_control: bool,
    /// The gateway's address, checked as part of the node's health.
    pub gateway: Option<String>,
    /// The control plane link's settings (token path expanded), for tool streams.
    pub control: Option<config::ControlConfig>,
    pub tools: Vec<tools::ToolConfig>,
    /// This node's age public key (`age1…`).
    pub public_key: String,
    pub identity: age::x25519::Identity,
    pub sync: sync::Sync,
    pub opt_ins: tools::OptIns,
    pub readings: readings::Readings,
}

impl Node {
    pub fn plugin(&self, name: &str) -> Option<&Arc<PluginHandle>> {
        self.plugins.iter().find(|p| p.name() == name)
    }

    /// Ask the running observer plugins (summaries) until one answers `method`.
    pub async fn ask_observer(
        &self,
        method: &str,
        params: &serde_json::Value,
        timeout: std::time::Duration,
    ) -> Result<serde_json::Value, fleetlayer_protocol::rpc::RpcError> {
        for p in &self.plugins {
            if p.roles().await.observer.is_none() {
                continue;
            }
            if let Some(peer) = p.peer().await {
                match peer.request_within(method, params, timeout).await {
                    Err(e) if e.code == fleetlayer_protocol::rpc::code::METHOD_NOT_FOUND => continue,
                    other => return other,
                }
            }
        }
        Err(fleetlayer_protocol::rpc::RpcError::not_here(format!("no summaries plugin runs on {}", self.name)))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FLEETLAYER_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    let path = cli.config.unwrap_or_else(config::default_path);
    let cfg = NodeConfig::load(&path)?;
    match cli.command.unwrap_or(Command::Run) {
        Command::Config => {
            println!("# {}", path.display());
            println!(
                "name: {}\nmachine: {}\nuser: {}\nsocket: {}",
                cfg.name(),
                cfg.machine(),
                cfg.user(),
                cfg.socket().display()
            );
            println!("{}", yaml_serde::to_string(&serde_json::json!({"plugins": cfg.plugins}))?);
            Ok(())
        }
        Command::Run => run(cfg).await,
        Command::Install { no_start } => service::install(&config::home(), &path, !no_start),
        Command::Uninstall => service::uninstall(&config::home()),
    }
}

async fn run(cfg: NodeConfig) -> anyhow::Result<()> {
    let mut names = std::collections::HashSet::new();
    for p in &cfg.plugins {
        anyhow::ensure!(names.insert(p.name.clone()), "two plugins are called {} in node.yaml", p.name);
    }
    let key = keys::load_or_create(&config::config_dir().join("node-key"))?;
    let node = Arc::new(Node {
        name: cfg.name(),
        machine: cfg.machine(),
        user: cfg.user(),
        home: config::home(),
        plugins: cfg.plugins.iter().cloned().map(PluginHandle::new).collect(),
        registry: sessions::Registry::default(),
        deliveries: deliver::Deliveries::default(),
        link: link::Link::default(),
        typing_fallback: cfg.typing_fallback,
        has_control: cfg.control.is_some(),
        gateway: cfg.gateway.clone(),
        control: cfg.control.clone().map(|mut c| {
            if let Ok(rest) = c.token_file.strip_prefix("~") {
                c.token_file = config::home().join(rest);
            }
            c
        }),
        tools: cfg.tools.clone(),
        public_key: key.to_public().to_string(),
        identity: key,
        sync: sync::Sync::default(),
        opt_ins: tools::OptIns::default(),
        readings: readings::Readings::default(),
    });
    tracing::info!(node = %node.name, machine = %node.machine, plugins = node.plugins.len(), "fleetlayer-node starting");
    for p in &node.plugins {
        tokio::spawn(plugins::supervise(node.clone(), p.clone()));
    }
    match node.control.clone() {
        Some(control) => {
            tokio::spawn(link::run(node.clone(), control));
        }
        None => tracing::info!("no control: section in node.yaml, so this node works on its own"),
    }
    if let Some(h) = cfg.heartbeat.clone() {
        tokio::spawn(heartbeat::run(node.clone(), h, cfg.control.is_some()));
    }
    let update = cfg.update.clone().unwrap_or(update::UpdateConfig { mode: None, every_hours: 6 });
    tokio::spawn(update::run(node.clone(), update));
    let socket = cfg.socket();
    let tools_socket = socket.with_extension("tools.sock");
    {
        let node = node.clone();
        tokio::spawn(async move {
            if let Err(e) = tools::listen(node, &tools_socket).await {
                tracing::error!("the tools socket stopped: {e}");
            }
        });
    }
    tokio::select! {
        r = local::serve(node.clone(), &socket) => r,
        _ = shutdown() => {
            tracing::info!("stopping");
            let _ = std::fs::remove_file(&socket);
            Ok(())
        }
    }
}

async fn shutdown() {
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(mut term) = signal(SignalKind::terminate()) else { return std::future::pending().await };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}
