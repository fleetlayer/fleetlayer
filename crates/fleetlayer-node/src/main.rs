//! fleetlayer-node: the daemon on each machine (or each Unix user on a shared machine). It runs the plugins its config
//! enables, keeps a registry of the sessions they report, delivers messages into sessions, and serves the `fleetlayer`
//! CLI on a local socket. Linking to the control plane comes next.

mod config;
mod deliver;
mod link;
mod local;
mod plugins;
mod sessions;

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
}

impl Node {
    pub fn plugin(&self, name: &str) -> Option<&Arc<PluginHandle>> {
        self.plugins.iter().find(|p| p.name() == name)
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
    }
}

async fn run(cfg: NodeConfig) -> anyhow::Result<()> {
    let mut names = std::collections::HashSet::new();
    for p in &cfg.plugins {
        anyhow::ensure!(names.insert(p.name.clone()), "two plugins are called {} in node.yaml", p.name);
    }
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
    });
    tracing::info!(node = %node.name, machine = %node.machine, plugins = node.plugins.len(), "fleetlayer-node starting");
    for p in &node.plugins {
        tokio::spawn(plugins::supervise(node.clone(), p.clone()));
    }
    match cfg.control.clone() {
        Some(mut control) => {
            if let Ok(rest) = control.token_file.strip_prefix("~") {
                control.token_file = config::home().join(rest);
            }
            tokio::spawn(link::run(node.clone(), control));
        }
        None => tracing::info!("no control: section in node.yaml, so this node works on its own"),
    }
    let socket = cfg.socket();
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
