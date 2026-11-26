//! fleetlayer-control: the control plane. It knows every enrolled node and the sessions on it, carries messages
//! between sessions on different nodes, and routes events (webhooks, plugins' events) to the sessions subscribed to
//! their channel. It never holds credentials: node tokens and webhook secrets are stored only as hashes, and secrets
//! of the user's only ever pass through sealed to a node's key.
//!
//! Self-hosted: one binary and a SQLite file. The API and node links listen on one port; webhooks on another, so only
//! that one needs to face the internet.

mod acl;
mod api;
mod db;
mod hooks;
mod link;
mod route;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::routing::get;
use clap::{Parser, Subcommand};

use crate::db::{DEFAULT_TENANT, Db, Subscription};

#[derive(Parser)]
#[command(name = "fleetlayer-control", version, about = "Fleetlayer's control plane")]
struct Cli {
    /// The SQLite database.
    #[arg(long, global = true, env = "FLEETLAYER_CONTROL_DB", default_value = "fleetlayer-control.db")]
    db: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the control plane.
    Serve {
        /// The API and node links.
        #[arg(long, env = "FLEETLAYER_CONTROL_LISTEN", default_value = "127.0.0.1:7600")]
        listen: SocketAddr,
        /// Webhooks only; the one listener to expose to the internet.
        #[arg(long, env = "FLEETLAYER_CONTROL_HOOKS_LISTEN", default_value = "127.0.0.1:7601")]
        hooks_listen: SocketAddr,
        /// A file holding the admin token for the read API. Without one, the API refuses every call.
        #[arg(long, env = "FLEETLAYER_CONTROL_ADMIN_TOKEN_FILE")]
        admin_token_file: Option<PathBuf>,
    },
    /// Enrol, list and remove nodes.
    Node {
        #[command(subcommand)]
        command: NodeCommand,
    },
    /// Webhook endpoints.
    Endpoint {
        #[command(subcommand)]
        command: EndpointCommand,
    },
    /// Subscribe a session (`<tab>@<node>`) to a channel, optionally to one subject on it.
    Subscribe {
        channel: String,
        target: String,
        #[arg(long)]
        subject: Option<String>,
    },
    Unsubscribe {
        channel: String,
        target: String,
        #[arg(long)]
        subject: Option<String>,
    },
    /// Access control: who may message whom (and which sessions a node sees).
    Acl {
        #[command(subcommand)]
        command: AclCommand,
    },
    /// Where a channel's events go when nobody subscribes (`<tab>@<node>`), or `--none`.
    Fallback {
        channel: String,
        target: Option<String>,
        #[arg(long)]
        none: bool,
    },
}

#[derive(Subcommand)]
enum AclCommand {
    /// Every rule, in order, and the default.
    List,
    /// Add a rule: `<tab>@<node>` patterns with * as a wildcard; a sender with no session is `-@<node>`.
    Add {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        /// allow or deny
        action: String,
        /// Where it goes in the order (lower first). Default: last.
        #[arg(long)]
        position: Option<i64>,
    },
    Remove {
        id: i64,
    },
    /// What happens when no rule matches: allow or deny.
    Default {
        action: String,
    },
    /// Whether `from` may message `to` now.
    Check {
        from: String,
        to: String,
    },
}

#[derive(Subcommand)]
enum NodeCommand {
    /// Enrol a node and print its token (shown once; enrolling again replaces it).
    Add {
        name: String,
    },
    List,
    Remove {
        name: String,
    },
}

#[derive(Subcommand)]
enum EndpointCommand {
    /// Add an endpoint and print its path (with its secret, shown once).
    Add {
        #[arg(long)]
        label: String,
        #[arg(long)]
        channel: String,
    },
    List,
}

pub struct App {
    pub db: Db,
    pub links: link::Links,
    pub hook_limits: hooks::Limits,
    pub admin_token_hash: Option<String>,
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
    let db = db::open(&cli.db).await?;
    let t = DEFAULT_TENANT;
    match cli.command {
        Command::Serve { listen, hooks_listen, admin_token_file } => {
            let admin_token_hash = match admin_token_file {
                Some(f) => Some(db::hash(std::fs::read_to_string(&f)?.trim())),
                None => None,
            };
            let app = Arc::new(App {
                db,
                links: link::Links::default(),
                hook_limits: hooks::Limits::default(),
                admin_token_hash,
            });
            serve(app, listen, hooks_listen).await
        }
        Command::Node { command: NodeCommand::Add { name } } => {
            let token = db::enrol(&db, t, &name).await?;
            println!("{token}");
            eprintln!("Node {name} enrolled. Put the token above in the node's token file; it isn't shown again.");
            Ok(())
        }
        Command::Node { command: NodeCommand::List } => {
            for n in db::nodes(&db, t).await? {
                let seen = n.last_seen_at.and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok());
                println!(
                    "{:<20} {:<16} {:<10} {}",
                    n.name,
                    n.machine.unwrap_or_default(),
                    n.version.unwrap_or_default(),
                    seen.map(|s| format!("last seen {s}")).unwrap_or_else(|| "never linked".into())
                );
            }
            Ok(())
        }
        Command::Node { command: NodeCommand::Remove { name } } => {
            anyhow::ensure!(db::remove_node(&db, t, &name).await?, "no node called {name}");
            println!("Removed {name}; its token no longer works.");
            Ok(())
        }
        Command::Endpoint { command: EndpointCommand::Add { label, channel } } => {
            let (e, secret) = db::add_endpoint(&db, t, &label, &channel).await?;
            println!("/hooks/{}/{secret}", e.id);
            eprintln!(
                "Endpoint {label} posts to channel {channel}. The path above holds its secret and isn't shown again."
            );
            Ok(())
        }
        Command::Endpoint { command: EndpointCommand::List } => {
            for e in db::endpoints(&db, t).await? {
                println!("{:<18} {:<20} {:<12} {} events", e.id, e.label, e.channel, e.event_count);
            }
            Ok(())
        }
        Command::Subscribe { channel, target, subject } => {
            anyhow::ensure!(route::split_target(&target).is_some(), "{target} isn't <tab>@<node>");
            db::subscribe(&db, t, &Subscription { channel, subject, target }).await
        }
        Command::Unsubscribe { channel, target, subject } => {
            anyhow::ensure!(
                db::unsubscribe(&db, t, &Subscription { channel, subject, target }).await?,
                "no such subscription"
            );
            Ok(())
        }
        Command::Acl { command } => match command {
            AclCommand::List => {
                for r in acl::rules(&db, t).await? {
                    println!(
                        "{:>4}  #{:<4} {:<5}  {} -> {}",
                        r.position,
                        r.id,
                        format!("{:?}", r.action).to_lowercase(),
                        r.from,
                        r.to
                    );
                }
                println!("default: {:?}", acl::default(&db, t).await?);
                Ok(())
            }
            AclCommand::Add { from, to, action, position } => {
                let id = acl::add(&db, t, &from, &to, acl::Action::parse(&action)?, position).await?;
                println!("Rule #{id} added.");
                Ok(())
            }
            AclCommand::Remove { id } => {
                anyhow::ensure!(acl::remove(&db, t, id).await?, "no rule #{id}");
                Ok(())
            }
            AclCommand::Default { action } => acl::set_default(&db, t, acl::Action::parse(&action)?).await,
            AclCommand::Check { from, to } => {
                println!("{}", if acl::allowed(&db, t, &from, &to).await? { "allowed" } else { "denied" });
                Ok(())
            }
        },
        Command::Fallback { channel, target, none } => {
            anyhow::ensure!(none != target.is_some(), "give a target or --none");
            if let Some(t) = &target {
                anyhow::ensure!(route::split_target(t).is_some(), "{t} isn't <tab>@<node>");
            }
            db::set_fallback(&db, t, &channel, target.as_deref()).await
        }
    }
}

async fn serve(app: Arc<App>, listen: SocketAddr, hooks_listen: SocketAddr) -> anyhow::Result<()> {
    let api = Router::new()
        .route("/healthz", get(api::healthz))
        .route("/v1/link", get(link::upgrade))
        .route("/v1/fleet", get(api::get_fleet))
        .route("/v1/events", get(api::get_events))
        .route("/v1/subscriptions", get(api::get_subscriptions))
        .route("/v1/endpoints", get(api::get_endpoints))
        .with_state(app.clone());
    let hooks = hooks::router(app);
    let api_listener = tokio::net::TcpListener::bind(listen).await?;
    let hooks_listener = tokio::net::TcpListener::bind(hooks_listen).await?;
    tracing::info!(%listen, %hooks_listen, "fleetlayer-control listening");
    tokio::try_join!(async { axum::serve(api_listener, api).await.map_err(anyhow::Error::from) }, async {
        axum::serve(hooks_listener, hooks).await.map_err(anyhow::Error::from)
    },)?;
    Ok(())
}
