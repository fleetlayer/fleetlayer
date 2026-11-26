//! fleetlayer: the command line. It talks to this machine's node over its local socket.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use clap::{Parser, Subcommand};
use fleetlayer_plugin::{Handler, Peer};
use fleetlayer_protocol::rpc::RpcError;
use serde_json::{Value, json};

#[derive(Parser)]
#[command(name = "fleetlayer", version, about = "Fleetlayer: every agent session on every machine, connected")]
struct Cli {
    /// The node's socket. Default: $FLEETLAYER_SOCKET, else the node's default.
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    /// Print JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// The node and its plugins.
    Status,
    /// Every session on this node.
    Sessions {
        /// Include sessions that ended.
        #[arg(long)]
        all: bool,
    },
    /// Every node and session in the fleet, from the control plane.
    Fleet,
    /// Subscribe this session (or --target) to a channel's events, or to one subject on it.
    Subscribe(SubscribeArgs),
    /// Undo a subscription.
    Unsubscribe(SubscribeArgs),
    /// Every subscription in the fleet.
    Subscriptions,
    /// Messages between sessions.
    Msg {
        #[command(subcommand)]
        command: Msg,
    },
}

#[derive(clap::Args)]
struct SubscribeArgs {
    channel: String,
    #[arg(long)]
    subject: Option<String>,
    /// `<tab>@<node>`. Default: the session this runs in.
    #[arg(long)]
    target: Option<String>,
}

#[derive(Subcommand)]
enum Msg {
    /// Send a message to a session: `<tab>@<node>`, `<tab>` on this node, or a session ID. Fails at once if it isn't
    /// running.
    Send {
        #[arg(long)]
        to: String,
        text: String,
    },
}

struct Named {
    command_name: &'static str,
    command: Command,
    json: bool,
}

fn default_socket() -> PathBuf {
    if let Some(p) = std::env::var_os("FLEETLAYER_SOCKET") {
        return PathBuf::from(p);
    }
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) => PathBuf::from(d).join("fleetlayer/node.sock"),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state/fleetlayer/node.sock"),
    }
}

struct NoRequests;

#[async_trait]
impl Handler for NoRequests {
    async fn request(&self, _peer: &Peer, method: &str, _params: Value) -> Result<Value, RpcError> {
        Err(RpcError::method_not_found(method))
    }
}

/// The session this command runs in, from the environment its harness and session manager set.
fn caller() -> Value {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    json!({
        "sessionId": env("CLAUDE_CODE_SESSION_ID"),
        "address": env("FLEETLAYER_SESSION_ADDRESS").or_else(|| env("HERDR_PANE_ID")),
    })
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("fleetlayer: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let socket = cli.socket.clone().unwrap_or_else(default_socket);
    let stream = tokio::net::UnixStream::connect(&socket).await.map_err(|e| {
        anyhow::anyhow!("can't reach the node at {} ({e}). Is fleetlayer-node running?", socket.display())
    })?;
    let (r, w) = stream.into_split();
    let (peer, _task) = fleetlayer_plugin::connect(r, w, Arc::new(NoRequests));
    let cli = Named {
        command_name: match &cli.command {
            Command::Unsubscribe(_) => "unsubscribe",
            Command::Subscribe(_) => "subscribe",
            _ => "",
        },
        command: cli.command,
        json: cli.json,
    };
    match cli.command {
        Command::Status => {
            let v: Value = peer.request("node/status", &json!({})).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            let c = &v["control"];
            let link = if c["connected"].as_bool() == Some(true) {
                "linked to the control plane".to_string()
            } else {
                match c["lastError"].as_str() {
                    Some(e) => format!("not linked ({e})"),
                    None => "not linked to a control plane".into(),
                }
            };
            println!(
                "node {} on {} (user {}), version {}, {link}",
                v["node"].as_str().unwrap_or("?"),
                v["machine"].as_str().unwrap_or("?"),
                v["user"].as_str().unwrap_or("?"),
                v["version"].as_str().unwrap_or("?")
            );
            for p in v["plugins"].as_array().into_iter().flatten() {
                let roles: Vec<&str> = p["roles"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                let mut line = format!(
                    "  {:<16} {:<11} {}",
                    p["name"].as_str().unwrap_or("?"),
                    p["state"].as_str().unwrap_or("?"),
                    roles.join(", ")
                );
                if let Some(r) = p["reason"].as_str() {
                    line.push_str(&format!("  ({r})"));
                }
                println!("{line}");
            }
        }
        Command::Sessions { all } => {
            let v: Value = peer.request("node/sessions", &json!({})).await?;
            let node = v["node"].as_str().unwrap_or("?").to_string();
            let sessions: Vec<&Value> =
                v["sessions"].as_array().into_iter().flatten().filter(|s| all || s["ended"].is_null()).collect();
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
                return Ok(());
            }
            for s in sessions {
                let m = &s["meta"];
                let name = m["tab"].as_str().or(s["title"].as_str()).unwrap_or("-");
                println!(
                    "{:<24} {:<8} {:<8} {:<10} {}",
                    format!("{name}@{node}"),
                    m["harness"].as_str().unwrap_or("?"),
                    m["status"].as_str().unwrap_or("unknown"),
                    if s["ended"].is_null() { "" } else { "ended" },
                    s["sessionId"].as_str().unwrap_or("")
                );
            }
        }
        Command::Fleet => {
            let v: Value = peer.request("node/fleet", &json!({})).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            for n in v["nodes"].as_array().into_iter().flatten() {
                let name = n["name"].as_str().unwrap_or("?");
                let online = if n["online"].as_bool() == Some(true) { "online" } else { "offline" };
                println!("{name} ({}, {online})", n["machine"].as_str().unwrap_or("?"));
                let mine = v["sessions"].as_array().into_iter().flatten().filter(|s| s["node"].as_str() == Some(name));
                for s in mine.filter(|s| s["ended"].is_null()) {
                    let m = &s["meta"];
                    println!(
                        "  {:<24} {:<8} {}",
                        format!("{}@{name}", m["tab"].as_str().unwrap_or("-")),
                        m["harness"].as_str().unwrap_or("?"),
                        m["status"].as_str().unwrap_or("unknown")
                    );
                }
            }
        }
        Command::Subscribe(a) | Command::Unsubscribe(a) => {
            let subscribing = matches!(cli.command_name, "subscribe");
            let method = if subscribing { "node/subscribe" } else { "node/unsubscribe" };
            let args = json!({"channel": a.channel, "subject": a.subject, "target": a.target, "caller": caller()});
            let v: Value = peer.request(method, &args).await?;
            if cli.json {
                println!("{v}");
            } else if subscribing {
                println!("Subscribed to {}.", a.channel);
            } else {
                println!(
                    "{}",
                    if v["removed"].as_bool() == Some(true) {
                        "Unsubscribed."
                    } else {
                        "There was no such subscription."
                    }
                );
            }
        }
        Command::Subscriptions => {
            let v: Value = peer.request("node/subscriptions", &json!({})).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            for s in v["subscriptions"].as_array().into_iter().flatten() {
                let on = match s["subject"].as_str() {
                    Some(subj) => format!("{} ({subj})", s["channel"].as_str().unwrap_or("?")),
                    None => s["channel"].as_str().unwrap_or("?").to_string(),
                };
                println!("{on:<40} {}", s["target"].as_str().unwrap_or("?"));
            }
        }
        Command::Msg { command: Msg::Send { to, text } } => {
            let v: Value = peer
                .request("_fleetlayer.dev/message/send", &json!({"to": to, "text": text, "caller": caller()}))
                .await?;
            if cli.json {
                println!("{v}");
            } else {
                println!("Delivered to {to} ({}).", v["via"].as_str().unwrap_or("?"));
            }
        }
    }
    Ok(())
}
