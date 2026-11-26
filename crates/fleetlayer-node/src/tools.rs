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
//! `tools.sock`; the node asks the control plane (`link/tool_open`), which checks access and asks this node to start
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
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if ws_tx.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
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

// ---------------------------------------------------------------- serving a tool this node shares

/// The control plane asks this node to start a tool for a caller.
pub async fn serve(node: Arc<Node>, req: ToolServe) -> Result<Value, RpcError> {
    let tool = node
        .tools
        .iter()
        .find(|t| t.name == req.tool)
        .cloned()
        .ok_or_else(|| RpcError::not_here(format!("{} shares no tool called {}", node.name, req.tool)))?;
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
    let mut child = tokio::process::Command::new(&tool.command)
        .args(&args)
        .envs(&env)
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
        pipe(stdout, stdin, ws).await;
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
    session_id: Option<String>,
    address: Option<String>,
}

/// `tools.sock` beside the node's socket: a caller (`fleetlayer mcp`) writes one JSON line naming the tool, gets one
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
            &ToolOpen { tool: req.tool.clone(), node: req.node.clone(), caller: caller_name },
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
        };
        assert!(!allowed(&t, "ci@agents"));
        t.allow = vec!["*@agents".into()];
        assert!(allowed(&t, "ci@agents"));
        assert!(!allowed(&t, "ci@agents-triage"));
    }

    #[test]
    fn stream_urls_come_from_the_link_url() {
        assert_eq!(stream_url("wss://c.example/v1/link", "abc", "use"), "wss://c.example/v1/stream/abc?side=use");
    }
}
