//! Tools on another machine. A node can share a stdio tool, such as an MCP server that drives the person's own
//! browser on their laptop, with sessions on other nodes:
//!
//! ```yaml
//! tools:
//!   - name: browser
//!     command: npx
//!     args: ["@playwright/mcp@0.0.86", "--extension", "--session-name", "{session}"]
//!     env_files: { PLAYWRIGHT_MCP_EXTENSION_TOKEN: ~/.config/browser-mcp/extension-token }
//!     allow: ["*@agents"]          # who may use it: <tab>@<node> patterns. Nobody when empty.
//! ```
//!
//! A remote MCP server can be shared the same way, with `url:` in place of `command:` and its credentials in files
//! that only this node's Unix user can read (`crate::mcp_http`); with `check_calls: true` each tool call is checked
//! with the control plane first (`crate::gate`). That's the MCP gate: `docs/access-control.md`.
//!
//! A session elsewhere uses it as its MCP server: `fleetlayer mcp browser@laptop`. That connects to its own node's
//! `node.tools.sock`; the node asks the control plane (`link/tool_open`), which checks access and asks this node to start
//! the tool (`link/tool_serve`) with `{session}` replaced by the caller's tab (`{caller}` by `<tab>@<node>`). Both nodes then open a WebSocket to
//! `/v1/stream/<id>` and the control plane joins them. One process per connection; it's killed when either side
//! closes.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use fleetlayer_protocol::link::{self as l, ToolOpen, ToolOpened, ToolServe};
use fleetlayer_protocol::rpc::RpcError;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::Node;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolConfig {
    pub name: String,
    /// One line for agents choosing a tool (`fleetlayer me` shows it): "Nathan's Chrome, signed in to his sites".
    #[serde(default)]
    pub description: Option<String>,
    /// The program to start (a stdio MCP server), or empty with `url:`.
    #[serde(default)]
    pub command: String,
    /// A remote MCP server (Streamable HTTP) instead of a program: `https://executor.example/mcp`.
    #[serde(default)]
    pub url: Option<String>,
    /// Headers sent to `url` (not secrets: those go in `header_files` or `bearer_file`).
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Headers whose values are read from files when a connection opens (0600 files of this node's Unix user).
    #[serde(default)]
    pub header_files: HashMap<String, std::path::PathBuf>,
    /// A file holding a bearer token for `url`: sent as `Authorization: Bearer <token>`.
    #[serde(default)]
    pub bearer_file: Option<std::path::PathBuf>,
    /// Check every tool call with the control plane before the server sees it: decided as verb `call` on
    /// `tool:<name>/<target>@<node>` by the policy file, audited with its caller, and held for the person's approval
    /// when an `ask:` rule matches (`crate::gate`).
    #[serde(default)]
    pub check_calls: bool,
    /// Argument rules: for a tool name, JSON pointers whose values become more of the call's target (`*` for each
    /// item of a list). Executor: `{call: ["/calls/*/path"]}`, so a call is `call/tools.google.gmail.send`.
    #[serde(default)]
    pub call_targets: std::collections::BTreeMap<String, Vec<String>>,
    /// How long a call that waits for the person is held before the agent is told it's still waiting (seconds,
    /// default 120).
    #[serde(default)]
    pub approval_wait: Option<u64>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Environment variables read from files when the tool starts (a token in a 0600 file, never in node.yaml).
    #[serde(default)]
    pub env_files: HashMap<String, std::path::PathBuf>,
    /// `<tab>@<node>` patterns (`*` as a wildcard) that may use it. Nobody when empty.
    #[serde(default)]
    pub allow: Vec<String>,
    /// For an MCP server: which of its own tools callers may use (`tools/call` by name, `*` as a wildcard).
    #[serde(default)]
    pub tools: Option<ToolFilter>,
    /// Offer it only while this node's link to the control plane goes out from one of these networks (CIDR, such as
    /// `192.168.1.0/24`): a laptop's browser only while the laptop is at home. Empty: on any network.
    /// `fleetlayer tools enable <name> --for 2h` opts in anywhere for a while.
    #[serde(default)]
    pub networks: Vec<String>,
    /// For an MCP server: tool calls made when a connection ends, before the server is stopped, so it cleans up after
    /// the session, and when the node stops. Only after the caller made a tool call: a server may start what the
    /// cleanup means to close at its first call (Playwright MCP connects to Chrome and opens a tab). Sent one at a time,
    /// each waiting for its reply; `repeat` makes a call again until it errors. Playwright MCP in extension mode: `[{name: browser_tabs, arguments: {action: close, index: 0}, repeat: 50}]`.
    #[serde(default)]
    pub on_close: Vec<OnClose>,
    /// Tool calls that run the `on_close` cleanup instead of reaching the server, answered as done. Playwright MCP in
    /// extension mode: `[browser_close]`, which only disconnects there, so a caller that "closes" first leaves its
    /// tabs open and the cleanup at disconnect no longer sees them.
    #[serde(default)]
    pub close_with: Vec<String>,
    /// For a stdio MCP server: start it at the caller's first tool call, answering `initialize` and `tools/list` from
    /// what it answered last time (`lazy.rs`), not when the caller connects. Sessions connect at launch whether or not
    /// they'll use the tool. Default true; false starts it on connect.
    #[serde(default = "yes")]
    pub lazy: bool,
    /// The directory the tool runs in. Default: an empty directory of its own
    /// (`~/.local/state/fleetlayer/tools/<name>`), never the node's, since tools often confine file access to it.
    #[serde(default)]
    pub cwd: Option<std::path::PathBuf>,
}

/// A tool call to make when a connection ends: a name (no arguments), or `{name, arguments}`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum OnClose {
    Name(String),
    Call {
        name: String,
        #[serde(default)]
        arguments: Value,
        /// Make the call up to this many times, stopping at the first error (close tab 0 until there's none left).
        #[serde(default)]
        repeat: Option<u32>,
    },
}

impl OnClose {
    fn call(&self) -> (String, Value, u32) {
        match self {
            OnClose::Name(n) => (n.clone(), serde_json::json!({}), 1),
            OnClose::Call { name, arguments, repeat } => (
                name.clone(),
                if arguments.is_null() { serde_json::json!({}) } else { arguments.clone() },
                repeat.unwrap_or(1).clamp(1, 1000),
            ),
        }
    }
}

impl ToolConfig {
    /// Whether the settings make sense together: a program or a URL, and the rest of what each takes.
    pub fn check(&self) -> Result<(), String> {
        match (&self.url, self.command.is_empty()) {
            (Some(_), false) => return Err("give command: or url:, not both".into()),
            (None, true) => return Err("give command: (a program) or url: (a remote MCP server)".into()),
            (Some(u), true) => {
                let parsed = reqwest::Url::parse(u).map_err(|e| format!("url {u}: {e}"))?;
                // The token goes with every request: in the clear only to this machine.
                let host = parsed.host_str().unwrap_or_default();
                let loopback = host == "localhost"
                    || host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
                match parsed.scheme() {
                    "https" => {}
                    "http" if loopback => {}
                    "http" => {
                        return Err(format!("url {u}: https, or http only to this machine (127.0.0.1, localhost)"));
                    }
                    _ => return Err(format!("url {u}: only https (or http to this machine)")),
                }
                if !self.args.is_empty() || !self.env.is_empty() || !self.env_files.is_empty() {
                    return Err("args, env and env_files are for a program, not a url".into());
                }
            }
            (None, false) => {
                if !self.headers.is_empty() || !self.header_files.is_empty() || self.bearer_file.is_some() {
                    return Err("headers, header_files and bearer_file are for a url, not a program".into());
                }
            }
        }
        if self.storage_caps() {
            return Err("Playwright MCP's storage capability (--caps storage, PLAYWRIGHT_MCP_CAPS) reads and writes every \
                        cookie in the browser: never share it as a tool. The cookies plugin reads a site's cookies with \
                        the person's approval instead (docs/plugins/cookies.md)"
                .into());
        }
        if !self.call_targets.is_empty() && !self.check_calls {
            return Err("call_targets name calls for the gate to check: set check_calls: true".into());
        }
        for (name, pointers) in &self.call_targets {
            if pointers.is_empty() || pointers.iter().any(|p| !p.starts_with('/')) {
                return Err(format!("call_targets.{name}: JSON pointers, each starting with /"));
            }
        }
        Ok(())
    }

    /// Playwright MCP started with its `storage` capability, which offers every cookie of the browser to whoever
    /// calls it: `--caps=storage`, `--caps storage` (in a list with others too) or `PLAYWRIGHT_MCP_CAPS`.
    fn storage_caps(&self) -> bool {
        let has = |v: &str| v.split(',').any(|c| c.trim() == "storage");
        let mut args = self.args.iter();
        while let Some(a) = args.next() {
            if let Some(v) = a.strip_prefix("--caps=") {
                if has(v) {
                    return true;
                }
            } else if a == "--caps" && args.next().is_some_and(|v| has(v)) {
                return true;
            }
        }
        self.env.get("PLAYWRIGHT_MCP_CAPS").is_some_and(|v| has(v))
            || self.env_files.contains_key("PLAYWRIGHT_MCP_CAPS")
    }

    fn wait(&self) -> Duration {
        self.approval_wait.map(Duration::from_secs).unwrap_or(crate::gate::DEFAULT_WAIT).min(Duration::from_secs(600))
    }
}

/// `~/x` in a setting means this node user's home.
fn expand(node: &Node, p: &std::path::Path) -> std::path::PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => node.home.join(rest),
        Err(_) => p.to_path_buf(),
    }
}

/// A remote tool's address and headers, with the secret ones read from their files now.
pub fn upstream(node: &Node, tool: &ToolConfig) -> Result<crate::mcp_http::Upstream, String> {
    let url = tool.url.clone().ok_or("no url")?;
    let mut headers: Vec<(String, String)> = tool.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let read = |f: &std::path::Path| {
        let f = expand(node, f);
        private(&f).map_err(|e| format!("{} can't connect: {e}", tool.name))?;
        std::fs::read_to_string(&f)
            .map(|v| v.trim().to_string())
            .map_err(|e| format!("{} can't connect: reading {}: {e}", tool.name, f.display()))
    };
    for (k, f) in &tool.header_files {
        headers.push((k.clone(), read(f)?));
    }
    if let Some(f) = &tool.bearer_file {
        headers.push(("authorization".into(), format!("Bearer {}", read(f)?)));
    }
    Ok(crate::mcp_http::Upstream { url, headers })
}

fn yes() -> bool {
    true
}

/// A credential file must be this Unix user's and readable by nobody else: otherwise another user (an agent beside the
/// gate) may have read it, or may have put it there.
pub fn private(f: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(f).map_err(|e| format!("reading {}: {e}", f.display()))?;
    if m.uid() != nix::unistd::getuid().as_raw() {
        return Err(format!("{} belongs to another Unix user", f.display()));
    }
    if m.mode() & 0o077 != 0 {
        return Err(format!(
            "{} can be read or changed by other Unix users (mode {:o}): chmod 600 it",
            f.display(),
            m.mode() & 0o777
        ));
    }
    Ok(())
}

/// How long to wait for each cleanup call's reply.
const CLOSE_CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// The first cleanup call can wait behind the server's own start: Playwright MCP connects to the browser at its first
/// tool call, and its extension has 30 seconds to answer.
const FIRST_CLOSE_CALL_TIMEOUT: Duration = Duration::from_secs(35);
/// How long a stopping node waits for its tool connections to clean up. launchd kills a service 20 seconds after
/// asking it to stop.
pub const STOP_CLEANUP: Duration = Duration::from_secs(15);

/// The node's live tool connections. Each one cleans up (`on_close`) when the node stops, not only when its caller
/// leaves, and the `tool.connections` reading counts them, so a leak shows.
pub struct Connections {
    closing: tokio::sync::watch::Sender<bool>,
    changed: tokio::sync::Notify,
    state: std::sync::Mutex<ConnState>,
}

#[derive(Default)]
struct ConnState {
    next: u64,
    live: std::collections::BTreeMap<u64, Live>,
    /// Per tool: connections whose cleanup didn't finish (no reply in time, or the server had gone), since the node
    /// started. Each may have left something open (a browser tab).
    unfinished: std::collections::BTreeMap<String, u64>,
}

struct Live {
    tool: String,
    caller: String,
    since: jiff::Timestamp,
    /// The caller made tool calls the server ran: there may be something to clean up.
    used: bool,
}

impl Default for Connections {
    fn default() -> Self {
        Self {
            closing: tokio::sync::watch::channel(false).0,
            changed: tokio::sync::Notify::new(),
            state: Default::default(),
        }
    }
}

/// One connection's entry in `Connections`, removed when dropped.
pub struct Conn {
    conns: Arc<Connections>,
    id: u64,
    closing: tokio::sync::watch::Receiver<bool>,
    /// Whether the server has run a tool call since it started or last cleaned up.
    used: Used,
}

/// A connection's "used" flag, shared with the tasks that forward checked calls.
#[derive(Clone)]
struct Used {
    conns: Arc<Connections>,
    id: u64,
    flag: Arc<std::sync::atomic::AtomicBool>,
}

impl Used {
    fn get(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn set(&self, used: bool) {
        if self.flag.swap(used, std::sync::atomic::Ordering::SeqCst) != used {
            self.conns.update(self.id, |s, id| {
                if let Some(l) = s.live.get_mut(&id) {
                    l.used = used;
                }
            });
        }
    }
}

impl Connections {
    pub fn open(self: &Arc<Self>, tool: &str, caller: &str) -> Conn {
        let id = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.next += 1;
            let id = s.next;
            let live = Live { tool: tool.into(), caller: caller.into(), since: jiff::Timestamp::now(), used: false };
            s.live.insert(id, live);
            id
        };
        self.changed.notify_one();
        let used = Used { conns: self.clone(), id, flag: Arc::default() };
        Conn { conns: self.clone(), id, closing: self.closing.subscribe(), used }
    }

    /// Tell every connection to clean up and close, and wait up to `within` for them; answers how many were left.
    pub async fn close_all(&self, within: Duration) -> usize {
        self.closing.send_replace(true);
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let left = self.state.lock().unwrap_or_else(|e| e.into_inner()).live.len();
            if left == 0 || tokio::time::Instant::now() >= deadline {
                return left;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The `tool.connections` reading of each tool that has had a connection.
    pub fn readings(&self, tools: &[ToolConfig]) -> Vec<Value> {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let at = jiff::Timestamp::now();
        tools
            .iter()
            .map(|t| {
                let live: Vec<&Live> = s.live.values().filter(|l| l.tool == t.name).collect();
                let sessions: Vec<Value> = live
                    .iter()
                    .map(|l| serde_json::json!({"caller": l.caller, "since": l.since, "used": l.used}))
                    .collect();
                serde_json::json!({"kind": "tool.connections", "key": t.name, "at": at, "data": {
                    "open": live.len(),
                    "used": live.iter().filter(|l| l.used).count(),
                    "unfinishedCleanups": s.unfinished.get(&t.name).copied().unwrap_or(0),
                    "sessions": sessions,
                }})
            })
            .collect()
    }

    fn update(&self, id: u64, f: impl FnOnce(&mut ConnState, u64)) {
        f(&mut self.state.lock().unwrap_or_else(|e| e.into_inner()), id);
        self.changed.notify_one();
    }
}

impl Conn {
    fn unfinished(&self) {
        self.conns.update(self.id, |s, id| {
            if let Some(tool) = s.live.get(&id).map(|l| l.tool.clone()) {
                *s.unfinished.entry(tool).or_default() += 1;
            }
        });
    }

    /// Resolves when the node is stopping.
    async fn stopping(&mut self) {
        let _ = self.closing.wait_for(|c| *c).await;
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.conns.update(self.id, |s, id| {
            s.live.remove(&id);
        });
    }
}

/// Publish each tool's `tool.connections` reading when its connections change (at most every 2 seconds), and every
/// 10 minutes so a reader can tell the figure is fresh.
pub async fn report(node: Arc<Node>) {
    if node.tools.is_empty() {
        return;
    }
    loop {
        let _ = tokio::time::timeout(Duration::from_secs(600), node.tool_connections.changed.notified()).await;
        let readings = node.tool_connections.readings(&node.tools);
        crate::readings::publish(&node, &serde_json::json!({"readings": readings})).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Whether `ip` is inside `cidr` (`10.20.0.0/16`, `fd7a::/48`, or a bare address).
pub fn in_cidr(ip: std::net::IpAddr, cidr: &str) -> bool {
    use std::net::IpAddr;
    let (net, bits) = match cidr.split_once('/') {
        Some((n, b)) => (n, b.parse::<u32>().ok()),
        None => (cidr, None),
    };
    let Ok(net) = net.trim().parse::<IpAddr>() else { return false };
    match (ip, net) {
        (IpAddr::V4(a), IpAddr::V4(n)) => {
            let bits = bits.unwrap_or(32).min(32);
            let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
            u32::from(a) & mask == u32::from(n) & mask
        }
        (IpAddr::V6(a), IpAddr::V6(n)) => {
            let bits = bits.unwrap_or(128).min(128);
            let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits) };
            u128::from(a) & mask == u128::from(n) & mask
        }
        _ => false,
    }
}

/// Opt-ins: a tool offered on any network until a time (`fleetlayer tools enable`).
#[derive(Default)]
pub struct OptIns {
    until: tokio::sync::Mutex<HashMap<String, std::time::Instant>>,
}

impl OptIns {
    pub async fn enable(&self, tool: &str, for_: Duration) {
        self.until.lock().await.insert(tool.to_string(), std::time::Instant::now() + for_);
    }
    pub async fn disable(&self, tool: &str) {
        self.until.lock().await.remove(tool);
    }
    pub async fn active(&self, tool: &str) -> Option<Duration> {
        let until = *self.until.lock().await.get(tool)?;
        until.checked_duration_since(std::time::Instant::now())
    }
}

/// Why the tool isn't offered on this network now, or None when it is.
pub async fn network_refusal(node: &Node, tool: &ToolConfig) -> Option<String> {
    if tool.networks.is_empty() || node.opt_ins.active(&tool.name).await.is_some() {
        return None;
    }
    match node.link.local_ip().await {
        Some(ip) if tool.networks.iter().any(|c| in_cidr(ip, c)) => None,
        Some(ip) => Some(format!(
            "{} offers {} only on its listed networks ({}), and it's on {ip} now. Its person can opt in with \
             'fleetlayer tools enable {} --for 2h' on that machine",
            node.name,
            tool.name,
            tool.networks.join(", "),
            tool.name
        )),
        None => Some(format!("{} can't tell which network it's on, so it doesn't offer {}", node.name, tool.name)),
    }
}

/// Which of an MCP server's tools may be called. A name in `deny` is refused; with `allow` set, so is any name not in
/// it. Refused tools are also left out of `tools/list`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolFilter {
    #[serde(default)]
    pub allow: Option<Vec<String>>,
    #[serde(default)]
    pub deny: Vec<String>,
}

impl ToolFilter {
    pub fn permits(&self, name: &str) -> bool {
        if self.deny.iter().any(|p| glob(p, name)) {
            return false;
        }
        self.allow.as_ref().is_none_or(|a| a.iter().any(|p| glob(p, name)))
    }
}

/// What to do with one line from the caller to an MCP server.
#[derive(Debug, PartialEq)]
pub enum Inbound {
    /// Pass it to the server. `Some(name)` for a tool call, for the log.
    Forward(Option<String>),
    /// Don't pass it; answer the caller with this line instead.
    Refuse(String),
}

/// Look at one JSON-RPC line from the caller: a `tools/call` for a refused tool is answered with an MCP error result.
pub fn inbound(filter: Option<&ToolFilter>, line: &str, node: &str, tool: &str) -> Inbound {
    // Only single JSON objects pass: a batch (an array, which MCP no longer has) or text that isn't JSON could carry a
    // tool call this check never saw, to a server that reads it differently.
    let refuse = |why: &str| {
        Inbound::Refuse(
            serde_json::json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32600, "message": format!("{node}'s \
                shared {tool} takes one JSON-RPC message per line: {why}")}})
            .to_string(),
        )
    };
    if line.trim().is_empty() {
        return Inbound::Forward(None);
    }
    let v = match serde_json::from_str::<Value>(line) {
        Ok(v @ Value::Object(_)) => v,
        Ok(Value::Array(_)) => return refuse("batches aren't accepted"),
        Ok(_) | Err(_) => return refuse("this line isn't a JSON-RPC message"),
    };
    if v["method"] != "tools/call" {
        return Inbound::Forward(None);
    }
    let name = v["params"]["name"].as_str().unwrap_or_default().to_string();
    match filter {
        Some(f) if !f.permits(&name) => Inbound::Refuse(
            serde_json::json!({"jsonrpc": "2.0", "id": v["id"], "result": {"isError": true, "content": [{"type": "text",
                "text": format!("{node} doesn't let callers use the {name} tool of its shared {tool} (its tools: rules). \
                                 If this stops you doing your job, report it to your person.")}]}})
            .to_string(),
        ),
        _ => Inbound::Forward(Some(name)),
    }
}

/// Look at one line from an MCP server to the caller: a `tools/list` result loses the refused tools.
pub fn outbound(filter: Option<&ToolFilter>, line: &str) -> String {
    let Some(f) = filter else { return line.to_string() };
    let Ok(mut v) = serde_json::from_str::<Value>(line) else { return line.to_string() };
    let Some(tools) = v.pointer_mut("/result/tools").and_then(Value::as_array_mut) else { return line.to_string() };
    tools.retain(|t| t["name"].as_str().is_some_and(|n| f.permits(n)));
    v.to_string()
}

/// A tool's arguments for one caller: `{session}` is the caller's tab made safe as a name ([`session_name`]), and
/// `{caller}` is the caller as the control plane recorded it, `<tab>@<node>` (`-@<node>` for a script), for a tool that
/// decides by who calls (the leases authority's shim). Each stays one argument: no shell reads them.
pub fn substitute(args: &[String], session: &str, caller: &str) -> Vec<String> {
    args.iter().map(|a| a.replace("{session}", session).replace("{caller}", caller)).collect()
}

/// A session name safe to pass as an argument: letters, digits, space, `-` and `_`, at most 40 characters.
pub fn session_name(caller: &str) -> String {
    let tab = caller.split('@').next().unwrap_or("session");
    let s: String =
        tab.chars().filter(|c| c.is_ascii_alphanumeric() || " -_".contains(*c)).take(40).collect::<String>();
    let s = s.trim().to_string();
    if s.is_empty() || s == "-" { "session".into() } else { s }
}

/// `*` matches any run of characters.
pub(crate) fn glob(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !text.starts_with(first) || text.len() < first.len() + last.len() || !text.ends_with(last) {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

pub fn allowed(tool: &ToolConfig, caller: &str) -> bool {
    tool.allow.iter().any(|p| glob(p, caller))
}

/// `wss://host/v1/link` → `wss://host/v1/stream/<id>?side=<side>`.
pub(crate) fn stream_url(link_url: &str, id: &str, side: &str) -> String {
    let base = link_url.strip_suffix("/v1/link").unwrap_or(link_url.trim_end_matches('/'));
    format!("{base}/v1/stream/{id}?side={side}")
}

pub(crate) async fn connect_stream(
    node: &Node,
    id: &str,
    side: &str,
) -> anyhow::Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>> {
    let control = node.control.as_ref().ok_or_else(|| anyhow::anyhow!("no control plane configured"))?;
    let token = std::fs::read_to_string(&control.token_file)?;
    let url = node.link.url().await.unwrap_or_else(|| control.url.clone());
    let mut req = stream_url(&url, id, side).into_client_request()?;
    req.headers_mut().insert("authorization", format!("Bearer {}", token.trim()).parse()?);
    let (ws, _) = tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(req))
        .await
        .map_err(|_| anyhow::anyhow!("the control plane didn't answer within 15s"))??;
    Ok(ws)
}

/// How long to wait for the other direction once one has reached its end.
const DRAIN: Duration = Duration::from_secs(10);

/// A stream with nothing received for this long is dead (a machine asleep, a node restarted, a connection half open):
/// both ends ping every 30 seconds, so three pings went unanswered. Closing it ends the caller's `fleetlayer mcp`, and
/// the client fails the call in flight instead of waiting forever. A ping into a half-open connection succeeds, so only
/// what comes back counts.
pub(crate) const IDLE: Duration = Duration::from_secs(90);

/// Copy bytes between a reader/writer pair and a WebSocket (binary frames). WebSocket has no half-close, so the end
/// of one direction travels as an empty binary frame: the other side shuts its writer and keeps sending until its
/// own end. Ends when both directions have ended (or `DRAIN` after the first), or the socket closes.
pub(crate) async fn pipe<R, W, S>(mut reader: R, mut writer: W, ws: S)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    S: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + futures::Sink<Message>
        + Unpin
        + Send
        + 'static,
{
    let (mut ws_tx, mut ws_rx) = ws.split();
    let up = async move {
        let mut buf = vec![0u8; 64 * 1024];
        // A stream can sit idle for a long time (a browser session between tasks); proxies on the way close idle
        // connections, so ping every 30 seconds.
        let mut ping = tokio::time::interval(Duration::from_secs(30));
        ping.tick().await;
        loop {
            tokio::select! {
                r = reader.read(&mut buf) => match r {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if ws_tx.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                            return ws_tx;
                        }
                    }
                },
                _ = ping.tick() => {
                    if ws_tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                        return ws_tx;
                    }
                }
            }
        }
        let _ = ws_tx.send(Message::Binary(Vec::new().into())).await;
        ws_tx
    };
    let down = async move {
        while let Ok(Some(Ok(m))) = tokio::time::timeout(IDLE, ws_rx.next()).await {
            let bytes = match m {
                Message::Binary(b) if b.is_empty() => break,
                Message::Ping(_) | Message::Pong(_) => continue,
                Message::Binary(b) => b.to_vec(),
                Message::Text(t) => t.as_bytes().to_vec(),
                Message::Close(_) => break,
                _ => continue,
            };
            if writer.write_all(&bytes).await.is_err() || writer.flush().await.is_err() {
                break;
            }
        }
        let _ = writer.shutdown().await;
    };
    tokio::pin!(up, down);
    let mut ws_tx = tokio::select! {
        tx = &mut up => {
            let _ = tokio::time::timeout(DRAIN, &mut down).await;
            tx
        }
        _ = &mut down => match tokio::time::timeout(DRAIN, &mut up).await {
            Ok(tx) => tx,
            Err(_) => return,
        },
    };
    let _ = ws_tx.close().await;
}

/// `pipe` for a tool's side of a stream, one JSON-RPC line at a time in both directions: refused tool calls are answered
/// here and never reach the tool, `tools/list` results lose refused tools, and every tool call is logged. With
/// `check_calls`, each call is checked with the control plane first (`crate::gate`), in a task of its own, so one call
/// waiting for the person doesn't hold up the others.
#[allow(clippy::too_many_arguments)]
async fn mcp_pipe<R, W, S>(
    stdout: R,
    stdin: W,
    ws: S,
    config: ToolConfig,
    node: Arc<Node>,
    caller: String,
    stream_id: String,
    mut conn: Conn,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    S: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + futures::Sink<Message>
        + Unpin
        + Send
        + 'static,
{
    let filter = config.tools.clone();
    let tool = config.name.clone();
    let node_name = node.name.clone();
    let stdin = Arc::new(tokio::sync::Mutex::new(stdin));
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Message>(64);
    // Everything to the caller goes through one sender: tool output, refusals and pings.
    let sender = tokio::spawn(async move {
        let mut ping = tokio::time::interval(Duration::from_secs(30));
        ping.tick().await;
        loop {
            tokio::select! {
                m = out_rx.recv() => match m {
                    Some(m) => if ws_tx.send(m).await.is_err() { break },
                    None => break,
                },
                _ = ping.tick() => if ws_tx.send(Message::Ping(Vec::new().into())).await.is_err() { break },
            }
        }
        let _ = ws_tx.close().await;
    });
    // Tool to caller, line by line.
    let down_tx = out_tx.clone();
    let f2 = filter.clone();
    let (down_node, down_ctx) = (node.clone(), format!("MCP result from tool:{tool}@{node_name} to {caller}"));
    // Replies to our own cleanup calls go here, not to the caller.
    let (close_tx, mut close_rx) = tokio::sync::mpsc::channel::<Value>(8);
    let down = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.contains("fleetlayer-close-")
                && let Ok(v) = serde_json::from_str::<Value>(&line)
                && v["id"].as_str().is_some_and(|id| id.starts_with("fleetlayer-close-"))
            {
                let _ = close_tx.send(v).await;
                continue;
            }
            let line = outbound(f2.as_ref(), &line);
            let line = crate::gate::scrub(&down_node, &line, &down_ctx);
            if down_tx.send(Message::Binary(format!("{line}\n").into_bytes().into())).await.is_err() {
                break;
            }
        }
        let _ = down_tx.send(Message::Binary(Vec::new().into())).await;
    });
    let up_ctx = format!("MCP call from {caller} to tool:{tool}@{node_name}");
    let mut checks = tokio::task::JoinSet::new();
    // Caller to tool, line by line.
    let mut buf: Vec<u8> = Vec::new();
    let mut seq = 0u32;
    // Whether the server has run a tool call since it started or last cleaned up. Until then there's nothing to clean
    // up, and a cleanup call would itself start what it means to close: Playwright MCP opens its browser connection,
    // and with it a tab, at its first tool call of any kind.
    let used = conn.used.clone();
    'read: loop {
        let m = tokio::select! {
            r = tokio::time::timeout(IDLE, ws_rx.next()) => match r {
                Ok(Some(Ok(m))) => m,
                _ => break,
            },
            () = conn.stopping() => {
                tracing::info!(tool = %tool, caller = %caller, "the node is stopping: closing the connection");
                break;
            }
        };
        let bytes = match m {
            Message::Binary(b) if b.is_empty() => break,
            Message::Binary(b) => b.to_vec(),
            Message::Text(t) => t.as_bytes().to_vec(),
            Message::Close(_) => break,
            _ => continue,
        };
        buf.extend_from_slice(&bytes);
        while let Some(i) = buf.iter().position(|b| *b == b'\n') {
            let raw: Vec<u8> = buf.drain(..=i).collect();
            let text = String::from_utf8_lossy(&raw).trim_end().to_string();
            // A delivered secret in what the agent sends goes no further.
            let text = crate::gate::scrub(&node, &text, &up_ctx).into_owned();
            // What the server gets is what the checks read: the message as parsed, so a duplicate key can't mean one
            // thing to the gate (which keeps the last) and another to the server.
            let line = format!("{}\n", crate::gate::reencode(&text));
            let mut gated = if config.check_calls { crate::gate::gated(&text, &config.call_targets) } else { None };
            match inbound(filter.as_ref(), &text, &node_name, &tool) {
                Inbound::Forward(Some(n)) if config.close_with.contains(&n) => {
                    // The caller is done: close what it opened, the same way as when it disconnects.
                    let made = if used.get() {
                        let mut w = stdin.lock().await;
                        let (made, finished) = cleanup(&mut *w, &mut close_rx, &config.on_close, &mut seq).await;
                        if !finished {
                            conn.unfinished();
                        }
                        made
                    } else {
                        0
                    };
                    used.set(false);
                    tracing::info!(tool = %tool, caller = %caller, call = %n, calls = made, "closed on the caller's request");
                    let id = serde_json::from_str::<Value>(&text).map(|v| v["id"].clone()).unwrap_or_default();
                    let reply = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text",
                        "text": format!("Closed what this session opened ({made} cleanup calls).")}]}});
                    let _ = out_tx.send(Message::Binary(format!("{reply}\n").into_bytes().into())).await;
                }
                Inbound::Refuse(reply) => {
                    tracing::warn!(tool = %tool, caller = %caller, line = %text, "refused a tool call");
                    let _ = out_tx.send(Message::Binary(format!("{reply}\n").into_bytes().into())).await;
                }
                Inbound::Forward(_) if gated.is_some() => {
                    let Some(g) = gated.take() else { continue };
                    let n = g.label.clone();
                    let (node, out_tx, stdin, used) = (node.clone(), out_tx.clone(), stdin.clone(), used.clone());
                    let (config, caller, stream_id, tool) =
                        (config.clone(), caller.clone(), stream_id.clone(), tool.clone());
                    checks.spawn(async move {
                        let verdict = match g.targets {
                            Err(why) => crate::gate::Verdict::Refused(format!(
                                "{} refused this call: {why}. If this stops you doing your job, report it to your person.",
                                node.name
                            )),
                            Ok(targets) => {
                                let call = fleetlayer_protocol::link::ToolCall {
                                    stream_id: stream_id.clone(),
                                    targets,
                                    args_hash: crate::gate::args_hash(&n, &g.args),
                                    // None when it's too long to show whole: the control plane then won't ask the
                                    // person, rather than show them part of it.
                                    detail: crate::gate::detail(&n, &g.args),
                                    justification: None,
                                    peek: None,
                                    audit: None,
                                };
                                crate::gate::decide(&node, &stream_id, &call, config.wait()).await
                            }
                        };
                        match verdict {
                            crate::gate::Verdict::Allowed => {
                                tracing::info!(tool = %tool, caller = %caller, call = %n, "tool call (checked)");
                                used.set(true);
                                let mut w = stdin.lock().await;
                                let _ = w.write_all(line.as_bytes()).await;
                                let _ = w.flush().await;
                            }
                            crate::gate::Verdict::Refused(why) => {
                                tracing::warn!(tool = %tool, caller = %caller, call = %n, "refused a tool call: {why}");
                                let reply = crate::gate::refusal(&g.id, &why);
                                let _ = out_tx.send(Message::Binary(format!("{reply}\n").into_bytes().into())).await;
                            }
                        }
                    });
                }
                Inbound::Forward(name) => {
                    if let Some(n) = name {
                        tracing::info!(tool = %tool, caller = %caller, call = %n, "tool call");
                        used.set(true);
                    }
                    let mut w = stdin.lock().await;
                    if w.write_all(line.as_bytes()).await.is_err() {
                        break 'read;
                    }
                    let _ = w.flush().await;
                }
            }
        }
    }
    // Calls still being checked are dropped with the caller: nobody is left to answer.
    checks.abort_all();
    // Clean up after the session (close its browser tabs) before the server stops.
    // One call at a time, each waiting for its reply: an MCP server may run calls concurrently, and cleanup calls that
    // race each other leave things behind.
    let mut w = stdin.lock().await;
    if used.get() && !config.on_close.is_empty() {
        let (made, finished) = cleanup(&mut *w, &mut close_rx, &config.on_close, &mut seq).await;
        if finished {
            tracing::info!(tool = %tool, caller = %caller, calls = made, "cleaned up after the session");
        } else {
            conn.unfinished();
            tracing::warn!(tool = %tool, caller = %caller, calls = made,
                "the cleanup after the session didn't finish: it may have left something open");
        }
    }
    drop(conn);
    let _ = w.shutdown().await;
    drop(w);
    drop(stdin);
    let _ = tokio::time::timeout(DRAIN, down).await;
    drop(out_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), sender).await;
}

/// Run the `on_close` calls one at a time, each waiting for its reply. Answers how many were made, and whether the
/// cleanup finished: every call answered, and each repeated call stopped by an error (nothing left to close) rather
/// than by running out of repeats.
async fn cleanup<W: AsyncWrite + Unpin>(
    stdin: &mut W,
    close_rx: &mut tokio::sync::mpsc::Receiver<Value>,
    on_close: &[OnClose],
    seq: &mut u32,
) -> (u32, bool) {
    let mut made = 0u32;
    let mut finished = true;
    'calls: for c in on_close {
        let (name, arguments, times) = c.call();
        let mut stopped = times == 1;
        for _ in 0..times {
            *seq += 1;
            let id = format!("fleetlayer-close-{seq}");
            let call = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": {"name": name, "arguments": arguments}});
            if stdin.write_all(format!("{call}\n").as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                break 'calls;
            }
            made += 1;
            let wait = if made == 1 { FIRST_CLOSE_CALL_TIMEOUT } else { CLOSE_CALL_TIMEOUT };
            match tokio::time::timeout(wait, close_rx.recv()).await {
                Ok(Some(reply)) if reply["error"].is_null() && reply["result"]["isError"] != true => {}
                // An error (no more tabs) ends this call's repeats; a timeout or a closed stream ends cleanup.
                Ok(Some(_)) => {
                    stopped = true;
                    break;
                }
                _ => {
                    finished = false;
                    break 'calls;
                }
            }
        }
        finished &= stopped;
    }
    (made, finished)
}

// ---------------------------------------------------------------- serving a tool this node shares

/// The control plane asks this node to start a tool for a caller.
pub async fn serve(node: Arc<Node>, req: ToolServe) -> Result<Value, RpcError> {
    let tool = node
        .tools
        .iter()
        .find(|t| t.name == req.tool)
        .cloned()
        .ok_or_else(|| RpcError::not_here(format!("{} shares no tool called {}", node.name, req.tool)))?;
    if let Some(why) = network_refusal(&node, &tool).await {
        tracing::warn!(tool = %tool.name, caller = %req.caller, "refused a tool connection: {why}");
        return Err(RpcError::refused(why));
    }
    if !allowed(&tool, &req.caller) {
        tracing::warn!(tool = %tool.name, caller = %req.caller, "refused a tool connection: not in its allow list");
        return Err(RpcError::refused(format!("{} may not use {} on {}", req.caller, tool.name, node.name)));
    }
    let name = session_name(&req.caller);
    if tool.url.is_some() {
        let up = upstream(&node, &tool).map_err(RpcError::refused)?;
        let ws = connect_stream(&node, &req.stream_id, "serve").await.map_err(|e| RpcError::refused(e.to_string()))?;
        tracing::info!(tool = %tool.name, caller = %req.caller, stream = %&req.stream_id[..8], "tool connection opened (remote server)");
        let (r, w) = tokio::io::split(crate::mcp_http::start(up));
        let (caller, stream_id) = (req.caller.clone(), req.stream_id.clone());
        let conn = node.tool_connections.open(&tool.name, &caller);
        tokio::spawn(async move {
            mcp_pipe(r, w, ws, tool.clone(), node.clone(), caller.clone(), stream_id, conn).await;
            tracing::info!(tool = %tool.name, caller = %caller, "tool connection closed");
        });
        return Ok(serde_json::json!({}));
    }
    let args: Vec<String> = substitute(&tool.args, &name, &req.caller);
    let mut env = tool.env.clone();
    for (var, file) in &tool.env_files {
        let file = expand(&node, file);
        let value = std::fs::read_to_string(&file)
            .map_err(|e| RpcError::refused(format!("{} can't start: reading {}: {e}", tool.name, file.display())))?;
        env.insert(var.clone(), value.trim().to_string());
    }
    let cwd = match &tool.cwd {
        Some(c) => expand(&node, c),
        None => node.home.join(".local/state/fleetlayer/tools").join(&tool.name),
    };
    std::fs::create_dir_all(&cwd).map_err(|e| RpcError::refused(format!("{}: {e}", cwd.display())))?;
    // The server starts at the caller's first tool call when what it answered last time is cached (`lazy.rs`).
    let key = crate::lazy::key(&tool.command, &tool.args, &tool.env, &cwd);
    let cache_path = node.home.join(".local/state/fleetlayer/tools").join(format!("{}.mcp-cache.json", tool.name));
    let cache = if tool.lazy { crate::lazy::load(&cache_path, &key) } else { None };
    let spawn = crate::lazy::Spawn { tool: tool.name.clone(), command: tool.command.clone(), args, env, cwd };
    let started = cache.is_none();
    let (stdout, stdin) = crate::lazy::serve(spawn, cache, cache_path, key)
        .map_err(|e| RpcError::refused(format!("starting {}: {e}", tool.command)))?;
    let ws = connect_stream(&node, &req.stream_id, "serve").await.map_err(|e| RpcError::refused(e.to_string()))?;
    tracing::info!(tool = %tool.name, caller = %req.caller, session = %name, stream = %&req.stream_id[..8], started,
        "tool connection opened");
    let (caller, stream_id) = (req.caller.clone(), req.stream_id.clone());
    let conn = node.tool_connections.open(&tool.name, &caller);
    tokio::spawn(async move {
        mcp_pipe(stdout, stdin, ws, tool.clone(), node.clone(), caller.clone(), stream_id, conn).await;
        tracing::info!(tool = %tool.name, caller = %caller, "tool connection closed");
    });
    Ok(serde_json::json!({}))
}

// ---------------------------------------------------------------- using a tool another node shares

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenRequest {
    tool: String,
    node: String,
    #[serde(default)]
    why: Option<String>,
    session_id: Option<String>,
    address: Option<String>,
}

/// `<socket>.tools.sock` beside the node's socket (`node.tools.sock`): a caller (`fleetlayer mcp`) writes one JSON line naming the tool, gets one
/// line back (`{"ok":true}` or `{"error":"…"}`), and the connection then carries the tool's bytes.
/// The socket is already claimed (`socket::claim`).
pub async fn listen(node: Arc<Node>, listener: std::os::unix::net::UnixListener) -> anyhow::Result<()> {
    let listener = UnixListener::from_std(listener)?;
    loop {
        let (stream, _) = listener.accept().await?;
        let node = node.clone();
        tokio::spawn(async move {
            let (r, mut w) = stream.into_split();
            let mut r = BufReader::new(r);
            let mut line = String::new();
            if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            // A page push (`{"pages": …}`) rather than a tool.
            if let Ok(p) = serde_json::from_str::<crate::pages::OpenRequest>(&line) {
                crate::pages::handle(node, p, r, w).await;
                return;
            }
            // A terminal (`{"terminal": …}`) rather than a tool.
            if let Ok(t) = serde_json::from_str::<crate::terminals::OpenRequest>(&line) {
                crate::terminals::handle(node, t, r, w).await;
                return;
            }
            // A shared command's run (`{"command": …}`).
            if let Ok(c) = serde_json::from_str::<crate::commands::OpenRequest>(&line) {
                crate::commands::handle(node, c, r, w).await;
                return;
            }
            match open(&node, &line).await {
                Ok(ws) => {
                    if w.write_all(b"{\"ok\":true}\n").await.is_ok() {
                        pipe(r, w, ws).await;
                    }
                }
                Err(e) => {
                    let msg = serde_json::json!({"error": e}).to_string();
                    let _ = w.write_all(format!("{msg}\n").as_bytes()).await;
                }
            }
        });
    }
}

/// How long a tool connection waits for this node to know the calling session's tab.
const CALLER_WAIT: Duration = Duration::from_secs(10);

/// The calling session's tab, or `-` for a shell. A session starts its MCP servers at launch, often before this node's
/// plugins have listed it (a session herdr resumes after a restart): wait a little for its tab name, which names its
/// tab group on the other machine.
pub(crate) async fn caller_name(node: &Node, session_id: Option<&str>, address: Option<&str>) -> String {
    let deadline = tokio::time::Instant::now() + CALLER_WAIT;
    loop {
        let tab = caller_tab(node, session_id, address).await;
        let named = session_id.is_some() || address.is_some();
        if tab.is_some() || !named || tokio::time::Instant::now() >= deadline {
            return tab.unwrap_or_else(|| "-".into());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The calling session's tab, if this node knows the session by its ID or its address.
async fn caller_tab(node: &Node, session_id: Option<&str>, address: Option<&str>) -> Option<String> {
    let caller = match session_id {
        Some(id) => node.registry.get(id).await,
        None => None,
    };
    let caller = match (caller, address) {
        (Some(c), _) => Some(c),
        (None, Some(a)) => node.registry.by_address(a).await,
        (None, None) => None,
    };
    caller.as_ref().and_then(|c| c.tab().map(str::to_string))
}

async fn open(
    node: &Node,
    line: &str,
) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, String> {
    let req: OpenRequest = serde_json::from_str(line).map_err(|e| format!("unreadable request: {e}"))?;
    let caller_name = caller_name(node, req.session_id.as_deref(), req.address.as_deref()).await;
    let peer = node.link.peer().await.ok_or_else(|| format!("{} isn't linked to a control plane now", node.name))?;
    let opened: ToolOpened = peer
        .request_within(
            l::method::TOOL_OPEN,
            &ToolOpen {
                tool: req.tool.clone(),
                node: req.node.clone(),
                caller: caller_name,
                justification: req.why.clone(),
            },
            Duration::from_secs(30),
        )
        .await
        .map_err(|e| e.message)?;
    connect_stream(node, &opened.stream_id, "use").await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_name_the_caller() {
        let args = vec!["serve".to_string(), "--caller".into(), "{caller}".into(), "--session-name={session}".into()];
        assert_eq!(
            substitute(&args, &session_name("billing tests@laptop"), "billing tests@laptop"),
            vec!["serve", "--caller", "billing tests@laptop", "--session-name=billing tests"]
        );
        assert_eq!(substitute(&args, &session_name("-@laptop"), "-@laptop")[2], "-@laptop");
    }

    #[test]
    fn session_names_are_safe_arguments() {
        assert_eq!(session_name("esign cert@laptop"), "esign cert");
        assert_eq!(session_name("-@laptop"), "session");
        assert_eq!(session_name("rm -rf /;$(x)@n"), "rm -rf x");
        assert_eq!(session_name(&format!("{}@n", "a".repeat(60))).len(), 40);
    }

    #[test]
    fn the_allow_list_is_patterns_and_empty_means_nobody() {
        let mut t = ToolConfig {
            name: "browser".into(),
            description: None,
            command: "x".into(),
            url: None,
            headers: HashMap::new(),
            header_files: HashMap::new(),
            bearer_file: None,
            check_calls: false,
            call_targets: Default::default(),
            approval_wait: None,
            args: vec![],
            env: HashMap::new(),
            env_files: HashMap::new(),
            allow: vec![],
            tools: None,
            networks: vec![],
            on_close: vec![],
            close_with: vec![],
            lazy: true,
            cwd: None,
        };
        assert!(!allowed(&t, "ci@agents"));
        t.allow = vec!["*@agents".into()];
        assert!(allowed(&t, "ci@agents"));
        assert!(!allowed(&t, "ci@agents-triage"));
    }

    #[test]
    fn refused_tools_are_answered_here_and_hidden_from_the_list() {
        let f = ToolFilter { allow: None, deny: vec!["browser_evaluate".into(), "browser_file_*".into()] };
        let call = |n: &str| {
            format!(r#"{{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{{"name":"{n}","arguments":{{}}}}}}"#)
        };
        assert_eq!(
            inbound(Some(&f), &call("browser_navigate"), "laptop", "browser"),
            Inbound::Forward(Some("browser_navigate".into()))
        );
        let Inbound::Refuse(r) = inbound(Some(&f), &call("browser_file_upload"), "laptop", "browser") else { panic!() };
        let v: Value = serde_json::from_str(&r).unwrap();
        assert_eq!((v["id"].as_i64(), v["result"]["isError"].as_bool()), (Some(7), Some(true)));
        assert_eq!(
            inbound(Some(&f), r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#, "l", "b"),
            Inbound::Forward(None)
        );
        assert_eq!(inbound(None, &call("anything"), "l", "b"), Inbound::Forward(Some("anything".into())));
        // A batch, or a line that isn't one JSON object, can't slip a call past the checks.
        let batch = format!("[{}]", call("browser_file_upload"));
        for line in [batch.as_str(), "not json", "7", r#""tools/call""#] {
            let Inbound::Refuse(r) = inbound(Some(&f), line, "l", "b") else { panic!("{line} passed") };
            assert_eq!(serde_json::from_str::<Value>(&r).unwrap()["error"]["code"], -32600);
        }
        let list =
            r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"browser_navigate"},{"name":"browser_evaluate"}]}}"#;
        let out: Value = serde_json::from_str(&outbound(Some(&f), list)).unwrap();
        assert_eq!(out["result"]["tools"].as_array().unwrap().len(), 1);
        let only = ToolFilter { allow: Some(vec!["browser_snapshot".into()]), deny: vec![] };
        assert!(only.permits("browser_snapshot") && !only.permits("browser_navigate"));
    }

    #[test]
    fn a_browser_tool_with_storage_access_is_never_shared() {
        let t = |extra: serde_json::Value| -> ToolConfig {
            let mut v = serde_json::json!({"name": "browser", "command": "node", "args": ["cli.js", "--extension"]});
            v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            serde_json::from_value(v).unwrap()
        };
        assert!(t(serde_json::json!({})).check().is_ok());
        assert!(t(serde_json::json!({"args": ["cli.js", "--caps=vision,pdf"]})).check().is_ok());
        for bad in [
            serde_json::json!({"args": ["cli.js", "--extension", "--caps=storage"]}),
            serde_json::json!({"args": ["cli.js", "--caps", "vision, storage"]}),
            serde_json::json!({"env": {"PLAYWRIGHT_MCP_CAPS": "storage"}}),
            serde_json::json!({"env_files": {"PLAYWRIGHT_MCP_CAPS": "/x"}}),
        ] {
            let e = t(bad.clone()).check().unwrap_err();
            assert!(e.contains("never share it as a tool"), "{bad}: {e}");
        }
    }

    #[test]
    fn a_remote_server_needs_https_and_a_private_token_file() {
        let t = |url: &str| -> ToolConfig {
            serde_json::from_value(serde_json::json!({"name": "executor", "url": url})).unwrap()
        };
        assert!(t("https://executor.example/mcp").check().is_ok());
        assert!(t("http://127.0.0.1:4400/mcp").check().is_ok());
        assert!(t("http://localhost:4400/mcp").check().is_ok());
        assert!(t("http://[::1]:4400/mcp").check().is_ok());
        assert!(t("http://executor.example/mcp").check().is_err());
        assert!(t("http://10.0.0.5/mcp").check().is_err());
        assert!(t("ftp://executor.example/mcp").check().is_err());
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("token");
        std::fs::write(&f, "secret").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(private(&f).unwrap_err().contains("chmod 600"));
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(private(&f).is_ok());
    }

    #[test]
    fn addresses_match_networks() {
        let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
        assert!(in_cidr(ip("10.20.7.20"), "10.20.0.0/16"));
        assert!(!in_cidr(ip("100.101.1.2"), "10.20.0.0/16"));
        assert!(in_cidr(ip("100.101.1.2"), "100.64.0.0/10"));
        assert!(in_cidr(ip("192.168.1.9"), "192.168.1.9"));
        assert!(!in_cidr(ip("192.168.1.9"), "nonsense"));
        assert!(in_cidr(ip("fd7a:115c:a1e0::1"), "fd7a:115c:a1e0::/48"));
        assert!(!in_cidr(ip("10.20.7.20"), "fd7a::/16"));
        assert!(in_cidr(ip("1.2.3.4"), "0.0.0.0/0"));
    }

    #[test]
    fn stream_urls_come_from_the_link_url() {
        assert_eq!(stream_url("wss://c.example/v1/link", "abc", "use"), "wss://c.example/v1/stream/abc?side=use");
    }
}

#[cfg(test)]
mod liveness_tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use std::process::Stdio;
    use tokio::io::{AsyncReadExt, duplex};
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::protocol::Role;

    /// `pipe` between a local stream and one end of an in-memory websocket; returns the local end, the far websocket
    /// end and the pipe's task.
    async fn piped() -> (tokio::io::DuplexStream, WebSocketStream<tokio::io::DuplexStream>, tokio::task::JoinHandle<()>)
    {
        let (near, far) = duplex(1 << 16);
        let ws_near = WebSocketStream::from_raw_socket(near, Role::Client, None).await;
        let ws_far = WebSocketStream::from_raw_socket(far, Role::Server, None).await;
        let (local, pipe_side) = duplex(1 << 16);
        let (r, w) = tokio::io::split(pipe_side);
        let task = tokio::spawn(pipe(r, w, ws_near));
        (local, ws_far, task)
    }

    #[tokio::test(start_paused = true)]
    async fn a_peer_that_stops_answering_ends_the_stream() {
        // The far end never reads or writes: no pongs, no pings, no data, like a machine gone to sleep.
        let (mut local, _far, task) = piped().await;
        let mut buf = [0u8; 16];
        let started = tokio::time::Instant::now();
        assert_eq!(local.read(&mut buf).await.unwrap(), 0, "the local side sees end-of-file");
        assert!(started.elapsed() >= IDLE && started.elapsed() < IDLE + Duration::from_secs(5));
        tokio::time::timeout(DRAIN + Duration::from_secs(1), task).await.expect("the pipe finishes").unwrap();
    }

    /// A watched terminal (`fleetlayer attach --observe`) of an idle agent: no data either way for minutes. The far end
    /// only reads (answering pings, as every websocket peer does) and never pings or sends: this end's own pings and
    /// their pongs keep the stream open.
    #[tokio::test(start_paused = true)]
    async fn a_quiet_terminal_stream_survives_two_minutes() {
        let (mut local, mut far, _task) = piped().await;
        let reader = tokio::spawn(async move { while far.next().await.is_some() {} });
        let mut buf = [0u8; 16];
        let r = tokio::time::timeout(Duration::from_secs(150), local.read(&mut buf)).await;
        assert!(r.is_err(), "still open after two and a half quiet minutes: {r:?}");
        reader.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_quiet_but_live_peer_keeps_the_stream() {
        let (mut local, mut far, _task) = piped().await;
        // The far end pings every 30 s and answers pings, but sends no data for five minutes.
        let keep = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tokio::select! {
                    _ = tick.tick() => if far.send(Message::Ping(Vec::new().into())).await.is_err() { break },
                    m = far.next() => if m.is_none() { break },
                }
            }
        });
        let mut buf = [0u8; 16];
        let r = tokio::time::timeout(Duration::from_secs(300), local.read(&mut buf)).await;
        assert!(r.is_err(), "still open after five quiet minutes: {r:?}");
        keep.abort();
    }

    /// A stand-in MCP server: logs every line it gets, answers cleanup calls with an error (nothing left to close)
    /// and other tool calls with a result.
    const FAKE_SERVER: &str = r#"while IFS= read -r l; do
        printf '%s\n' "$l" >> "$LOG"
        id=$(printf '%s' "$l" | sed -nE 's/.*"id":("[^"]*"|[0-9]+).*/\1/p')
        case "$l" in
          *fleetlayer-close-*) printf '{"jsonrpc":"2.0","id":%s,"result":{"isError":true,"content":[]}}\n' "$id" ;;
          *tools/call*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[]}}\n' "$id" ;;
        esac
      done"#;

    struct Fake {
        caller: WebSocketStream<tokio::io::DuplexStream>,
        task: tokio::task::JoinHandle<()>,
        child: tokio::process::Child,
        conns: Arc<Connections>,
        log: std::path::PathBuf,
        _dir: tempfile::TempDir,
    }

    impl Fake {
        async fn start() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let log = dir.path().join("server.log");
            std::fs::write(&log, "").unwrap();
            let mut child = tokio::process::Command::new("sh")
                .args(["-c", FAKE_SERVER])
                .env("LOG", &log)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let (stdin, stdout) = (child.stdin.take().unwrap(), child.stdout.take().unwrap());
            let (near, far) = duplex(1 << 16);
            let ws_near = WebSocketStream::from_raw_socket(near, Role::Server, None).await;
            let caller = WebSocketStream::from_raw_socket(far, Role::Client, None).await;
            let config: ToolConfig = serde_json::from_value(serde_json::json!({
                "name": "browser", "command": "x",
                "on_close": [{"name": "browser_tabs", "arguments": {"action": "close", "index": 0}, "repeat": 50}],
                "close_with": ["browser_close"],
            }))
            .unwrap();
            let node = Node::for_test(dir.path());
            let conns = node.tool_connections.clone();
            let conn = conns.open("browser", "ci@agents");
            let task =
                tokio::spawn(mcp_pipe(stdout, stdin, ws_near, config, node, "ci@agents".into(), "s".repeat(8), conn));
            Fake { caller, task, child, conns, log, _dir: dir }
        }

        async fn send(&mut self, line: &str) {
            self.caller.send(Message::Binary(format!("{line}\n").into_bytes().into())).await.unwrap();
        }

        /// The next JSON-RPC message the caller gets.
        async fn reply(&mut self) -> Value {
            loop {
                let m =
                    tokio::time::timeout(Duration::from_secs(10), self.caller.next()).await.unwrap().unwrap().unwrap();
                if let Message::Binary(b) = m
                    && !b.is_empty()
                {
                    return serde_json::from_slice(&b).unwrap();
                }
            }
        }

        /// The caller leaves; answers every line the server got.
        async fn leave(mut self) -> String {
            self.caller.send(Message::Binary(Vec::new().into())).await.unwrap();
            self.finish().await
        }

        async fn finish(mut self) -> String {
            tokio::time::timeout(Duration::from_secs(30), &mut self.task).await.unwrap().unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
            std::fs::read_to_string(&self.log).unwrap()
        }
    }

    const LIST: &str = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
    const NAVIGATE: &str = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"url":"about:blank"}}}"#;
    const CLOSE: &str =
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_close","arguments":{}}}"#;

    #[tokio::test]
    async fn a_connection_that_never_called_a_tool_leaves_without_cleanup_calls() {
        // Playwright MCP opens its browser connection (and a tab) at its first tool call, the cleanup's included: a
        // session that only listed the tools must leave the server untouched.
        let mut f = Fake::start().await;
        f.send(LIST).await;
        let log = f.leave().await;
        assert!(log.contains("tools/list"), "{log}");
        assert!(!log.contains("fleetlayer-close-"), "no cleanup call for an unused connection: {log}");
    }

    #[tokio::test]
    async fn a_connection_that_used_the_tool_cleans_up_when_it_leaves() {
        let mut f = Fake::start().await;
        f.send(NAVIGATE).await;
        assert_eq!(f.reply().await["id"], 2);
        let conns = f.conns.clone();
        let log = f.leave().await;
        // One call, stopped by the error that says nothing is left.
        assert_eq!(log.matches("fleetlayer-close-").count(), 1, "{log}");
        let tools: Vec<ToolConfig> = vec![serde_json::from_str(r#"{"name":"browser","command":"x"}"#).unwrap()];
        let r = &conns.readings(&tools)[0];
        assert_eq!(r["data"]["open"], 0);
        assert_eq!(r["data"]["unfinishedCleanups"], 0);
    }

    #[tokio::test]
    async fn close_with_cleans_up_only_what_was_used() {
        let mut f = Fake::start().await;
        f.send(CLOSE).await;
        let r = f.reply().await;
        assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("0 cleanup calls"), "{r}");
        f.send(NAVIGATE).await;
        assert_eq!(f.reply().await["id"], 2);
        f.send(CLOSE).await;
        let r = f.reply().await;
        assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("1 cleanup calls"), "{r}");
        // Already cleaned up, and nothing used since: leaving makes no more calls.
        let log = f.leave().await;
        assert_eq!(log.matches("fleetlayer-close-").count(), 1, "{log}");
        assert!(!log.contains("browser_close"), "browser_close never reaches the server: {log}");
    }

    #[tokio::test]
    async fn a_stopping_node_cleans_up_live_connections_first() {
        let mut f = Fake::start().await;
        f.send(NAVIGATE).await;
        assert_eq!(f.reply().await["id"], 2);
        let conns = f.conns.clone();
        let tools: Vec<ToolConfig> = vec![serde_json::from_str(r#"{"name":"browser","command":"x"}"#).unwrap()];
        assert_eq!(conns.readings(&tools)[0]["data"]["used"], 1);
        assert_eq!(conns.close_all(Duration::from_secs(10)).await, 0, "every connection closed in time");
        let log = f.finish().await;
        assert_eq!(log.matches("fleetlayer-close-").count(), 1, "{log}");
        assert_eq!(conns.readings(&tools)[0]["data"]["open"], 0);
    }
}
