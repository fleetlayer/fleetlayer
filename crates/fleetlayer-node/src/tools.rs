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
//! A session elsewhere uses it as its MCP server: `fleetlayer mcp browser@laptop`. That connects to its own node's
//! `node.tools.sock`; the node asks the control plane (`link/tool_open`), which checks access and asks this node to start
//! the tool (`link/tool_serve`) with `{session}` replaced by the caller's tab. Both nodes then open a WebSocket to
//! `/v1/stream/<id>` and the control plane joins them. One process per connection; it's killed when either side
//! closes.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
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
    pub command: String,
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
    /// `10.5.0.0/16`): a laptop's browser only while the laptop is at home. Empty: on any network.
    /// `fleetlayer tools enable <name> --for 2h` opts in anywhere for a while.
    #[serde(default)]
    pub networks: Vec<String>,
    /// For an MCP server: tool calls made when a connection ends, before the server is stopped, so it cleans up after
    /// the session. Sent one at a time, each waiting for its reply; `repeat` makes a call again until it errors.
    /// Playwright MCP in extension mode: `[{name: browser_tabs, arguments: {action: close, index: 0}, repeat: 50}]`.
    #[serde(default)]
    pub on_close: Vec<OnClose>,
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

/// How long to wait for each cleanup call's reply.
const CLOSE_CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether `ip` is inside `cidr` (`10.5.0.0/16`, `fd7a::/48`, or a bare address).
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
    let Ok(v) = serde_json::from_str::<Value>(line) else { return Inbound::Forward(None) };
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

/// A session name safe to pass as an argument: letters, digits, space, `-` and `_`, at most 40 characters.
pub fn session_name(caller: &str) -> String {
    let tab = caller.split('@').next().unwrap_or("session");
    let s: String =
        tab.chars().filter(|c| c.is_ascii_alphanumeric() || " -_".contains(*c)).take(40).collect::<String>();
    let s = s.trim().to_string();
    if s.is_empty() || s == "-" { "session".into() } else { s }
}

/// `*` matches any run of characters.
fn glob(pattern: &str, text: &str) -> bool {
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
fn stream_url(link_url: &str, id: &str, side: &str) -> String {
    let base = link_url.strip_suffix("/v1/link").unwrap_or(link_url.trim_end_matches('/'));
    format!("{base}/v1/stream/{id}?side={side}")
}

async fn connect_stream(
    node: &Node,
    id: &str,
    side: &str,
) -> anyhow::Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>> {
    let control = node.control.as_ref().ok_or_else(|| anyhow::anyhow!("no control plane configured"))?;
    let token = std::fs::read_to_string(&control.token_file)?;
    let mut req = stream_url(&control.url, id, side).into_client_request()?;
    req.headers_mut().insert("authorization", format!("Bearer {}", token.trim()).parse()?);
    let (ws, _) = tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(req))
        .await
        .map_err(|_| anyhow::anyhow!("the control plane didn't answer within 15s"))??;
    Ok(ws)
}

/// How long to wait for the other direction once one has reached its end.
const DRAIN: Duration = Duration::from_secs(10);

/// Copy bytes between a reader/writer pair and a WebSocket (binary frames). WebSocket has no half-close, so the end
/// of one direction travels as an empty binary frame: the other side shuts its writer and keeps sending until its
/// own end. Ends when both directions have ended (or `DRAIN` after the first), or the socket closes.
async fn pipe<R, W, S>(mut reader: R, mut writer: W, ws: S)
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
        while let Some(Ok(m)) = ws_rx.next().await {
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
/// here and never reach the tool, `tools/list` results lose refused tools, and every tool call is logged.
async fn mcp_pipe<S>(
    stdout: tokio::process::ChildStdout,
    mut stdin: tokio::process::ChildStdin,
    ws: S,
    config: ToolConfig,
    node: String,
    caller: String,
) where
    S: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + futures::Sink<Message>
        + Unpin
        + Send
        + 'static,
{
    let filter = config.tools.clone();
    let tool = config.name.clone();
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
            if down_tx.send(Message::Binary(format!("{line}\n").into_bytes().into())).await.is_err() {
                break;
            }
        }
        let _ = down_tx.send(Message::Binary(Vec::new().into())).await;
    });
    // Caller to tool, line by line.
    let mut buf: Vec<u8> = Vec::new();
    while let Some(Ok(m)) = ws_rx.next().await {
        let bytes = match m {
            Message::Binary(b) if b.is_empty() => break,
            Message::Binary(b) => b.to_vec(),
            Message::Text(t) => t.as_bytes().to_vec(),
            Message::Close(_) => break,
            _ => continue,
        };
        buf.extend_from_slice(&bytes);
        while let Some(i) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=i).collect();
            let text = String::from_utf8_lossy(&line);
            match inbound(filter.as_ref(), text.trim_end(), &node, &tool) {
                Inbound::Forward(name) => {
                    if let Some(n) = name {
                        tracing::info!(tool = %tool, caller = %caller, call = %n, "tool call");
                    }
                    if stdin.write_all(&line).await.is_err() {
                        break;
                    }
                    let _ = stdin.flush().await;
                }
                Inbound::Refuse(reply) => {
                    tracing::warn!(tool = %tool, caller = %caller, line = %text.trim_end(), "refused a tool call");
                    let _ = out_tx.send(Message::Binary(format!("{reply}\n").into_bytes().into())).await;
                }
            }
        }
    }
    // Clean up after the session (close its browser tabs) before the server stops.
    // One call at a time, each waiting for its reply: an MCP server may run calls concurrently, and cleanup calls that
    // race each other leave things behind.
    let mut seq = 0u32;
    let mut made = 0u32;
    'calls: for c in &config.on_close {
        let (name, arguments, times) = c.call();
        for _ in 0..times {
            seq += 1;
            let id = format!("fleetlayer-close-{seq}");
            let call = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": {"name": name, "arguments": arguments}});
            if stdin.write_all(format!("{call}\n").as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                break 'calls;
            }
            made += 1;
            match tokio::time::timeout(CLOSE_CALL_TIMEOUT, close_rx.recv()).await {
                Ok(Some(reply)) if reply["error"].is_null() && reply["result"]["isError"] != true => {}
                // An error (no more tabs) ends this call's repeats; a timeout or a closed stream ends cleanup.
                Ok(Some(_)) => break,
                _ => break 'calls,
            }
        }
    }
    if made > 0 {
        tracing::info!(tool = %tool, caller = %caller, calls = made, "cleaned up after the session");
    }
    let _ = stdin.shutdown().await;
    drop(stdin);
    let _ = tokio::time::timeout(DRAIN, down).await;
    drop(out_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), sender).await;
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
    let args: Vec<String> = tool.args.iter().map(|a| a.replace("{session}", &name)).collect();
    let mut env = tool.env.clone();
    for (var, file) in &tool.env_files {
        let file = match file.strip_prefix("~") {
            Ok(rest) => node.home.join(rest),
            Err(_) => file.clone(),
        };
        let value = std::fs::read_to_string(&file)
            .map_err(|e| RpcError::refused(format!("{} can't start: reading {}: {e}", tool.name, file.display())))?;
        env.insert(var.clone(), value.trim().to_string());
    }
    let cwd = match &tool.cwd {
        Some(c) => match c.strip_prefix("~") {
            Ok(rest) => node.home.join(rest),
            Err(_) => c.clone(),
        },
        None => node.home.join(".local/state/fleetlayer/tools").join(&tool.name),
    };
    std::fs::create_dir_all(&cwd).map_err(|e| RpcError::refused(format!("{}: {e}", cwd.display())))?;
    let mut child = tokio::process::Command::new(&tool.command)
        .args(&args)
        .envs(&env)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| RpcError::refused(format!("starting {}: {e}", tool.command)))?;
    let ws = connect_stream(&node, &req.stream_id, "serve").await.map_err(|e| RpcError::refused(e.to_string()))?;
    tracing::info!(tool = %tool.name, caller = %req.caller, session = %name, stream = %&req.stream_id[..8], "tool connection opened");
    let (stdin, stdout, stderr) = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
        return Err(RpcError::internal("the tool's pipes didn't open"));
    };
    if let Some(stderr) = stderr {
        let tool_name = tool.name.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(tool = %tool_name, "{line}");
            }
        });
    }
    let caller = req.caller.clone();
    tokio::spawn(async move {
        mcp_pipe(stdout, stdin, ws, tool.clone(), node.name.clone(), caller.clone()).await;
        let _ = child.kill().await;
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
pub async fn listen(node: Arc<Node>, path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
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

async fn open(
    node: &Node,
    line: &str,
) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, String> {
    let req: OpenRequest = serde_json::from_str(line).map_err(|e| format!("unreadable request: {e}"))?;
    let caller = match &req.session_id {
        Some(id) => node.registry.get(id).await,
        None => None,
    };
    let caller = match (caller, &req.address) {
        (Some(c), _) => Some(c),
        (None, Some(a)) => node.registry.by_address(a).await,
        (None, None) => None,
    };
    let caller_name = caller.as_ref().and_then(|c| c.tab().map(str::to_string)).unwrap_or_else(|| "-".into());
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
            command: "x".into(),
            args: vec![],
            env: HashMap::new(),
            env_files: HashMap::new(),
            allow: vec![],
            tools: None,
            networks: vec![],
            on_close: vec![],
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
        let list =
            r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"browser_navigate"},{"name":"browser_evaluate"}]}}"#;
        let out: Value = serde_json::from_str(&outbound(Some(&f), list)).unwrap();
        assert_eq!(out["result"]["tools"].as_array().unwrap().len(), 1);
        let only = ToolFilter { allow: Some(vec!["browser_snapshot".into()]), deny: vec![] };
        assert!(only.permits("browser_snapshot") && !only.permits("browser_navigate"));
    }

    #[test]
    fn addresses_match_networks() {
        let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
        assert!(in_cidr(ip("10.5.7.20"), "10.5.0.0/16"));
        assert!(!in_cidr(ip("100.101.1.2"), "10.5.0.0/16"));
        assert!(in_cidr(ip("100.101.1.2"), "100.64.0.0/10"));
        assert!(in_cidr(ip("192.168.1.9"), "192.168.1.9"));
        assert!(!in_cidr(ip("192.168.1.9"), "nonsense"));
        assert!(in_cidr(ip("fd7a:115c:a1e0::1"), "fd7a:115c:a1e0::/48"));
        assert!(!in_cidr(ip("10.5.7.20"), "fd7a::/16"));
        assert!(in_cidr(ip("1.2.3.4"), "0.0.0.0/0"));
    }

    #[test]
    fn stream_urls_come_from_the_link_url() {
        assert_eq!(stream_url("wss://c.example/v1/link", "abc", "use"), "wss://c.example/v1/stream/abc?side=use");
    }
}
