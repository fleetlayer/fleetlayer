//! fleetlayer: the command line. It talks to this machine's node over its local socket.

mod pages;
#[cfg(test)]
mod skill_commands;
mod up;

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
    /// Start everything on this machine as user services: the control plane (or --join one elsewhere), the node with
    /// the Claude sign-in hub and pages, and the gateway (http://<name>.localhost:7680 and a status page).
    Up(up::UpArgs),
    /// Stop what `up` started. Every file stays.
    Down,
    /// Every service the gateway serves: its address and where it really runs.
    Services {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        rest: Vec<String>,
    },
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
    /// The newest readings of these kinds (`claude.usage`, `claude.token_health`...) from every node this one may
    /// read, one line each; --json for the whole readings.
    Readings {
        #[arg(required = true)]
        kinds: Vec<String>,
    },
    /// Subscribe this session (or --target) to a channel's events, or to one subject on it.
    Subscribe(SubscribeArgs),
    /// Undo a subscription.
    Unsubscribe(SubscribeArgs),
    /// Every subscription in the fleet.
    Subscriptions,
    /// Search session summaries across the fleet, or show one session's timeline with --session.
    Summaries {
        /// Words to search for.
        query: Vec<String>,
        /// A session ID: its timeline, the newest --limit records, oldest first.
        #[arg(long)]
        session: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: i64,
        /// Show each summary's paragraph too.
        #[arg(long)]
        long: bool,
        /// Sessions whose summaries mention this exact text (an ID such as sc-1234), newest mention each.
        #[arg(long)]
        mentions: Option<String>,
        /// Import claude-summarizer's history from this directory (once per machine).
        #[arg(long)]
        import: Option<String>,
    },
    /// Tools this node shares: opt in to offering one on any network for a while, or end the opt-in.
    Tools {
        #[command(subcommand)]
        command: ToolsCommand,
    },
    /// Use a tool another node shares, as this session's MCP server (stdio): `fleetlayer mcp browser@laptop`.
    Mcp {
        /// `<tool>@<node>`.
        target: String,
        /// Why this session uses it, in one line, when its access rules ask for a reason. Default: $FLEETLAYER_WHY.
        #[arg(long, env = "FLEETLAYER_WHY")]
        why: Option<String>,
    },
    /// Watch or drive a session's terminal anywhere in the fleet: `fleetlayer attach planning@laptop`. Ctrl-] leaves.
    Attach {
        /// `<tab>@<node>`, or `<tab>` on this node.
        target: String,
        /// Only watch: nothing you type reaches it (needs `read`; driving it needs `attach`).
        #[arg(long)]
        observe: bool,
        /// Print the terminal stream's JSON lines instead of drawing them (for other programs).
        #[arg(long)]
        raw: bool,
        /// Why, in one line, when its access rules ask for a reason. Default: $FLEETLAYER_WHY.
        #[arg(long, env = "FLEETLAYER_WHY")]
        why: Option<String>,
    },
    /// Pages for people, hosted by a pages service anywhere in the fleet.
    Pages {
        #[command(subcommand)]
        command: PagesCommand,
    },
    /// Messages between sessions.
    Msg {
        #[command(subcommand)]
        command: Msg,
    },
    /// Start or resume agent sessions, here or on another node.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
}

#[derive(Subcommand)]
enum SessionCommand {
    /// Start a session in a new tab, without taking focus from anyone: `fleetlayer session start --on server --tab
    /// review --cwd /srv/app --prompt "…"`. Answers once the agent takes input. With --json:
    /// `{node, tab, address, sessionId, promptError}`.
    Start {
        /// The new tab's name; refused if a session is already running in a tab of that name.
        #[arg(long)]
        tab: String,
        /// The folder it works in (absolute, on that node; not the home folder).
        #[arg(long)]
        cwd: String,
        /// The node to start it on. Default: this one.
        #[arg(long)]
        on: Option<String>,
        /// The agent: claude (default) or codex. Its arguments (permission mode...) come from that node's config.
        #[arg(long)]
        harness: Option<String>,
        /// The first prompt, typed in once the agent takes input.
        #[arg(long)]
        prompt: Option<String>,
        /// Why, in one line, when the access rules ask for a reason. Default: $FLEETLAYER_WHY.
        #[arg(long, env = "FLEETLAYER_WHY")]
        why: Option<String>,
    },
    /// Resume a stored session in a new tab, in the folder it worked in. Finds the node from the session ID when
    /// --on is left out.
    Resume {
        session_id: String,
        #[arg(long)]
        on: Option<String>,
        /// The tab's name. Default: the session's old tab, else "resumed <id>".
        #[arg(long)]
        tab: Option<String>,
        /// A different folder than the one it worked in.
        #[arg(long)]
        cwd: Option<String>,
        /// A prompt to type in once it takes input.
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long, env = "FLEETLAYER_WHY")]
        why: Option<String>,
    },
}

#[derive(clap::Args)]
struct SubscribeArgs {
    channel: String,
    #[arg(long)]
    subject: Option<String>,
    /// A commit of the git repository here (default HEAD): its CI results, matched by commit, so a push to a branch
    /// others push to too (main) reaches only you. Ends by itself once every run of the commit has completed.
    #[arg(long, num_args = 0..=1, default_missing_value = "HEAD", value_name = "REV", conflicts_with = "subject")]
    commit: Option<String>,
    /// `<tab>@<node>`. Default: the session this runs in.
    #[arg(long)]
    target: Option<String>,
}

#[derive(Subcommand)]
enum PagesCommand {
    /// Send a page folder (index.md and its images) to a space on another machine: only changed files travel, and a
    /// page of the same name is updated in place. Prints its link and any problems the service found.
    Push {
        /// The page's folder.
        dir: PathBuf,
        /// The space it goes in.
        #[arg(long)]
        space: String,
        /// The page's short name (lower case, hyphens). Default: the folder's name without its date.
        #[arg(long)]
        name: Option<String>,
        /// Start a new dated folder even if a page with this name exists.
        #[arg(long)]
        new: bool,
        /// Why, in one line, when the access rules ask for a reason. Default: $FLEETLAYER_WHY.
        #[arg(long, env = "FLEETLAYER_WHY")]
        why: Option<String>,
    },
}

#[derive(Subcommand)]
enum ToolsCommand {
    /// Offer a tool even off its listed networks, for a while (it stops by itself).
    Enable {
        tool: String,
        /// How long: 30m, 2h, 1d (at most a day).
        #[arg(long = "for", default_value = "2h")]
        duration: String,
    },
    /// End an opt-in now.
    Disable { tool: String },
}

#[derive(Subcommand)]
enum Msg {
    /// Send a message to a session: `<tab>@<node>`, `<tab>` on this node, or a session ID. Fails at once if it isn't
    /// running.
    Send {
        #[arg(long)]
        to: String,
        text: String,
        /// Why, in one line, when the access rules ask for a reason.
        #[arg(long)]
        why: Option<String>,
    },
}

/// Connect stdin and stdout to a shared tool through this node's tools socket.
async fn mcp(socket: &std::path::Path, target: &str, why: Option<&str>) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (tool, node) = target.rsplit_once('@').ok_or_else(|| anyhow::anyhow!("give <tool>@<node>"))?;
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|e| anyhow::anyhow!("can't reach the node's tools socket {} ({e})", socket.display()))?;
    let (r, mut w) = stream.into_split();
    let c = caller();
    let req = json!({"tool": tool, "node": node, "sessionId": c["sessionId"], "address": c["address"], "why": why});
    w.write_all(format!("{req}\n").as_bytes()).await?;
    let mut r = BufReader::new(r);
    let mut line = String::new();
    r.read_line(&mut line).await?;
    let answer: Value = serde_json::from_str(&line).unwrap_or_default();
    if let Some(e) = answer["error"].as_str() {
        anyhow::bail!("{e}");
    }
    let up = async move {
        let _ = tokio::io::copy(&mut tokio::io::stdin(), &mut w).await;
        let _ = w.shutdown().await;
    };
    let down = async move {
        let mut out = tokio::io::stdout();
        let _ = tokio::io::copy(&mut r, &mut out).await;
        let _ = out.flush().await;
    };
    // When stdin ends, the tool gets end-of-file and exits, which ends `down`; when the tool ends, so do we.
    tokio::spawn(up);
    down.await;
    Ok(())
}

/// Ctrl-]: leave an attached terminal.
const DETACH: u8 = 0x1d;

/// Watch or drive a session's terminal through this node's tools socket: frames drawn on this terminal, keys sent
/// as `terminal.input`, size changes as `terminal.resize` (`docs/protocol.md`, "Terminals").
async fn attach(
    socket: &std::path::Path,
    target: &str,
    observe: bool,
    raw: bool,
    why: Option<&str>,
) -> anyhow::Result<()> {
    use base64::Engine;
    use std::io::IsTerminal;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let b64 = &base64::engine::general_purpose::STANDARD;
    let tty = std::io::stdout().is_terminal() && std::io::stdin().is_terminal() && !raw;
    let (cols, rows) = if tty { crossterm::terminal::size().unwrap_or((120, 40)) } else { (120, 40) };
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|e| anyhow::anyhow!("can't reach the node's tools socket {} ({e})", socket.display()))?;
    let (r, mut w) = stream.into_split();
    let c = caller();
    let mode = if observe { "observe" } else { "control" };
    let req = json!({"terminal": target, "mode": mode, "cols": cols, "rows": rows,
        "sessionId": c["sessionId"], "address": c["address"], "why": why});
    w.write_all(format!("{req}\n").as_bytes()).await?;
    let mut lines = BufReader::new(r).lines();
    let first = lines.next_line().await?.unwrap_or_default();
    let answer: Value = serde_json::from_str(&first).unwrap_or_default();
    if let Some(e) = answer["error"].as_str() {
        anyhow::bail!("{e}");
    }
    if !tty {
        // Pass the stream through as it is, both ways.
        let up = async move {
            let _ = tokio::io::copy(&mut tokio::io::stdin(), &mut w).await;
        };
        tokio::spawn(up);
        let mut out = tokio::io::stdout();
        while let Some(line) = lines.next_line().await? {
            out.write_all(format!("{line}\n").as_bytes()).await?;
            out.flush().await?;
        }
        return Ok(());
    }
    crossterm::terminal::enable_raw_mode()?;
    print!("\x1b[?1049h\x1b[2J\x1b[H");
    let result = async {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
        // Keys: Ctrl-] leaves; anything else is typed into the session (unless only watching).
        let keys = {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut stdin = tokio::io::stdin();
                let mut buf = [0u8; 1024];
                while let Ok(n) = stdin.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let bytes = &buf[..n];
                    if let Some(i) = bytes.iter().position(|b| *b == DETACH) {
                        if i > 0 && !observe {
                            let _ = tx
                                .send(json!({"type": "terminal.input", "bytes": b64.encode(&bytes[..i])}).to_string())
                                .await;
                        }
                        let _ = tx.send(json!({"type": "terminal.release"}).to_string()).await;
                        let _ = tx.send(String::new()).await;
                        break;
                    }
                    if !observe {
                        let _ =
                            tx.send(json!({"type": "terminal.input", "bytes": b64.encode(bytes)}).to_string()).await;
                    }
                }
            })
        };
        // Size changes follow this terminal.
        let resize = {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut last = (cols, rows);
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    if let Ok(now) = crossterm::terminal::size()
                        && now != last
                    {
                        last = now;
                        if !observe {
                            let msg = json!({"type": "terminal.resize", "cols": now.0, "rows": now.1}).to_string();
                            if tx.send(msg).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            })
        };
        let writer = tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if m.is_empty() {
                    break;
                }
                if w.write_all(format!("{m}\n").as_bytes()).await.is_err() {
                    break;
                }
            }
            let _ = w.shutdown().await;
        });
        let mut out = tokio::io::stdout();
        let mut reason = String::new();
        loop {
            tokio::select! {
                line = lines.next_line() => {
                    let Ok(Some(line)) = line else { break };
                    let v: Value = serde_json::from_str(&line).unwrap_or_default();
                    match v["type"].as_str() {
                        Some("terminal.frame") => {
                            if let Some(bytes) = v["bytes"].as_str().and_then(|b| b64.decode(b).ok()) {
                                out.write_all(&bytes).await?;
                                out.flush().await?;
                            }
                        }
                        Some("terminal.closed") => {
                            reason = v["reason"].as_str().or(v["code"].as_str()).unwrap_or("closed").to_string();
                            break;
                        }
                        _ => {}
                    }
                }
                _ = writer_done(&writer) => break,
            }
        }
        keys.abort();
        resize.abort();
        writer.abort();
        anyhow::Ok(reason)
    }
    .await;
    print!("\x1b[?1049l");
    let _ = crossterm::terminal::disable_raw_mode();
    match result {
        Ok(reason) if !reason.is_empty() && reason != "detached" => eprintln!("{target}: {reason}"),
        Ok(_) => {}
        Err(e) => return Err(e),
    }
    Ok(())
}

async fn writer_done(h: &tokio::task::JoinHandle<()>) {
    while !h.is_finished() {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
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
    fleetlayer_protocol::local::default_node_socket()
}

struct NoRequests;

#[async_trait]
impl Handler for NoRequests {
    async fn request(&self, _peer: &Peer, method: &str, _params: Value) -> Result<Value, RpcError> {
        Err(RpcError::method_not_found(method))
    }
}

/// A folder on this machine made absolute; one on another node is passed as it is (that node checks it).
fn absolute(dir: &str, elsewhere: bool) -> String {
    if elsewhere || dir.starts_with('/') {
        return dir.to_string();
    }
    std::path::absolute(dir).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| dir.to_string())
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
    match &cli.command {
        Command::Up(_) => {
            let Command::Up(a) = cli.command else { unreachable!() };
            return up::up(a).await;
        }
        Command::Down => return up::down(),
        Command::Services { rest } => return up::services(rest),
        _ => {}
    }
    let socket = cli.socket.clone().unwrap_or_else(default_socket);
    if let Command::Attach { target, observe, raw, why } = &cli.command {
        return attach(&socket.with_extension("tools.sock"), target, *observe, *raw, why.as_deref()).await;
    }
    if let Command::Pages { command: PagesCommand::Push { dir, space, name, new, why } } = &cli.command {
        let push = pages::Push { dir, space, name: name.as_deref(), new: *new, why: why.as_deref(), caller: caller() };
        return pages::push(&socket.with_extension("tools.sock"), push).await;
    }
    if let Command::Mcp { target, why } = &cli.command {
        return mcp(&socket.with_extension("tools.sock"), target, why.as_deref()).await;
    }
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
        Command::Up(_) | Command::Down | Command::Services { .. } => unreachable!("handled before connecting"),
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
            match v["health"].as_str() {
                Some(problem) => println!("  unhealthy: {problem}"),
                None => println!("  healthy"),
            }
            for p in v["plugins"].as_array().into_iter().flatten() {
                let roles: Vec<&str> = p["roles"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                // Running but unable to do its job (its sessions can't be read) shows as failing.
                let state = if p["problem"].is_string() { "failing" } else { p["state"].as_str().unwrap_or("?") };
                let mut line =
                    format!("  {:<16} {:<11} {}", p["name"].as_str().unwrap_or("?"), state, roles.join(", "));
                if let Some(r) = p["reason"].as_str().or(p["problem"].as_str()) {
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
                        "  {:<24} {:<8} {:<8} {}",
                        format!("{}@{name}", m["tab"].as_str().unwrap_or("-")),
                        m["harness"].as_str().unwrap_or("?"),
                        m["status"].as_str().unwrap_or("unknown"),
                        s["summary"]["sentence"].as_str().unwrap_or("")
                    );
                }
            }
        }
        Command::Readings { kinds } => {
            let v: Value = peer.request("node/readings", &json!({"kinds": kinds})).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&v["readings"])?);
                return Ok(());
            }
            for r in v["readings"].as_array().into_iter().flatten() {
                println!(
                    "{:<24} {:<38} {:<20} {:<24} {}",
                    r["kind"].as_str().unwrap_or("?"),
                    r["key"].as_str().unwrap_or("?"),
                    r["node"].as_str().unwrap_or("?"),
                    r["at"].as_str().unwrap_or("?"),
                    r["data"]
                );
            }
        }
        Command::Subscribe(a) | Command::Unsubscribe(a) => {
            let subscribing = matches!(cli.command_name, "subscribe");
            let method = if subscribing { "node/subscribe" } else { "node/unsubscribe" };
            let subject = match &a.commit {
                Some(rev) => Some(commit_subject(rev)?),
                None => a.subject.clone(),
            };
            let args = json!({"channel": a.channel, "subject": subject, "target": a.target, "caller": caller()});
            let v: Value = peer.request(method, &args).await?;
            if cli.json {
                println!("{v}");
            } else if subscribing && !v["commit"].is_null() {
                print!("{}", commit_report(&a.channel, &v["commit"]));
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
        Command::Summaries { query, session, limit, long, mentions, import } => {
            if let Some(dir) = import {
                let v: Value = peer
                    .request_within("node/summaries_import", &json!({"dir": dir}), std::time::Duration::from_secs(620))
                    .await?;
                println!("Imported {} records.", v["records"]);
                return Ok(());
            }
            let args = json!({"q": query.join(" "), "session": session, "limit": limit, "mentions": mentions});
            let v: Value = peer.request("node/summaries", &args).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            if v["summaries"].as_array().is_none_or(|a| a.is_empty()) {
                println!("No summaries match (or none you may read).");
            }
            for s in v["summaries"].as_array().into_iter().flatten() {
                let when = s["activityTo"].as_i64().and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok());
                println!(
                    "{}  {}@{}  {}",
                    when.map(|t| t.strftime("%Y-%m-%d %H:%M").to_string()).unwrap_or_default(),
                    s["tab"].as_str().unwrap_or("-"),
                    s["node"].as_str().unwrap_or("?"),
                    s["sentence"].as_str().unwrap_or("")
                );
                if long && let Some(p) = s["paragraph"].as_str().filter(|p| !p.is_empty()) {
                    println!("    {}\n", p.replace('\n', "\n    "));
                }
            }
        }
        Command::Mcp { .. } | Command::Attach { .. } | Command::Pages { .. } => {}
        Command::Tools { command } => {
            let (method, args) = match &command {
                ToolsCommand::Enable { tool, duration } => {
                    let d = duration.trim();
                    let (n, unit) = d.split_at(d.len().saturating_sub(1));
                    let n: u64 = n.parse().map_err(|_| anyhow::anyhow!("--for is like 30m, 2h or 1d"))?;
                    let minutes = match unit {
                        "m" => n,
                        "h" => n * 60,
                        "d" => n * 1440,
                        _ => anyhow::bail!("--for is like 30m, 2h or 1d"),
                    };
                    ("node/tools_enable", json!({"tool": tool, "minutes": minutes}))
                }
                ToolsCommand::Disable { tool } => ("node/tools_disable", json!({"tool": tool})),
            };
            let v: Value = peer.request(method, &args).await?;
            if v["enabled"].as_bool() == Some(true) {
                println!(
                    "{} is offered on any network for the next {} minutes.",
                    v["tool"].as_str().unwrap_or("?"),
                    v["minutes"]
                );
            } else {
                println!("{} is offered only on its listed networks again.", v["tool"].as_str().unwrap_or("?"));
            }
        }
        Command::Session { command } => {
            let req = match command {
                SessionCommand::Start { tab, cwd, on, harness, prompt, why } => json!({
                    "tab": tab, "cwd": absolute(&cwd, on.is_some()), "on": on, "harness": harness, "prompt": prompt,
                    "why": why, "caller": caller(),
                }),
                SessionCommand::Resume { session_id, on, tab, cwd, prompt, why } => json!({
                    "resume": session_id, "tab": tab, "cwd": cwd.map(|c| absolute(&c, on.is_some())), "on": on,
                    "prompt": prompt, "why": why, "caller": caller(),
                }),
            };
            let v: Value = peer.request_within("node/session_open", &req, std::time::Duration::from_secs(180)).await?;
            if cli.json {
                println!("{v}");
            } else {
                let id = v["sessionId"].as_str().map(|id| format!(", session {id}")).unwrap_or_default();
                println!("Started {}@{}{id}.", v["tab"].as_str().unwrap_or("?"), v["node"].as_str().unwrap_or("?"));
                if let Some(e) = v["promptError"].as_str() {
                    println!("  The first prompt didn't go in: {e}");
                }
            }
        }
        Command::Msg { command: Msg::Send { to, text, why } } => {
            let v: Value = peer
                .request(
                    "_fleetlayer.dev/message/send",
                    &json!({"to": to, "text": text, "caller": caller(), "why": why}),
                )
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

/// `<owner/repo>@<full sha>` for `rev` in the git repository here, from its `origin` remote on GitHub.
fn commit_subject(rev: &str) -> anyhow::Result<String> {
    let git = |args: &[&str]| -> anyhow::Result<String> {
        let out = std::process::Command::new("git").args(args).output()?;
        anyhow::ensure!(
            out.status.success(),
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let url = git(&["remote", "get-url", "origin"])?;
    let repo =
        github_repo(&url).ok_or_else(|| anyhow::anyhow!("origin ({url}) isn't a GitHub repository; use --subject"))?;
    let sha = git(&["rev-parse", "--verify", &format!("{rev}^{{commit}}")])?;
    Ok(format!("{repo}@{sha}"))
}

/// `owner/repo` from a GitHub remote URL (https or SSH).
fn github_repo(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, repo) = rest.split_once('/')?;
    (!owner.is_empty() && !repo.is_empty() && !repo.contains('/')).then(|| format!("{owner}/{repo}"))
}

/// What a commit subscription found: results already in, runs still pending, whether it ended at once.
fn commit_report(channel: &str, c: &Value) -> String {
    let subject = c["subject"].as_str().unwrap_or_default();
    let short = match subject.rsplit_once('@') {
        Some((repo, sha)) => format!("{repo}@{}", &sha[..sha.len().min(7)]),
        None => subject.to_string(),
    };
    let mut out = format!("Subscribed to {channel} for {short}.\n");
    let delivered = c["delivered"].as_array().cloned().unwrap_or_default();
    for d in &delivered {
        let how = if d["ok"].as_bool() == Some(true) { "sent to you now" } else { "couldn't be sent" };
        out.push_str(&format!("Already in ({how}): {}\n", d["summary"].as_str().unwrap_or_default()));
    }
    let pending: Vec<&str> = c["pending"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if !pending.is_empty() {
        out.push_str(&format!(
            "Pending: {}. Each result arrives as a message when it completes.\n",
            pending.join(", ")
        ));
    } else if c["ended"].as_bool() == Some(true) {
        out.push_str("Nothing pending, so the subscription has ended.\n");
    } else if delivered.is_empty() {
        out.push_str("No runs seen yet for this commit; each result arrives as a message when it completes.\n");
    }
    out
}

#[cfg(test)]
mod commit_tests {
    use super::*;

    #[test]
    fn github_remotes_name_their_repository() {
        for url in [
            "git@github.com:fleetlayer/fleetlayer.git",
            "https://github.com/fleetlayer/fleetlayer",
            "https://github.com/fleetlayer/fleetlayer.git/",
            "ssh://git@github.com/fleetlayer/fleetlayer.git",
        ] {
            assert_eq!(github_repo(url).as_deref(), Some("fleetlayer/fleetlayer"), "{url}");
        }
        assert_eq!(github_repo("git@gitlab.com:a/b.git"), None);
        assert_eq!(github_repo("https://github.com/a"), None);
    }

    #[test]
    fn a_commit_subscription_says_what_it_found() {
        let c = json!({"subject": "o/r@1a2b3c4d5e", "pending": ["CI"],
            "delivered": [{"summary": "Site success on o/r@main 1a2b3c4", "ok": true}], "ended": false});
        assert_eq!(
            commit_report("ci", &c),
            "Subscribed to ci for o/r@1a2b3c4.\nAlready in (sent to you now): Site success on o/r@main 1a2b3c4\n\
             Pending: CI. Each result arrives as a message when it completes.\n"
        );
        let c = json!({"subject": "o/r@1a2b3c4d5e", "pending": [], "delivered": [], "ended": false});
        assert!(commit_report("ci", &c).contains("No runs seen yet"));
    }
}
