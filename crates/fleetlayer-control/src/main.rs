//! fleetlayer-control: the control plane. It knows every enrolled node and the sessions on it, carries messages
//! between sessions on different nodes, and routes events (webhooks, plugins' events) to the sessions subscribed to
//! their channel. It never holds credentials: node tokens and webhook secrets are stored only as hashes, and secrets
//! of the user's only ever pass through sealed to a node's key.
//!
//! Self-hosted: one binary and a SQLite file. The API and node links listen on one port; webhooks on another, so only
//! that one needs to face the internet.

mod acl;
mod api;
mod approvals;
mod audit;
mod authz;
mod db;
mod directory;
mod hooks;
mod link;
mod pages;
mod people;
mod policy;
mod readings;
mod route;
mod signin;
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
    /// The SQLite database. Default: `./fleetlayer-control.db` if there is one (older setups), else
    /// `~/.local/state/fleetlayer/control/control.db`, where `fleetlayer up` puts it.
    #[arg(long, global = true, env = "FLEETLAYER_CONTROL_DB")]
    db: Option<PathBuf>,
    /// The tenant (its slug) that people, teams, invites and agents commands act on; the deployment's first tenant
    /// when left out.
    #[arg(long, global = true, env = "FLEETLAYER_TENANT")]
    tenant: Option<String>,
    /// Who is making the change, for the audit log (`admin`, or a person's handle).
    #[arg(long = "as", global = true, default_value = "admin")]
    actor: String,
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
        /// A file holding the audit log's HMAC key (kept outside the database). Without one the chain is plain SHA-256.
        #[arg(long, env = "FLEETLAYER_CONTROL_AUDIT_KEY_FILE")]
        audit_key_file: Option<PathBuf>,
        /// A directory outside the database for hourly audit anchors (another disk, a git checkout). Optional.
        #[arg(long, env = "FLEETLAYER_CONTROL_AUDIT_ANCHOR_DIR")]
        audit_anchor_dir: Option<PathBuf>,
    },
    /// Tenants: add, list, rename (an ID never changes; a slug and name can).
    Tenant {
        #[command(subcommand)]
        command: TenantCommand,
    },
    /// Every person in the deployment (the operator's view), and removing one from all of it.
    Person {
        #[command(subcommand)]
        command: PersonCommand,
    },
    /// The people here: list, change a role, remove.
    #[command(visible_alias = "people")]
    Member {
        #[command(subcommand)]
        command: MemberCommand,
    },
    /// The tenant's teams, which can nest.
    Team {
        #[command(subcommand)]
        command: TeamCommand,
    },
    /// Agents the tenant admits as members.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Invites by link to the tenant (also how a person who lost their passkeys gets back in).
    Invite {
        #[command(subcommand)]
        command: InviteCommand,
    },
    /// The audit log of access decisions.
    Audit {
        #[command(subcommand)]
        command: AuditCommand,
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
    /// Requests waiting for the person's approval (policy ask: rules).
    Approvals {
        #[command(subcommand)]
        command: ApprovalsCommand,
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
    /// The sign-in service's token (`docs/sso.md`): it reads memberships, redeems invites, asks checks and records
    /// sign-ins, for every tenant, and nothing else.
    Signin {
        #[command(subcommand)]
        command: SigninCommand,
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
enum ApprovalsCommand {
    /// Pending requests (--all: decided ones too).
    List {
        #[arg(long)]
        all: bool,
    },
    /// Approve a request: the same agent may do the same thing to the same target for its rule's time, or --for.
    Approve {
        id: String,
        #[arg(long = "for")]
        hold: Option<String>,
    },
    /// Refuse a request (it stands for an hour).
    Deny { id: String },
}

#[derive(Subcommand)]
enum AuditCommand {
    /// Check the chain (and anchors). Exits non-zero on any break.
    Verify {
        #[arg(long, env = "FLEETLAYER_CONTROL_AUDIT_KEY_FILE")]
        key_file: Option<PathBuf>,
        #[arg(long, env = "FLEETLAYER_CONTROL_AUDIT_ANCHOR_DIR")]
        anchors: Option<PathBuf>,
    },
    /// The newest decisions.
    Tail {
        #[arg(long, default_value_t = 30)]
        limit: i64,
        /// Only refusals.
        #[arg(long)]
        denied: bool,
    },
}

#[derive(Subcommand)]
enum SigninCommand {
    /// Make the sign-in token and print it (shown once). Making it again replaces it.
    Token {
        #[arg(long, default_value = "signin")]
        name: String,
    },
    /// The sign-in tokens there are.
    Tokens,
    /// Remove a sign-in token: the sign-in service using it stops working.
    Revoke { name: String },
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
    /// The decision for one request in a tenant (--tenant), and the rules behind it: membership first, then the
    /// tenant's stored policy, or --policy <file>. People (`person:<handle>`) are decided with their teams and role.
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
    /// A case about a person gives their `role` (admin or member) and `teams`.
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
    /// Enrol a node and print its token (shown once; enrolling again replaces it). Its agents are admitted to the
    /// tenant (`--tenant`) unless `--no-admit`.
    Add {
        name: String,
        /// Its owner: a person (ID or handle), `team:<name>` or `tenant`.
        #[arg(long)]
        owner: Option<String>,
        #[arg(long)]
        no_admit: bool,
    },
    List,
    Remove {
        name: String,
    },
    /// Set or transfer a node's owner: a person (ID or handle), `team:<name>`, `tenant`, or `none`.
    Owner {
        name: String,
        owner: String,
    },
}

#[derive(Subcommand)]
enum TenantCommand {
    Add {
        slug: String,
        #[arg(long)]
        name: Option<String>,
    },
    List,
    Rename {
        slug: String,
        new_slug: String,
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Subcommand)]
enum PersonCommand {
    List,
    /// Remove a person from every tenant and take their own nodes offline (team and tenant nodes stay).
    Remove {
        id: String,
    },
}

#[derive(Subcommand)]
enum MemberCommand {
    List,
    Role {
        handle: String,
        role: String,
    },
    /// Remove a member (their handle stays reserved; their own nodes' agents lose this tenant).
    Remove {
        handle: String,
    },
}

#[derive(Subcommand)]
enum TeamCommand {
    Add {
        name: String,
        #[arg(long)]
        parent: Option<String>,
    },
    Remove {
        name: String,
    },
    List,
    /// Add someone to a team, or with --remove take them out.
    Member {
        team: String,
        handle: String,
        #[arg(long)]
        maintainer: bool,
        #[arg(long)]
        remove: bool,
    },
}

#[derive(Subcommand)]
enum AgentCommand {
    /// Admit an agent, or a pattern of them (`finances@laptop`, `*@laptop`).
    Admit {
        pattern: String,
    },
    Remove {
        pattern: String,
    },
    /// The admitted patterns, and whether each may also be a member elsewhere (never where).
    List,
}

#[derive(Subcommand)]
enum InviteCommand {
    /// Make an invite and print its code (shown once).
    Add {
        #[arg(long)]
        handle: String,
        #[arg(long, default_value = "member")]
        role: String,
        #[arg(long = "team")]
        teams: Vec<String>,
        /// An existing person (recovery, or someone who already has an account).
        #[arg(long)]
        person: Option<String>,
    },
    List,
    /// Revoke the open invites for a handle.
    Revoke {
        handle: String,
    },
    /// Use an invite from the command line (until a sign-in service does it).
    Redeem {
        code: String,
        /// Someone new, with this display name.
        #[arg(long = "new", conflicts_with = "person")]
        new: Option<String>,
        /// A person who already has an account.
        #[arg(long)]
        person: Option<String>,
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
        /// A file holding the secret GitHub signs deliveries with (the webhook's "Secret"): requests whose
        /// X-Hub-Signature-256 doesn't verify are refused.
        #[arg(long)]
        github_secret_file: Option<PathBuf>,
        /// Deliver every action of a GitHub workflow run (requested, in_progress), not only completed ones.
        #[arg(long)]
        all_actions: bool,
    },
    /// Set an existing endpoint's GitHub signing secret from a file, or remove it with --clear.
    Secret {
        id: String,
        #[arg(long, conflicts_with = "clear", required_unless_present = "clear")]
        github_secret_file: Option<PathBuf>,
        #[arg(long)]
        clear: bool,
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
    pub audit: audit::Audit,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr) // stdout carries tokens and invite codes that scripts capture
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FLEETLAYER_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    // `acl test` reads only its files: it works anywhere, with no database (CI runs it in a clean checkout).
    let offline = matches!(&cli.command, Command::Acl { command: AclCommand::Test { .. } });
    let db = match &cli.db {
        Some(p) => db::open(p).await?,
        None if offline => db::open_memory().await?,
        None => {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
            let path = default_db(std::path::Path::new("fleetlayer-control.db"), &home);
            // An admin command run from the wrong folder mustn't quietly start an empty control plane.
            anyhow::ensure!(
                path.exists() || matches!(cli.command, Command::Serve { .. }),
                "no control plane database at {} (pass --db, or set FLEETLAYER_CONTROL_DB)",
                path.display()
            );
            db::open(&path).await?
        }
    };
    let t = DEFAULT_TENANT;
    match cli.command {
        Command::Serve { listen, hooks_listen, admin_token_file, audit_key_file, audit_anchor_dir } => {
            let audit_key = read_key(audit_key_file.as_deref())?;
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
                audit: audit::Audit::new(audit_key.clone()),
            });
            if audit_key.is_none() {
                tracing::warn!("no --audit-key-file: the audit log is chained with plain SHA-256, not an HMAC");
            }
            tokio::spawn(audit::run(app.clone(), audit_anchor_dir));
            tokio::spawn(link::sync_approved(app.clone()));
            serve(app, listen, hooks_listen).await
        }
        Command::Node { command: NodeCommand::Add { name, owner, no_admit } } => {
            let tenant = people::tenant_or_first(&db, cli.tenant.as_deref()).await?;
            let owner = match &owner {
                Some(o) => Some(people::resolve_owner(&db, &tenant.id, o).await?),
                None => None,
            };
            let token = db::enrol(&db, t, &name).await?;
            people::node_enrolled(&db, &name, &cli.actor).await?;
            if let Some(o) = &owner {
                people::node_owner(&db, &name, Some(o), &cli.actor, &tenant.id).await?;
            }
            if !no_admit && owner.as_ref().and_then(|o| o.tenant()).is_none() {
                people::agent_admit(&db, &tenant.id, &format!("*@{name}"), &cli.actor).await?;
            }
            println!("{token}");
            eprintln!("Node {name} enrolled. Put the token above in the node's token file; it isn't shown again.");
            Ok(())
        }
        Command::Node { command: NodeCommand::List } => {
            for n in db::nodes(&db, t).await? {
                let seen = n.last_seen_at.and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok());
                let owner = people::owner_of_node(&db, &n.name).await?.map(|o| o.stored());
                println!(
                    "{:<20} {:<16} {:<10} {:<28} {}",
                    n.name,
                    n.machine.unwrap_or_default(),
                    n.version.unwrap_or_default(),
                    owner.unwrap_or_else(|| "no owner".into()),
                    seen.map(|s| format!("last seen {s}")).unwrap_or_else(|| "never linked".into())
                );
            }
            Ok(())
        }
        Command::Node { command: NodeCommand::Owner { name, owner } } => {
            let tenant = people::tenant_or_first(&db, cli.tenant.as_deref()).await?;
            let owner = match owner.as_str() {
                "none" => None,
                o => Some(people::resolve_owner(&db, &tenant.id, o).await?),
            };
            people::node_owner(&db, &name, owner.as_ref(), &cli.actor, &tenant.id).await?;
            println!("{name} now belongs to {}.", owner.map(|o| o.stored()).unwrap_or_else(|| "nobody".into()));
            Ok(())
        }
        Command::Tenant { command } => match command {
            TenantCommand::Add { slug, name } => {
                let tn = people::tenant_add(&db, &slug, name.as_deref().unwrap_or(&slug), &cli.actor).await?;
                println!("Tenant {} added (ID {}).", tn.slug, tn.id);
                Ok(())
            }
            TenantCommand::List => {
                for tn in people::tenants(&db).await? {
                    println!("{:<20} {:<22} {}", tn.slug, tn.id, tn.name);
                }
                Ok(())
            }
            TenantCommand::Rename { slug, new_slug, name } => {
                people::tenant_rename(&db, &slug, &new_slug, name.as_deref(), &cli.actor).await?;
                println!("Renamed {slug} to {new_slug}; its ID and everything in it are unchanged.");
                Ok(())
            }
        },
        Command::Person { command } => match command {
            PersonCommand::List => {
                for p in people::people(&db).await? {
                    println!("{:<24} {:<30} {} tenants", p.id, p.display_name, p.tenants);
                }
                Ok(())
            }
            PersonCommand::Remove { id } => {
                let nodes = people::person_remove(&db, &id, &cli.actor).await?;
                println!(
                    "Removed {id} from every tenant. Nodes taken offline: {}.",
                    if nodes.is_empty() { "none".into() } else { nodes.join(", ") }
                );
                Ok(())
            }
        },
        Command::Member { command } => {
            let tn = people::tenant_or_first(&db, cli.tenant.as_deref()).await?.id;
            match command {
                MemberCommand::List => {
                    for mut m in people::members(&db, &tn).await? {
                        m.teams.retain(|t| t != people::EVERYONE);
                        println!(
                            "{:<20} {:<7} {:<24} {:<24} {}",
                            m.handle,
                            m.role,
                            m.person,
                            m.display_name,
                            m.teams.join(",")
                        );
                    }
                }
                MemberCommand::Role { handle, role } => {
                    people::member_role(&db, &tn, &handle, &role, &cli.actor).await?
                }
                MemberCommand::Remove { handle } => people::member_remove(&db, &tn, &handle, &cli.actor).await?,
            }
            Ok(())
        }
        Command::Team { command } => {
            let tn = people::tenant_or_first(&db, cli.tenant.as_deref()).await?.id;
            match command {
                TeamCommand::Add { name, parent } => {
                    people::team_add(&db, &tn, &name, parent.as_deref(), &cli.actor).await?
                }
                TeamCommand::Remove { name } => people::team_remove(&db, &tn, &name, &cli.actor).await?,
                TeamCommand::List => {
                    for team in people::teams(&db, &tn).await? {
                        let members: Vec<String> = team
                            .members
                            .iter()
                            .map(|(h, r)| if r == "maintainer" { format!("{h} (maintainer)") } else { h.clone() })
                            .collect();
                        println!("{:<20} {:<20} {}", team.name, team.parent.unwrap_or_default(), members.join(", "));
                    }
                }
                TeamCommand::Member { team, handle, maintainer, remove } => {
                    people::team_member(&db, &tn, &team, &handle, !remove, maintainer, &cli.actor).await?
                }
            }
            Ok(())
        }
        Command::Agent { command } => {
            let tn = people::tenant_or_first(&db, cli.tenant.as_deref()).await?.id;
            match command {
                AgentCommand::Admit { pattern } => people::agent_admit(&db, &tn, &pattern, &cli.actor).await?,
                AgentCommand::Remove { pattern } => people::agent_remove(&db, &tn, &pattern, &cli.actor).await?,
                AgentCommand::List => {
                    for a in people::agents(&db, &tn).await? {
                        let note = if a.elsewhere { "also admitted by another tenant" } else { "" };
                        println!("{:<30} {:<16} {note}", a.pattern, a.added_by);
                    }
                }
            }
            Ok(())
        }
        Command::Invite { command } => {
            let tn = people::tenant_or_first(&db, cli.tenant.as_deref()).await?.id;
            match command {
                InviteCommand::Add { handle, role, teams, person } => {
                    let person = match person {
                        Some(p) => Some(people::resolve_person(&db, &tn, &p).await?),
                        None => None,
                    };
                    let i =
                        people::NewInvite { handle: &handle, role: &role, teams: &teams, person: person.as_deref() };
                    let code = people::invite(&db, &tn, i, &cli.actor).await?;
                    println!("{code}");
                    eprintln!(
                        "An invite for {handle}, valid {} days. The code above isn't shown again.",
                        people::INVITE_DAYS
                    );
                }
                InviteCommand::List => {
                    for i in people::invites(&db, &tn).await? {
                        let until =
                            jiff::Timestamp::from_millisecond(i.expires_at).map(|t| t.to_string()).unwrap_or_default();
                        let what = if i.person.is_some() { "for an existing person" } else { "" };
                        println!("{:<20} {:<7} until {until} {what}", i.handle, i.role);
                    }
                }
                InviteCommand::Revoke { handle } => people::invite_revoke(&db, &tn, &handle, &cli.actor).await?,
                InviteCommand::Redeem { code, new, person } => {
                    let who = match (&new, &person) {
                        (Some(n), None) => people::Redeemer::New { display_name: n },
                        (None, Some(p)) => people::Redeemer::Existing { person: p },
                        _ => anyhow::bail!("say who uses it: --new \"<display name>\" or --person <id>"),
                    };
                    let r = people::invite_redeem(&db, &code, who).await?;
                    println!(
                        "{} is now {} (person {}).",
                        r.handle,
                        if r.recovery { "recovered" } else { "a member" },
                        r.person
                    );
                }
            }
            Ok(())
        }
        Command::Node { command: NodeCommand::Remove { name } } => {
            anyhow::ensure!(db::remove_node(&db, t, &name).await?, "no node called {name}");
            println!("Removed {name}; its token no longer works.");
            Ok(())
        }
        Command::Endpoint { command: EndpointCommand::Add { label, channel, github_secret_file, all_actions } } => {
            let github = github_secret_file.as_deref().map(read_github_secret).transpose()?;
            let (e, secret) = db::add_endpoint(&db, t, &label, &channel, github.as_deref(), all_actions).await?;
            println!("/hooks/{}/{secret}", e.id);
            eprintln!(
                "Endpoint {label} posts to channel {channel}. The path above holds its secret and isn't shown again."
            );
            Ok(())
        }
        Command::Endpoint { command: EndpointCommand::Secret { id, github_secret_file, clear } } => {
            let github =
                if clear { None } else { github_secret_file.as_deref().map(read_github_secret).transpose()? };
            anyhow::ensure!(db::set_github_secret(&db, t, &id, github.as_deref()).await?, "no endpoint {id}");
            println!(
                "{}",
                if clear { "GitHub signatures are no longer checked." } else { "GitHub signatures are checked." }
            );
            Ok(())
        }
        Command::Endpoint { command: EndpointCommand::List } => {
            for e in db::endpoints(&db, t).await? {
                let signed = if e.github_secret.is_some() { "  signed" } else { "" };
                println!("{:<18} {:<20} {:<12} {} events{signed}", e.id, e.label, e.channel, e.event_count);
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
        Command::Approvals { command } => {
            let pt = people::tenant_or_first(&db, cli.tenant.as_deref()).await?.id;
            match command {
                ApprovalsCommand::List { all } => {
                    for a in approvals::list(&db, &pt, all).await? {
                        println!(
                            "{}  {:<9} {} {} {}",
                            a["id"].as_str().unwrap_or("?"),
                            a["status"].as_str().unwrap_or("?"),
                            a["from"].as_str().unwrap_or("?"),
                            a["verb"].as_str().unwrap_or("?"),
                            a["to"].as_str().unwrap_or("?")
                        );
                    }
                    Ok(())
                }
                ApprovalsCommand::Approve { id, hold } => {
                    let hold = hold.map(|h| policy::parse_duration_ms(&h)).transpose()?;
                    let d = approvals::decide(&db, &pt, &id, true, hold)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("no pending request {id}"))?;
                    println!("Approved {id}: {} may {} {}.", d.subject, d.verb, d.target);
                    Ok(())
                }
                ApprovalsCommand::Deny { id } => {
                    let d = approvals::decide(&db, &pt, &id, false, None)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("no pending request {id}"))?;
                    println!("Refused {id}: {} may not {} {}.", d.subject, d.verb, d.target);
                    Ok(())
                }
            }
        }
        Command::Audit { command: AuditCommand::Verify { key_file, anchors } } => {
            let a = audit::Audit::new(read_key(key_file.as_deref())?);
            let (report, ok) = audit::verify(&a, &db, anchors.as_deref()).await?;
            for l in &report {
                println!("{l}");
            }
            anyhow::ensure!(ok, "the audit log failed verification");
            Ok(())
        }
        Command::Audit { command: AuditCommand::Tail { limit, denied } } => {
            use sqlx::Row;
            let rows = sqlx::query(
                "SELECT seq, at, decision, subject, verb, target, reason, count FROM audit_log
                 WHERE (? = 0 OR decision = 'denied') ORDER BY seq DESC LIMIT ?",
            )
            .bind(denied as i64)
            .bind(limit)
            .fetch_all(&db)
            .await?;
            for r in rows.iter().rev() {
                let at = jiff::Timestamp::from_millisecond(r.get(1))
                    .map(|t| t.strftime("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_default();
                let count: i64 = r.get(7);
                println!(
                    "{:>6} {at} {:<7} {} {} {}{}  ({})",
                    r.get::<i64, _>(0),
                    r.get::<String, _>(2),
                    r.get::<String, _>(3),
                    r.get::<String, _>(4),
                    r.get::<String, _>(5),
                    if count > 1 { format!(" ×{count}") } else { String::new() },
                    r.get::<String, _>(6)
                );
            }
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
        Command::Signin { command } => match command {
            SigninCommand::Token { name } => {
                println!("{}", signin::token_add(&db, &name).await?);
                eprintln!(
                    "Sign-in token {name} made. Give it to the sign-in service (its token file); it isn't shown again."
                );
                Ok(())
            }
            SigninCommand::Tokens => {
                for (name, created) in signin::tokens(&db).await? {
                    let at = jiff::Timestamp::from_millisecond(created).map(|t| t.to_string()).unwrap_or_default();
                    println!("{name:<24} {at}");
                }
                Ok(())
            }
            SigninCommand::Revoke { name } => {
                anyhow::ensure!(signin::token_remove(&db, &name).await?, "no sign-in token {name}");
                println!("Sign-in token {name} revoked.");
                Ok(())
            }
        },
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
        Command::Acl { command } => {
            let pt = people::tenant_or_first(&db, cli.tenant.as_deref()).await?.id;
            match command {
                AclCommand::List => {
                    for r in acl::rules(&db, &pt).await? {
                        println!(
                            "{:>4}  #{:<4} {:<5}  {} -> {}",
                            r.position,
                            r.id,
                            format!("{:?}", r.action).to_lowercase(),
                            r.from,
                            r.to
                        );
                    }
                    println!("default: {:?}", acl::default(&db, &pt).await?);
                    Ok(())
                }
                AclCommand::Add { from, to, action, position } => {
                    let id = acl::add(&db, &pt, &from, &to, acl::Action::parse(&action)?, position).await?;
                    println!("Rule #{id} added.");
                    Ok(())
                }
                AclCommand::Remove { id } => {
                    anyhow::ensure!(acl::remove(&db, &pt, id).await?, "no rule #{id}");
                    Ok(())
                }
                AclCommand::Apply { file, check } if is_v2(&file) => {
                    let text = std::fs::read_to_string(&file)?;
                    policy::Compiled::parse(&text).map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
                    let old = authz::stored(&db, &pt).await?.unwrap_or_default();
                    if old == text {
                        println!("The access policy already matches {}.", file.display());
                        return Ok(());
                    }
                    let (old_lines, new_lines): (Vec<&str>, Vec<&str>) =
                        (old.lines().collect(), text.lines().collect());
                    for l in old_lines.iter().filter(|l| !new_lines.contains(l)) {
                        println!("- {l}");
                    }
                    for l in new_lines.iter().filter(|l| !old_lines.contains(l)) {
                        println!("+ {l}");
                    }
                    if check {
                        println!("Would apply {} (version 2).", file.display());
                    } else {
                        authz::store(&db, &pt, &text).await?;
                        println!("Applied {} (version 2). Version 1 rules no longer decide anything.", file.display());
                    }
                    Ok(())
                }
                AclCommand::Apply { file, check } => {
                    let text = std::fs::read_to_string(&file)?;
                    let f: acl::File =
                        yaml_serde::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
                    let changes = acl::apply(&db, &pt, &f, check).await?;
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
                        None => match authz::effective(&db, &pt).await? {
                            Some((text, builtin)) => {
                                if builtin {
                                    println!("(no policy is applied: the built-in default decides)");
                                }
                                text
                            }
                            None => anyhow::bail!("version 1 rules decide here (acl list); pass --policy"),
                        },
                    };
                    let p = policy::Compiled::parse(&text)?;
                    // Decided in the chosen tenant, with who belongs to it (membership first, people's teams and roles).
                    let fleet = directory::Fleet::load(&db).await?;
                    let tenant = fleet.tenant(&pt).ok_or_else(|| anyhow::anyhow!("no tenant {pt}"))?;
                    let scope = Some(policy::Scope { fleet: &fleet, tenant });
                    let d = p.decide_in(scope, &from, &can, &to, reply)?;
                    println!("{}: {from} {can} {to}", if d.allowed { "ALLOW" } else { "DENY" });
                    for b in &d.because {
                        println!("  because {b}");
                    }
                    for a in &d.audit_would_deny {
                        println!("  an audit rule would refuse it: {a}");
                    }
                    if d.allowed
                        && let Some(hold) = p.ask_for_in(scope, &from, &can, &to)
                    {
                        println!(
                            "  but it needs its person's approval first (an approval holds {} minutes)",
                            hold / 60_000
                        );
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
                    for r in
                        sqlx::query("SELECT from_subject, to_subject FROM message_log WHERE tenant = ? AND at >= ?")
                            .bind(t)
                            .bind(since)
                            .fetch_all(&db)
                            .await?
                    {
                        seen.push((r.get(0), "message".into(), r.get(1)));
                    }
                    for r in sqlx::query(
                    "SELECT DISTINCT caller, tool, server_node FROM tool_connections WHERE tenant = ? AND opened_at >= ?
                     AND bytes_from_tool > 0",
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
                    if authz::remove(&db, &pt).await? {
                        match authz::effective(&db, &pt).await? {
                            None => println!("Removed the version 2 policy; version 1 rules decide again."),
                            Some(_) => println!("Removed the version 2 policy; the built-in default decides now."),
                        }
                    } else {
                        println!("No version 2 policy was applied.");
                    }
                    Ok(())
                }
                AclCommand::Show => {
                    match authz::effective(&db, &pt).await? {
                        Some((text, _)) => print!("{text}"),
                        None => println!("No version 2 policy is applied; version 1 rules decide (acl list)."),
                    }
                    Ok(())
                }
                AclCommand::Log { limit } => {
                    for e in db::recent_events(&db, &pt, 500)
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
                AclCommand::Default { action } => acl::set_default(&db, &pt, acl::Action::parse(&action)?).await,
                AclCommand::Check { from, to } => {
                    println!("{}", if acl::allowed(&db, &pt, &from, &to).await? { "allowed" } else { "denied" });
                    Ok(())
                }
            }
        }
        Command::Fallback { channel, target, none } => {
            anyhow::ensure!(none != target.is_some(), "give a target or --none");
            if let Some(t) = &target {
                anyhow::ensure!(route::split_target(t).is_some(), "{t} isn't <tab>@<node>");
            }
            db::set_fallback(&db, t, &channel, target.as_deref()).await
        }
    }
}

fn read_key(path: Option<&std::path::Path>) -> anyhow::Result<Option<Vec<u8>>> {
    match path {
        Some(p) => {
            let k = std::fs::read_to_string(p).map_err(|e| anyhow::anyhow!("reading {}: {e}", p.display()))?;
            anyhow::ensure!(k.trim().len() >= 32, "{}: the audit key should be at least 32 characters", p.display());
            Ok(Some(k.trim().as_bytes().to_vec()))
        }
        None => Ok(None),
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
        .route("/v1/approvals", get(api::get_approvals))
        .route("/v1/messages", axum::routing::post(api::post_message))
        .route("/v1/approvals/{id}", axum::routing::post(api::post_approval))
        .route("/v1/signin/person", axum::routing::post(signin::post_person))
        .route("/v1/signin/invite_redeem", axum::routing::post(signin::post_invite_redeem))
        .route("/v1/signin/check", axum::routing::post(signin::post_check))
        .route("/v1/signin/event", axum::routing::post(signin::post_event))
        .route("/v1/signin/join", axum::routing::post(signin::post_join))
        .route("/v1/signin/admin/view", axum::routing::post(signin::post_admin_view))
        .route("/v1/signin/admin/{action}", axum::routing::post(signin::post_admin_action))
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

/// The database to use when none is named: `./fleetlayer-control.db` when it exists, else the one `fleetlayer up`
/// makes.
fn default_db(cwd_db: &std::path::Path, home: &std::path::Path) -> PathBuf {
    if cwd_db.exists() { cwd_db.to_path_buf() } else { home.join(".local/state/fleetlayer/control/control.db") }
}

/// A GitHub signing secret from a file: its text, trimmed, not empty.
fn read_github_secret(path: &std::path::Path) -> anyhow::Result<String> {
    let s = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    let s = s.trim().to_string();
    anyhow::ensure!(!s.is_empty(), "{} is empty", path.display());
    Ok(s)
}

#[cfg(test)]
mod default_db_tests {
    #[test]
    fn an_existing_local_database_wins_else_the_one_up_makes() {
        let d = tempfile::tempdir().unwrap();
        let local = d.path().join("fleetlayer-control.db");
        assert_eq!(super::default_db(&local, d.path()), d.path().join(".local/state/fleetlayer/control/control.db"));
        std::fs::write(&local, "").unwrap();
        assert_eq!(super::default_db(&local, d.path()), local);
    }
}
