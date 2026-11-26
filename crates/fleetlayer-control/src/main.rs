//! fleetlayer-control: the control plane. It knows every enrolled node and the sessions on it, carries messages
//! between sessions on different nodes, and routes events (webhooks, plugins' events) to the sessions subscribed to
//! their channel. It never holds credentials: node tokens and webhook secrets are stored only as hashes, and secrets
//! of the user's only ever pass through sealed to a node's key.
//!
//! Self-hosted: one binary and a SQLite file. The API and node links listen on one port; webhooks on another, so only
//! that one needs to face the internet.

mod acl;
mod api;
mod authz;
mod db;
mod hooks;
mod link;
mod policy;
mod route;
mod streams;
mod summaries;

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
    /// Delete one session's summaries as one node published them (it summarised a session that isn't its own).
    SummariesDelete {
        #[arg(long)]
        node: String,
        #[arg(long)]
        session: String,
    },
    /// The latest connections to tools shared by nodes (the audit log).
    Tools {
        #[arg(long, default_value_t = 30)]
        limit: i64,
    },
    /// API tokens for programs that read the control plane.
    Token {
        #[command(subcommand)]
        command: TokenCommand,
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
enum TokenCommand {
    /// Add a token and print it (shown once). `read`: every GET and catch-ups; `admin`: everything.
    Add {
        name: String,
        #[arg(long, default_value = "read")]
        scope: String,
    },
    List,
    Remove {
        name: String,
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
    /// Make the rules and default match a YAML file exactly (`default:` and `rules:` of `{from, to, action}` in
    /// order), printing what changed. Running it again changes nothing.
    Apply {
        file: PathBuf,
        /// Print what would change without changing it.
        #[arg(long)]
        check: bool,
    },
    /// The decision for one request, and the rules behind it. Uses the stored policy, or --policy <file>.
    Explain {
        #[arg(long)]
        from: String,
        #[arg(long)]
        can: String,
        #[arg(long)]
        to: String,
        /// Treat it as a reply to a message `to` sent `from` within 24 hours.
        #[arg(long)]
        reply: bool,
        #[arg(long)]
        policy: Option<PathBuf>,
    },
    /// Check a file of expected decisions (`[{from, can, to, expect: allow|deny}]`) against a policy file, offline.
    /// Exits non-zero on any mismatch: run it in CI.
    Test {
        cases: PathBuf,
        #[arg(long)]
        policy: PathBuf,
    },
    /// Suggest the smallest `only` rules that allow what each subject actually did (messages and tool connections).
    Suggest {
        /// How far back to look, in days.
        #[arg(long, default_value_t = 7)]
        days: i64,
        /// One rule per session rather than per node.
        #[arg(long)]
        per_session: bool,
    },
    /// Print the stored version 2 policy.
    Show,
    /// Remove the version 2 policy, so the version 1 rules decide again.
    Unapply,
    /// Recent refusals and what audit rules would have refused.
    Log {
        #[arg(long, default_value_t = 30)]
        limit: i64,
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
    pub streams: streams::Streams,
    pub authz: authz::Authz,
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
                streams: streams::Streams::default(),
                authz: authz::Authz::default(),
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
        Command::SummariesDelete { node, session } => {
            let n = summaries::delete(&db, t, &node, &session).await?;
            println!("Deleted {n} records of {session} published by {node}.");
            Ok(())
        }
        Command::Tools { limit } => {
            for c in streams::recent(&db, t, limit).await? {
                let at = c["openedAt"].as_i64().and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok());
                println!(
                    "{}  {} -> {}@{}  {} B in, {} B out  {}",
                    at.map(|t| t.strftime("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default(),
                    c["caller"].as_str().unwrap_or("?"),
                    c["tool"].as_str().unwrap_or("?"),
                    c["node"].as_str().unwrap_or("?"),
                    c["bytesToTool"],
                    c["bytesFromTool"],
                    c["outcome"].as_str().unwrap_or("open")
                );
            }
            Ok(())
        }
        Command::Token { command } => match command {
            TokenCommand::Add { name, scope } => {
                println!("{}", db::add_token(&db, t, &name, &scope).await?);
                eprintln!("Token {name} ({scope}) added. It isn't shown again.");
                Ok(())
            }
            TokenCommand::List => {
                for (name, scope, created) in db::tokens(&db, t).await? {
                    let at = jiff::Timestamp::from_millisecond(created).map(|t| t.to_string()).unwrap_or_default();
                    println!("{name:<24} {scope:<6} {at}");
                }
                Ok(())
            }
            TokenCommand::Remove { name } => {
                anyhow::ensure!(db::remove_token(&db, t, &name).await?, "no token {name}");
                Ok(())
            }
        },
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
            AclCommand::Apply { file, check } if is_v2(&file) => {
                let text = std::fs::read_to_string(&file)?;
                policy::Compiled::parse(&text).map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
                let old = authz::stored(&db, t).await?.unwrap_or_default();
                if old == text {
                    println!("The access policy already matches {}.", file.display());
                    return Ok(());
                }
                let (old_lines, new_lines): (Vec<&str>, Vec<&str>) = (old.lines().collect(), text.lines().collect());
                for l in old_lines.iter().filter(|l| !new_lines.contains(l)) {
                    println!("- {l}");
                }
                for l in new_lines.iter().filter(|l| !old_lines.contains(l)) {
                    println!("+ {l}");
                }
                if check {
                    println!("Would apply {} (version 2).", file.display());
                } else {
                    authz::store(&db, t, &text).await?;
                    println!("Applied {} (version 2). Version 1 rules no longer decide anything.", file.display());
                }
                Ok(())
            }
            AclCommand::Apply { file, check } => {
                let text = std::fs::read_to_string(&file)?;
                let f: acl::File =
                    yaml_serde::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
                let changes = acl::apply(&db, t, &f, check).await?;
                if changes.is_empty() {
                    println!("The access rules already match {}.", file.display());
                } else {
                    for c in &changes {
                        println!("{c}");
                    }
                    println!("{} {} change(s).", if check { "Would make" } else { "Made" }, changes.len());
                }
                Ok(())
            }
            AclCommand::Explain { from, can, to, reply, policy } => {
                let text = match policy {
                    Some(f) => std::fs::read_to_string(f)?,
                    None => authz::stored(&db, t)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("no version 2 policy is applied; pass --policy"))?,
                };
                let p = policy::Compiled::parse(&text)?;
                let d = p.decide(&from, &can, &to, reply)?;
                println!("{}: {from} {can} {to}", if d.allowed { "ALLOW" } else { "DENY" });
                for b in &d.because {
                    println!("  because {b}");
                }
                for a in &d.audit_would_deny {
                    println!("  an audit rule would refuse it: {a}");
                }
                if can == "message" {
                    let (per_hour, distinct) = p.limits_for(&from);
                    if per_hour.is_some() || distinct.is_some() {
                        let n = |v: Option<u32>| v.map_or("no limit".to_string(), |n| n.to_string());
                        println!("  limits: {} messages an hour, {} recipients an hour", n(per_hour), n(distinct));
                    }
                    if p.untrusted(&from) {
                        println!("  delivered as outside data (untrusted)");
                    }
                }
                Ok(())
            }
            AclCommand::Test { cases, policy } => {
                let p = policy::Compiled::parse(&std::fs::read_to_string(&policy)?)
                    .map_err(|e| anyhow::anyhow!("{}: {e}", policy.display()))?;
                let list: Vec<policy::Case> = yaml_serde::from_str(&std::fs::read_to_string(&cases)?)
                    .map_err(|e| anyhow::anyhow!("{}: {e}", cases.display()))?;
                let failures = policy::run_cases(&p, &list)?;
                for f in &failures {
                    println!("FAIL {f}");
                }
                println!("{} of {} cases pass.", list.len() - failures.len(), list.len());
                anyhow::ensure!(failures.is_empty(), "{} case(s) failed", failures.len());
                Ok(())
            }
            AclCommand::Suggest { days, per_session } => {
                use sqlx::Row;
                let since = db::now_ms() - days * 86_400_000;
                let mut seen: Vec<(String, String, String)> = vec![];
                for r in sqlx::query("SELECT from_subject, to_subject FROM message_log WHERE tenant = ? AND at >= ?")
                    .bind(t)
                    .bind(since)
                    .fetch_all(&db)
                    .await?
                {
                    seen.push((r.get(0), "message".into(), r.get(1)));
                }
                for r in sqlx::query(
                    "SELECT DISTINCT caller, tool, server_node FROM tool_connections WHERE tenant = ? AND opened_at >= ?
                     AND (outcome IS NULL OR outcome NOT LIKE 'refused%')",
                )
                .bind(t)
                .bind(since)
                .fetch_all(&db)
                .await?
                {
                    let (caller, tool, node): (String, String, String) = (r.get(0), r.get(1), r.get(2));
                    seen.push((caller, "use_tool".into(), format!("tool:{tool}@{node}")));
                }
                print!("{}", policy::suggest(&seen, !per_session));
                Ok(())
            }
            AclCommand::Unapply => {
                if authz::remove(&db, t).await? {
                    println!("Removed the version 2 policy; version 1 rules decide again.");
                } else {
                    println!("No version 2 policy was applied.");
                }
                Ok(())
            }
            AclCommand::Show => {
                match authz::stored(&db, t).await? {
                    Some(text) => print!("{text}"),
                    None => println!("No version 2 policy is applied; version 1 rules decide (acl list)."),
                }
                Ok(())
            }
            AclCommand::Log { limit } => {
                for e in db::recent_events(&db, t, 500)
                    .await?
                    .into_iter()
                    .filter(|e| e["channel"] == "access")
                    .take(limit as usize)
                {
                    let at = e["at"].as_i64().and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok());
                    println!(
                        "{}  {:<13} {}",
                        at.map(|t| t.strftime("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default(),
                        e["kind"].as_str().unwrap_or("?"),
                        e["summary"].as_str().unwrap_or("")
                    );
                }
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

/// Whether a policy file is version 2 (`version: 2`).
fn is_v2(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| yaml_serde::from_str::<serde_json::Value>(&t).ok())
        .is_some_and(|v| v["version"].as_i64() == Some(2))
}

async fn serve(app: Arc<App>, listen: SocketAddr, hooks_listen: SocketAddr) -> anyhow::Result<()> {
    let api = Router::new()
        .route("/healthz", get(api::healthz))
        .route("/v1/link", get(link::upgrade))
        .route("/v1/stream/{id}", get(streams::upgrade))
        .route("/v1/fleet", get(api::get_fleet))
        .route("/v1/events", get(api::get_events))
        .route("/v1/subscriptions", get(api::get_subscriptions))
        .route("/v1/endpoints", get(api::get_endpoints))
        .route("/v1/summaries", get(api::get_summaries))
        .route("/v1/sessions/{id}/catchup", axum::routing::post(api::post_catchup))
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
