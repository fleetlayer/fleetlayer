//! `fleetlayer lease`: leases from the fleet's leases authority (`docs/plugins/leases.md`), a tool its node shares.
//! Each command opens the tool through this node's tools socket, as `fleetlayer mcp` does, so the control plane checks
//! and audits every call, and speaks MCP to it.
//!
//! A lease taken in a session is tied to the session: it ends when the session stops or stays idle too long. One
//! taken by a script (or with `--pid`) is tied to a process: a keeper (`fleetlayer lease keep`, started in the
//! background) holds a connection open to the authority while that process lives, and releases the lease when it
//! exits.

use std::path::Path;
use std::time::Duration;

use clap::Subcommand;
use serde_json::{Value, json};
use tokio::io::Lines;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

#[derive(Subcommand)]
pub enum LeaseCommand {
    /// Take a lease: `fleetlayer lease acquire build-server --reason "cargo test" --wait`. Prints its ID once it's
    /// held (or asking, for a kind the person approves). Release it when you're done.
    Acquire {
        /// The kind of lease (`fleetlayer lease list` shows them).
        kind: String,
        /// What it's for, in one line: shown to whoever waits behind you, and to the person when it needs approval.
        #[arg(long)]
        reason: String,
        /// What exactly is claimed, as a JSON object, for kinds that take one: `{"account":"work","query":"from:x"}`.
        #[arg(long)]
        payload: Option<String>,
        /// Wait in line (and for approval) until it's held, instead of failing when every slot is taken.
        #[arg(long)]
        wait: bool,
        /// Its limit: 30m, 2h (at most the kind's max_ttl; default the kind's ttl).
        #[arg(long)]
        ttl: Option<String>,
        /// Lose it when this session stays idle (or waits on the person) this long (default the kind's idle).
        #[arg(long)]
        idle: Option<String>,
        /// Tie it to this process instead of the session: it's released when the process exits. Default
        /// $FLEETLAYER_LEASE_PID; outside a session, the process that ran this command.
        #[arg(long, env = "FLEETLAYER_LEASE_PID")]
        pid: Option<u32>,
        #[command(flatten)]
        at: At,
    },
    /// End a lease of yours.
    Release {
        id: u64,
        #[command(flatten)]
        at: At,
    },
    /// Every kind of lease, who holds and who waits, and the leases that ended lately.
    List {
        #[arg(long)]
        kind: Option<String>,
        /// Every lease that ended in the last day, not only the latest.
        #[arg(long)]
        all: bool,
        #[command(flatten)]
        at: At,
    },
    /// One lease, or how it ended.
    Show {
        id: u64,
        #[command(flatten)]
        at: At,
    },
    /// Wait until a lease of yours that's asking or waiting is held; prints its ID.
    Wait {
        id: u64,
        #[command(flatten)]
        at: At,
    },
    /// Keep a process-tied lease while the process lives (started by `acquire` in the background).
    #[command(hide = true)]
    Keep {
        id: u64,
        #[arg(long)]
        pid: u32,
        #[command(flatten)]
        at: At,
    },
}

#[derive(clap::Args, Clone)]
pub struct At {
    /// The leases authority: `<node>` or `<tool>@<node>`. Default $FLEETLAYER_LEASES, else the one `leases` tool this
    /// session may use.
    #[arg(long, env = "FLEETLAYER_LEASES")]
    at: Option<String>,
}

pub async fn run(socket: &Path, command: LeaseCommand, as_json: bool) -> anyhow::Result<()> {
    match command {
        LeaseCommand::Acquire { kind, reason, payload, wait, ttl, idle, pid, at } => {
            let target = target(socket, &at).await?;
            let payload: Value = match payload {
                Some(p) => serde_json::from_str(&p).map_err(|e| anyhow::anyhow!("--payload isn't JSON: {e}"))?,
                None => Value::Null,
            };
            let caller = crate::caller();
            let in_session = !caller["sessionId"].is_null() || !caller["address"].is_null();
            // In a session, the lease follows the session unless a process is named. Outside one, it follows the
            // process that ran this command (a script, on-dev); the session's node may not know the session yet, so
            // that's the fallback the authority uses then too.
            let tie = if in_session && pid.is_none() { "session" } else { "process" };
            let pid = pid.unwrap_or_else(std::os::unix::process::parent_id);
            let args = json!({"kind": kind, "reason": reason, "payload": payload, "wait": wait, "ttl": ttl,
                "idle": idle, "tie": tie, "pid": pid});
            // A wait cut off by the authority, its node or the control plane restarting asks again (its place in line
            // is gone with the connection) for up to ten minutes, rather than leaving the script to go without.
            let deadline = std::time::Instant::now() + Duration::from_secs(600);
            let mut backoff = Duration::from_secs(2);
            let v = loop {
                match call(socket, &target, "acquire", args.clone(), true).await {
                    Err(e) if wait && transient(&e) && std::time::Instant::now() < deadline => {
                        eprintln!("{e}; asking again in {}s", backoff.as_secs());
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                    }
                    other => break other?,
                }
            };
            let lease = &v["lease"];
            let id = lease["id"].as_u64().ok_or_else(|| anyhow::anyhow!("the authority's answer has no lease: {v}"))?;
            if lease["tie"]["kind"] == "process" {
                if tie == "session" {
                    eprintln!(
                        "Note: this node doesn't know your session, so lease #{id} follows this command's parent \
                         process ({pid}) and is released when it exits. Give --pid <the process doing the work> to \
                         keep it longer."
                    );
                }
                keep_in_background(id, pid, &target)?;
            }
            report(lease, as_json);
            Ok(())
        }
        LeaseCommand::Release { id, at } => {
            let target = target(socket, &at).await?;
            call(socket, &target, "release", json!({"id": id}), false).await?;
            if as_json {
                println!("{}", json!({"released": id}));
            } else {
                eprintln!("Released lease #{id}.");
            }
            Ok(())
        }
        LeaseCommand::List { kind, all, at } => {
            let target = target(socket, &at).await?;
            let v = call(socket, &target, "list", json!({"kind": kind, "all": all}), false).await?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print!("{}", list_text(&v));
            }
            Ok(())
        }
        LeaseCommand::Show { id, at } => {
            let target = target(socket, &at).await?;
            let v = call(socket, &target, "show", json!({"id": id}), false).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        LeaseCommand::Wait { id, at } => {
            let target = target(socket, &at).await?;
            let v = call(socket, &target, "wait", json!({"id": id}), true).await?;
            report(&v["lease"], as_json);
            Ok(())
        }
        LeaseCommand::Keep { id, pid, at } => {
            let target = target(socket, &at).await?;
            keep(socket, &target, id, pid).await
        }
    }
}

/// The ID on stdout (for `id=$(fleetlayer lease acquire …)`), the rest on stderr.
fn report(lease: &Value, as_json: bool) {
    if as_json {
        println!("{lease}");
        return;
    }
    let id = &lease["id"];
    match lease["phase"].as_str() {
        Some("held") => eprintln!(
            "Holding {} lease #{id} until {} at the latest. Release it when you're done: fleetlayer lease release {id}",
            lease["kind"].as_str().unwrap_or("?"),
            lease["expiresAt"].as_str().unwrap_or("?")
        ),
        Some("asking") => eprintln!(
            "Lease #{id} waits for approval ({}). You'll get a message when it starts; `fleetlayer lease wait {id}` \
             waits for it here.",
            lease["approval"].as_str().unwrap_or("asking the access policy")
        ),
        other => eprintln!("Lease #{id} is {}.", other.unwrap_or("?")),
    }
    println!("{id}");
}

/// The authority's tool and node, `(tool, node)`.
async fn target(socket: &Path, at: &At) -> anyhow::Result<(String, String)> {
    find_tool(socket, at.at.as_deref(), "leases", "leases authority", "FLEETLAYER_LEASES").await
}

/// The node sharing tool `name` that this session may use, or the one `at` names (`<node>` or `<tool>@<node>`):
/// `(tool, node)`. `what` and `env` name it in errors.
pub(crate) async fn find_tool(
    socket: &Path,
    at: Option<&str>,
    name: &str,
    what: &str,
    env: &str,
) -> anyhow::Result<(String, String)> {
    if let Some(a) = at {
        return Ok(match a.rsplit_once('@') {
            Some((tool, node)) => (tool.to_string(), node.to_string()),
            None => (name.to_string(), a.to_string()),
        });
    }
    let stream = tokio::net::UnixStream::connect(socket).await.map_err(|e| {
        anyhow::anyhow!("can't reach the node at {} ({e}). Is fleetlayer-node running?", socket.display())
    })?;
    let (r, w) = stream.into_split();
    let (peer, _task) = fleetlayer_plugin::connect(r, w, std::sync::Arc::new(crate::NoRequests));
    let v: Value = peer.request("node/me", &json!({"caller": crate::caller()})).await?;
    let found: Vec<(String, String)> = v["me"]["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let (tool, node) = t["name"].as_str()?.rsplit_once('@')?;
            (tool == name).then(|| (tool.to_string(), node.to_string()))
        })
        .collect();
    match &found[..] {
        [one] => Ok(one.clone()),
        [] => anyhow::bail!(
            "no {what} found: no node shares a `{name}` tool this session may use (`fleetlayer me` lists \
             them). Name it with --at <node> or ${env}"
        ),
        many => anyhow::bail!(
            "several nodes share a `{name}` tool ({}): choose one with --at <node> or ${env}",
            many.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// An open MCP connection to the authority's tool.
struct Conn {
    lines: Lines<BufReader<OwnedReadHalf>>,
    w: OwnedWriteHalf,
    next: u64,
    /// `<tool> authority`, for errors.
    authority: String,
}

async fn open(socket: &Path, target: &(String, String)) -> anyhow::Result<Conn> {
    open_as(socket, target, "fleetlayer lease").await
}

/// Open the connection, trying again a few times when the way there failed in passing ([`transient`]: the node or the
/// control plane restarting, or the proxy in front of it answering 502, 503 or 504).
async fn open_as(socket: &Path, target: &(String, String), client: &str) -> anyhow::Result<Conn> {
    let waits = [Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(4)];
    let mut attempt = 0;
    loop {
        match open_once(socket, target, client).await {
            Err(e) if attempt < waits.len() && transient(&e) => {
                eprintln!("{e}; trying again");
                tokio::time::sleep(waits[attempt]).await;
                attempt += 1;
            }
            r => return r,
        }
    }
}

async fn open_once(socket: &Path, (tool, node): &(String, String), client: &str) -> anyhow::Result<Conn> {
    let tools = socket.with_extension("tools.sock");
    let stream = tokio::net::UnixStream::connect(&tools)
        .await
        .map_err(|e| anyhow::anyhow!("can't reach the node's tools socket {} ({e})", tools.display()))?;
    let (r, mut w) = stream.into_split();
    let c = crate::caller();
    let req = json!({"tool": tool, "node": node, "sessionId": c["sessionId"], "address": c["address"],
        "why": std::env::var("FLEETLAYER_WHY").ok()});
    w.write_all(format!("{req}\n").as_bytes()).await?;
    let mut lines = BufReader::new(r).lines();
    let first = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
        .await
        .map_err(|_| anyhow::anyhow!("{tool}@{node} didn't answer within 30s"))??
        .ok_or_else(|| anyhow::anyhow!("the node closed the tools socket"))?;
    let answer: Value = serde_json::from_str(&first).unwrap_or_default();
    if let Some(e) = answer["error"].as_str() {
        anyhow::bail!("{tool}@{node}: {e}");
    }
    let mut conn = Conn { lines, w, next: 1, authority: format!("{tool} authority") };
    conn.request(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": client, "version": env!("CARGO_PKG_VERSION")}}),
        |_| {},
    )
    .await?;
    conn.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await?;
    Ok(conn)
}

impl Conn {
    async fn send(&mut self, v: Value) -> anyhow::Result<()> {
        self.w.write_all(format!("{v}\n").as_bytes()).await?;
        self.w.flush().await?;
        Ok(())
    }

    /// One request; progress notifications go to `on_progress` while it waits.
    async fn request(
        &mut self,
        method: &str,
        params: Value,
        mut on_progress: impl FnMut(&str),
    ) -> anyhow::Result<Value> {
        let id = self.next;
        self.next += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await?;
        while let Some(line) = self.lines.next_line().await? {
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if v["method"] == "notifications/progress" {
                if let Some(m) = v["params"]["message"].as_str() {
                    on_progress(m);
                }
                continue;
            }
            // A request from the server (a ping): answer it.
            if v.get("method").is_some() {
                if let Some(rid) = v.get("id") {
                    self.send(json!({"jsonrpc": "2.0", "id": rid, "result": {}})).await?;
                }
                continue;
            }
            if v["id"] != json!(id) {
                continue;
            }
            if let Some(e) = v.get("error") {
                anyhow::bail!("{}", e["message"].as_str().unwrap_or("the authority refused the call"));
            }
            return Ok(v["result"].clone());
        }
        anyhow::bail!("the {} went away (its node or the control plane may be restarting)", self.authority)
    }

    /// One tool call; its JSON answer, or its error text as an error.
    async fn call(&mut self, name: &str, args: Value, progress: bool) -> anyhow::Result<Value> {
        let r = self
            .request("tools/call", json!({"name": name, "arguments": args, "_meta": {"progressToken": 1}}), |m| {
                if progress {
                    eprintln!("{m}");
                }
            })
            .await?;
        let text = r["content"][0]["text"].as_str().unwrap_or("").to_string();
        if r["isError"] == true {
            anyhow::bail!("{text}");
        }
        Ok(if r["structuredContent"].is_object() {
            r["structuredContent"].clone()
        } else {
            serde_json::from_str(&text)?
        })
    }
}

/// One tool call on a fresh connection, for another command built on a shared tool (`fleetlayer presence`).
pub(crate) async fn call_tool(
    socket: &Path,
    target: &(String, String),
    client: &str,
    name: &str,
    args: Value,
) -> anyhow::Result<Value> {
    let mut conn = open_as(socket, target, client).await?;
    conn.call(name, args, false).await
}

async fn call(
    socket: &Path,
    target: &(String, String),
    name: &str,
    args: Value,
    progress: bool,
) -> anyhow::Result<Value> {
    let mut conn = open(socket, target).await?;
    conn.call(name, args, progress).await
}

/// Start the keeper detached, in a process group of its own (so a Ctrl-C to the script's group doesn't stop it
/// before it can release), with no output (so `id=$(…)` returns at once).
fn keep_in_background(id: u64, pid: u32, (tool, node): &(String, String)) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["lease", "keep", &id.to_string(), "--pid", &pid.to_string(), "--at", &format!("{tool}@{node}")])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    if let Some(s) = std::env::args_os().zip(std::env::args_os().skip(1)).find(|(a, _)| a == "--socket").map(|(_, b)| b)
    {
        cmd.arg("--socket").arg(s);
    }
    cmd.spawn().map_err(|e| anyhow::anyhow!("starting the lease's keeper: {e}"))?;
    Ok(())
}

/// The authority couldn't be reached or went away mid-call: worth asking again.
fn transient(e: &anyhow::Error) -> bool {
    let m = e.to_string();
    [
        "went away",
        "can't reach",
        "didn't answer",
        "isn't linked",
        "closed the tools socket",
        "stopped waiting",
        // The control plane, or the proxy in front of it, restarting (a node before 0.4.2 doesn't retry these).
        "502 Bad Gateway",
        "503 Service Unavailable",
        "504 Gateway Timeout",
    ]
    .iter()
    .any(|s| m.contains(s))
}

pub fn alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else { return false };
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(e) => e == nix::errno::Errno::EPERM,
    }
}

/// Keep a process-tied lease while `pid` lives: hold it over one connection, reconnecting when that drops; release
/// it when the process exits; stop when the lease ends.
async fn keep(socket: &Path, target: &(String, String), mut id: u64, pid: u32) -> anyhow::Result<()> {
    let mut backoff = Duration::from_secs(2);
    loop {
        if !alive(pid) {
            let _ = call(socket, target, "release", json!({"id": id}), false).await;
            return Ok(());
        }
        let holding = id;
        let held = async move {
            let mut conn = open(socket, target).await?;
            conn.call("hold", json!({"id": holding}), false).await
        };
        tokio::pin!(held);
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        let out = loop {
            tokio::select! {
                r = &mut held => break Some(r),
                _ = tick.tick() => if !alive(pid) { break None },
            }
        };
        match out {
            // The process exited: the connection drops with `held`, and the lease is released.
            None => continue,
            // The lease ended (released elsewhere, expired).
            Some(Ok(_)) => return Ok(()),
            Some(Err(e)) => {
                let m = e.to_string();
                // Cut off longer than the authority's grace while the process still works: take a lease again (in
                // line, if someone took the slot meanwhile) and keep that one.
                if m.contains("the process holding it went away") && alive(pid) {
                    match reclaim(socket, target, id, pid).await {
                        Ok(new) => {
                            id = new;
                            backoff = Duration::from_secs(2);
                            continue;
                        }
                        Err(_) => {
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(Duration::from_secs(30));
                            continue;
                        }
                    }
                }
                if m.contains("has ended") || m.contains("no lease #") || m.contains("only its own node") {
                    return Ok(());
                }
                // A blip (the node or control plane restarting): try again within the authority's grace.
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// A new lease like the one that ended (same kind, payload and reason), tied to the same process; its ID once held.
async fn reclaim(socket: &Path, target: &(String, String), old: u64, pid: u32) -> anyhow::Result<u64> {
    let v = call(socket, target, "show", json!({"id": old}), false).await?;
    let e = &v["ended"];
    let args = json!({"kind": e["kind"], "reason": e["reason"], "payload": e["payload"], "wait": true,
        "tie": "process", "pid": pid});
    let v = call(socket, target, "acquire", args, false).await?;
    v["lease"]["id"].as_u64().ok_or_else(|| anyhow::anyhow!("no lease in the answer"))
}

/// `fleetlayer lease list` for people and agents.
fn list_text(v: &Value) -> String {
    use std::fmt::Write;
    let mut o = String::new();
    if let Some(a) = v["authority"].as_str() {
        let _ = writeln!(o, "Leases at {a}:");
    }
    let kinds = v["kinds"].as_array().cloned().unwrap_or_default();
    if kinds.is_empty() {
        let _ = writeln!(o, "No kinds of lease are set up.");
    }
    let line = |l: &Value| -> String {
        let since = l["startedAt"].as_str().or(l["requestedAt"].as_str()).unwrap_or("?");
        let payload = l.get("payload").filter(|p| !p.is_null()).map(|p| format!(" {p}")).unwrap_or_default();
        format!(
            "#{} {} since {since}: {}{payload}",
            l["id"],
            l["holder"].as_str().unwrap_or("?"),
            l["reason"].as_str().unwrap_or("")
        )
    };
    for k in &kinds {
        let held = k["held"].as_array().map_or(0, Vec::len);
        let slots = match k["slots"].as_u64() {
            Some(n) => format!("{held} of {n} slots held"),
            None => format!("{held} held"),
        };
        let approval = if k["approval"] == true { ", each payload approved once" } else { "" };
        let _ = writeln!(
            o,
            "{}: {slots} (ttl {}, idle {}{approval})",
            k["name"].as_str().unwrap_or("?"),
            k["ttl"].as_str().unwrap_or("?"),
            k["idle"].as_str().unwrap_or("?")
        );
        for key in ["held", "waiting", "asking"] {
            for l in k[key].as_array().into_iter().flatten() {
                let extra = match key {
                    "held" => format!(" (until {})", l["expiresAt"].as_str().unwrap_or("?")),
                    "asking" => l["approval"].as_str().map(|a| format!(" ({a})")).unwrap_or_default(),
                    _ => String::new(),
                };
                let _ = writeln!(o, "  {key:<8} {}{extra}", line(l));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_503_on_the_way_to_the_authority_is_tried_again() {
        // What a node before 0.4.2 answered when the control plane's proxy was restarting (28 Nov 2026).
        let t = |m: &str| transient(&anyhow::anyhow!("{m}"));
        assert!(t("leases@agents: HTTP error: 503 Service Unavailable"));
        assert!(t("leases@agents: HTTP error: 502 Bad Gateway"));
        assert!(!t("leases@agents: refused by the access policy"));
        assert!(!t("leases@agents: HTTP error: 404 Not Found"));
    }
}
