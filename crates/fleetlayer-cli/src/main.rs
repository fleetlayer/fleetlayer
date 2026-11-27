//! fleetlayer: the command line. It talks to this machine's node over its local socket.

mod mcp_bridge;
mod pages;
mod relocate;
mod secrets;
#[cfg(test)]
mod skill_check;
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
    /// Move one piece (control, node, gateway, hub, pages, sign-in, any plugin) to another machine over SSH, keeping
    /// its identity, keys and data: stop it here, copy it sealed and checked, start it there, point everything at it,
    /// then remove the copy here. `--dry-run` prints the plan. docs/guides/moving.md.
    Move(relocate::MoveArgs),
    /// The destination's half of `fleetlayer move`, run over SSH.
    #[command(name = "move-receive", hide = true)]
    MoveReceive {
        step: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Every service the gateway serves: its address and where it really runs.
    Services {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        rest: Vec<String>,
    },
    /// The node and its plugins.
    Status,
    /// What this session may use from here: sessions to message, shared tools (with the line that adds each one),
    /// pages spaces, subscriptions, and whether its messages arrive as outside data. Asking files no approval request.
    Me,
    /// The secrets delivered to this node's role: run a command with them (the `op run` replacement), scrub files of
    /// them, list them by reference, see who may read what, or remove them all.
    Secrets {
        #[command(subcommand)]
        command: secrets::SecretsCommand,
    },
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
        /// Seconds one call may take before it's answered with an error (the machine may be asleep, or its node
        /// restarting).
        #[arg(long, default_value_t = 120)]
        call_timeout: u64,
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
    /// Windows of roles that are off until the person approves one (the admin window, or "sudo mode").
    Role {
        #[command(subcommand)]
        command: RoleCommand,
    },
}

#[derive(Subcommand)]
enum RoleCommand {
    /// Ask the person to open a role's window: `fleetlayer role activate admin --for 1h --why "…"`. It opens when they
    /// approve; until it closes, the role's rules apply, and at the end its sessions are closed, what was delivered to
    /// it is removed and its hook runs.
    Activate {
        role: String,
        /// How long: 1h, 12h, 1d (at most the role's `longest:`).
        #[arg(long = "for", default_value = "1h")]
        hold: String,
        /// What it's for, in one line, shown to the person and kept in the audit log. Default: $FLEETLAYER_WHY.
        #[arg(long, env = "FLEETLAYER_WHY")]
        why: Option<String>,
    },
    /// Close a role's window now, before its end.
    Deactivate { role: String },
    /// The windowed roles this session is in or may ask for, and whether each is open.
    List,
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
    /// Answer the prompt a session is blocked on (a question, a permission prompt, a plan), as shown in its
    /// `meta.prompt`: `fleetlayer session answer review@server --prompt toolu_01… --pick 2`. Refused when that prompt
    /// isn't showing any more.
    Answer {
        /// `<tab>@<node>`, a tab on this node, or a session ID.
        to: String,
        /// The prompt's `id`.
        #[arg(long)]
        prompt: String,
        /// One per question, in order: an option's number, numbers with commas for a multiple choice (`1,3`), or
        /// `=` and free text (`=Use the staging database`).
        #[arg(long = "pick", alias = "answer")]
        picks: Vec<String>,
        /// Free text: the answer to a one-question form, or a plan's "Tell Claude what to change".
        #[arg(long)]
        text: Option<String>,
        #[arg(long, env = "FLEETLAYER_WHY")]
        why: Option<String>,
    },
    /// Close a session's tab (asked by default from another node).
    Close {
        to: String,
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
async fn mcp(
    socket: &std::path::Path,
    target: &str,
    why: Option<&str>,
    limit: std::time::Duration,
) -> anyhow::Result<()> {
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
    match tokio::time::timeout(std::time::Duration::from_secs(30), r.read_line(&mut line)).await {
        Ok(read) => read?,
        Err(_) => anyhow::bail!(
            "{target} didn't answer within 30s (the node or the control plane may be restarting, or {node} asleep)"
        ),
    };
    let answer: Value = serde_json::from_str(&line).unwrap_or_default();
    if let Some(e) = answer["error"].as_str() {
        anyhow::bail!("{e}");
    }
    // When stdin ends, the tool gets end-of-file and exits, which ends the bridge; when the tool ends, so do we.
    mcp_bridge::bridge(tokio::io::stdin(), tokio::io::stdout(), r, w, target, limit).await;
    // Exit now: tokio reads stdin on a blocking thread, and the runtime would wait for that read (for the client to
    // close stdin) before exiting, so a tool whose node went away would hang the client's call instead of failing it.
    std::process::exit(0)
}

/// `fleetlayer me` for a person or an agent to read.
fn me_text(v: &Value) -> String {
    use std::fmt::Write;
    let mut o = String::new();
    let address = v["address"].as_str().unwrap_or("?");
    let harness = v["harness"].as_str().map(|h| format!(" ({h})")).unwrap_or_default();
    let node = v["node"].as_str().unwrap_or("?");
    if v["caller"] == "-" {
        let _ = writeln!(
            o,
            "You aren't in a session this node knows, so this is what a shell on {node} (-@{node}) may use."
        );
    } else {
        let _ = writeln!(o, "You are {address}{harness}, on node {node}.");
    }
    if v["offline"] == true {
        let _ = writeln!(o, "{node} isn't linked to a control plane now: you can reach only this node's sessions.");
        return o;
    }
    let me = &v["me"];
    let list = |k: &str| me[k].as_array().cloned().unwrap_or_default();
    let message = list("message");
    let refused = me["messageRefused"].as_u64().unwrap_or(0);
    let _ = match (message.len(), refused) {
        (0, 0) => writeln!(o, "Messages: no running sessions on other nodes that you can see."),
        (n, 0) => writeln!(
            o,
            "Messages: you may message all {n} running sessions on other nodes (`fleetlayer fleet` lists them)."
        ),
        (0, _) => writeln!(
            o,
            "Messages: you may message none of the sessions on other nodes, only reply to ones that wrote to you."
        ),
        _ => writeln!(
            o,
            "Messages: you may message {}.",
            message.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")
        ),
    };
    let _ = writeln!(
        o,
        "Your messages arrive {}.",
        if me["untrusted"] == true { "marked as outside data (you read untrusted input)" } else { "as from you" }
    );
    let justify: Vec<String> = list("justify").iter().filter_map(Value::as_str).map(String::from).collect();
    if !justify.is_empty() {
        let _ = writeln!(o, "Give a reason with --why for: {}.", justify.join(", "));
    }
    let state = |t: &Value| -> String {
        let mut s = match t["state"].as_str().unwrap_or("?") {
            "allowed" => match t["until"].as_i64().and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok()) {
                Some(u) => format!("allowed (approved until {})", u.strftime("%Y-%m-%d")),
                None => "allowed".into(),
            },
            "ask" => "your first use asks your person".into(),
            "pending" => format!("waiting for your person to approve request {}", t["request"].as_str().unwrap_or("?")),
            "refused" => format!(
                "your person refused request {} (ask again after an hour)",
                t["request"].as_str().unwrap_or("?")
            ),
            other => other.into(),
        };
        if t["offline"] == true {
            s.push_str("; its node is offline");
        }
        s
    };
    let tools = list("tools");
    let unknown: Vec<&str> = me["unknown"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if tools.is_empty() && !unknown.is_empty() {
        let _ = writeln!(o, "Shared tools: none reported; {}.", unknown.join("; "));
    } else if tools.is_empty() {
        let _ = writeln!(o, "Shared tools: none you may use.");
    } else {
        let _ = writeln!(o, "Shared tools:");
        for t in &tools {
            let name = t["name"].as_str().unwrap_or("?");
            let _ = writeln!(o, "  {name}: {}", state(t));
            if let Some(d) = t["description"].as_str() {
                let _ = writeln!(o, "    {d}");
            }
            if let Some(n) = t["notOffered"].as_str() {
                let _ = writeln!(o, "    Not offered now: {n}");
            }
            let folders: Vec<String> = t["folders"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|f| {
                    let mut can = vec!["read"];
                    if f["upload"] == true {
                        can.push("upload");
                    }
                    if f["delete"] == true {
                        can.push("delete");
                    }
                    format!("{} ({})", f["name"].as_str().unwrap_or("?"), can.join(", "))
                })
                .collect();
            if !folders.is_empty() {
                let _ = writeln!(o, "    Folders: {}", folders.join(", "));
            }
            let short = name.split('@').next().unwrap_or(name);
            let _ = writeln!(o, "    Add it: claude mcp add {short} -- fleetlayer mcp {name}");
        }
    }
    for p in list("pages") {
        let space = p["name"].as_str().unwrap_or("?");
        let _ = writeln!(o, "Pages space {space} (on {}): {}", p["node"].as_str().unwrap_or("?"), state(&p));
        let _ = writeln!(o, "    Push a page: fleetlayer pages push <folder> --space {space}");
    }
    let start: Vec<String> = list("start")
        .iter()
        .map(|t| {
            let n = t["name"].as_str().unwrap_or("?");
            match (n == node, t["state"].as_str()) {
                (true, _) => format!("{n} (this node)"),
                (false, Some("ask")) => format!("{n} (asks your person first)"),
                _ => n.to_string(),
            }
        })
        .collect();
    if !start.is_empty() {
        let _ =
            writeln!(o, "Start or resume sessions on: {} (`fleetlayer session start --on <node>`).", start.join(", "));
    }
    if !tools.is_empty() && !unknown.is_empty() {
        let _ = writeln!(o, "Not reported, so not listed: {}.", unknown.join("; "));
    }
    for w in me["windows"].as_array().into_iter().flatten() {
        let _ = writeln!(o, "Role window: {}", window_line(w));
    }
    let subs: Vec<&str> = me["subscriptions"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if !subs.is_empty() {
        let _ = writeln!(o, "Subscribed to: {}.", subs.join(", "));
    }
    o
}

/// One windowed role: `admin: open until 2026-11-28 15:00 UTC (you're one of its agents)`.
fn window_line(w: &Value) -> String {
    let role = w["role"].as_str().unwrap_or("?");
    let when = |ms: i64| {
        jiff::Timestamp::from_millisecond(ms).map(|t| t.strftime("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default()
    };
    let longest = w["longestMs"].as_i64().unwrap_or(0) / 3_600_000;
    let state = match w["state"].as_str() {
        Some("open") => format!("open until {}", w["endsAt"].as_i64().map(when).unwrap_or_default()),
        Some("pending") => format!("waiting for the person to approve {}", w["id"].as_str().unwrap_or("")),
        _ => format!("off (`fleetlayer role activate {role} --for 1h`, at most {longest}h)"),
    };
    let member = if w["member"].as_bool() == Some(true) { " (you're one of its agents)" } else { "" };
    format!("{role}: {state}{member}")
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

/// One `--pick`: `2`, `1,3` or `=free text`.
fn parse_answer(p: &str) -> anyhow::Result<Value> {
    if let Some(text) = p.strip_prefix('=') {
        return Ok(json!({"text": text}));
    }
    let picks: Vec<u32> = p
        .split(',')
        .map(|n| n.trim().parse::<u32>().map_err(|_| anyhow::anyhow!("--pick {p}: give option numbers, or =text")))
        .collect::<anyhow::Result<_>>()?;
    Ok(if picks.len() == 1 && !p.contains(',') { json!({"pick": picks[0]}) } else { json!({"picks": picks}) })
}

fn print_acted(v: &Value, json: bool) {
    if json {
        println!("{v}");
        return;
    }
    let tab = v["tab"].as_str().or(v["sessionId"].as_str()).unwrap_or("?");
    let done = if v["done"] == "closed" { "Closed" } else { "Answered" };
    println!("{done} {tab}@{}.", v["node"].as_str().unwrap_or("?"));
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
        Command::MoveReceive { step, args } => return relocate::receive(step, args).await,
        Command::Move(_) => {
            let Command::Move(a) = cli.command else { unreachable!() };
            return relocate::run(a).await;
        }
        _ => {}
    }
    if let Command::Secrets { command } = &cli.command
        && !matches!(command, secrets::SecretsCommand::Grants | secrets::SecretsCommand::Forget)
    {
        let Command::Secrets { command } = cli.command else { unreachable!() };
        let code = secrets::run(command).await?;
        std::process::exit(code);
    }
    let socket = cli.socket.clone().unwrap_or_else(default_socket);
    if let Command::Attach { target, observe, raw, why } = &cli.command {
        return attach(&socket.with_extension("tools.sock"), target, *observe, *raw, why.as_deref()).await;
    }
    if let Command::Pages { command: PagesCommand::Push { dir, space, name, new, why } } = &cli.command {
        let push = pages::Push { dir, space, name: name.as_deref(), new: *new, why: why.as_deref(), caller: caller() };
        return pages::push(&socket.with_extension("tools.sock"), push).await;
    }
    if let Command::Mcp { target, why, call_timeout } = &cli.command {
        let limit = std::time::Duration::from_secs(*call_timeout);
        return mcp(&socket.with_extension("tools.sock"), target, why.as_deref(), limit).await;
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
        Command::Up(_) | Command::Down | Command::Services { .. } | Command::Move(_) | Command::MoveReceive { .. } => {
            unreachable!("handled before connecting")
        }
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
            let skills = &v["skills"];
            if let Some(n) = skills["note"].as_str() {
                println!("  skills: {n}");
            }
            for d in skills["skipped"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                println!(
                    "  skill skipped: {d} is someone's own, so Fleetlayer's isn't installed there. To use Fleetlayer's, \
                     move it away and restart the node"
                );
            }
        }
        Command::Me => {
            let v: Value = peer.request("node/me", &json!({"caller": caller()})).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&v)?);
                return Ok(());
            }
            print!("{}", me_text(&v));
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
        Command::Secrets { command: secrets::SecretsCommand::Forget } => {
            let v: Value = peer.request("node/secrets_forget", &json!({})).await?;
            println!(
                "Removed what {} bundle(s) delivered to this node; the next delivery of each puts it back.",
                v["forgotten"]
            );
        }
        Command::Secrets { .. } => {
            let v: Value = peer.request("node/readings", &json!({"kinds": ["secrets.grants"]})).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&v["readings"])?);
            } else {
                secrets::print_grants(&v["readings"]);
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
        Command::Session { command: SessionCommand::Answer { to, prompt, picks, text, why } } => {
            let mut answers = picks.iter().map(|p| parse_answer(p)).collect::<anyhow::Result<Vec<Value>>>()?;
            if let Some(t) = text {
                answers.push(json!({"text": t}));
            }
            anyhow::ensure!(!answers.is_empty(), "give an answer with --pick or --text");
            let req = json!({"to": to, "prompt": prompt, "answers": answers, "why": why, "caller": caller()});
            let v: Value = peer.request_within("node/session_answer", &req, std::time::Duration::from_secs(60)).await?;
            print_acted(&v, cli.json);
        }
        Command::Role { command } => {
            let (method, req) = match command {
                RoleCommand::Activate { role, hold, why } => {
                    ("node/role_activate", json!({"role": role, "for": hold, "why": why, "caller": caller()}))
                }
                RoleCommand::Deactivate { role } => ("node/role_deactivate", json!({"role": role, "caller": caller()})),
                RoleCommand::List => ("node/role_list", json!({"caller": caller()})),
            };
            let v: Value = peer.request_within(method, &req, std::time::Duration::from_secs(30)).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else if let Some(windows) = v["windows"].as_array() {
                if windows.is_empty() {
                    println!("No role with a window is yours to ask for.");
                }
                for w in windows {
                    println!("{}", window_line(w));
                }
            } else {
                println!("{}", v["message"].as_str().unwrap_or(""));
            }
        }
        Command::Session { command: SessionCommand::Close { to, why } } => {
            let req = json!({"to": to, "why": why, "caller": caller()});
            let v: Value = peer.request_within("node/session_close", &req, std::time::Duration::from_secs(60)).await?;
            print_acted(&v, cli.json);
        }
        Command::Session { command } => {
            let req = match command {
                SessionCommand::Answer { .. } | SessionCommand::Close { .. } => unreachable!("handled above"),
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
mod me_tests {
    use super::*;

    fn me(tools: Value, unknown: Value) -> String {
        me_text(&json!({"node": "server", "caller": "planning", "address": "planning@server", "harness": "claude",
            "me": {"tools": tools, "unknown": unknown, "message": [], "messageRefused": 0}}))
    }

    #[test]
    fn a_node_too_old_to_report_tools_is_named_on_the_tools_line() {
        let t = me(json!([]), json!(["nathans-mac is too old to say (v0.3.8)"]));
        assert!(t.contains("Shared tools: none reported; nathans-mac is too old to say (v0.3.8).\n"), "{t}");
        assert!(!t.contains("none you may use"), "{t}");
        let t = me(json!([]), json!([]));
        assert!(t.contains("Shared tools: none you may use.\n"), "{t}");
        let t = me(
            json!([{"name": "browser@laptop", "node": "laptop", "state": "allowed"}]),
            json!(["nathans-mac is too old to say (v0.3.8)"]),
        );
        assert!(t.contains("  browser@laptop: allowed\n"), "{t}");
        assert!(t.contains("Not reported, so not listed: nathans-mac is too old to say (v0.3.8).\n"), "{t}");
    }
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
