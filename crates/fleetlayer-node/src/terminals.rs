//! Terminals: watching and driving a session's terminal from anywhere in the fleet (`docs/protocol.md`,
//! "Terminals"). The session's manager plugin names a command that streams the terminal in the terminal stream format
//! (herdr's own: `terminal.opened`, `terminal.frame`, `terminal.closed` one way; `terminal.input`, `terminal.resize`,
//! `terminal.release` the other, one JSON object per line). The node runs it and connects it either to a local caller
//! or, for a caller on another node, to a stream the control plane pairs, as it does for shared tools.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use fleetlayer_protocol::ext::{self, TerminalCommand, TerminalCommandRequest, TerminalMode};
use fleetlayer_protocol::link::{self as l, TerminalOpen, TerminalServe, ToolOpened};
use fleetlayer_protocol::rpc::RpcError;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::Node;

/// The first line a local caller writes on the node's tools socket to open a terminal.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenRequest {
    /// `<tab>@<node>`, `<tab>` or a session ID.
    pub terminal: String,
    #[serde(default)]
    pub mode: TerminalMode,
    #[serde(default = "cols")]
    pub cols: u16,
    #[serde(default = "rows")]
    pub rows: u16,
    #[serde(default)]
    pub why: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
}

fn cols() -> u16 {
    120
}
fn rows() -> u16 {
    40
}

/// The command that streams one of this node's sessions' terminals, from its manager plugin.
async fn command(
    node: &Node,
    name: &str,
    mode: TerminalMode,
    cols: u16,
    rows: u16,
) -> Result<TerminalCommand, RpcError> {
    let session = crate::deliver::resolve(node, name).await?;
    let manager = session
        .manager
        .clone()
        .ok_or_else(|| RpcError::refused(format!("{name} on {} has no session manager", node.name)))?;
    let plugin =
        node.plugin(&manager).ok_or_else(|| RpcError::refused(format!("plugin {manager} isn't configured")))?;
    let peer = plugin.peer().await.ok_or_else(|| RpcError::refused(format!("plugin {manager} isn't running")))?;
    let req = TerminalCommandRequest { session_id: session.session_id.clone(), mode, cols, rows };
    peer.request_within(ext::method::TERMINAL_COMMAND, &req, Duration::from_secs(10)).await.map_err(|e| {
        if e.code == fleetlayer_protocol::rpc::code::METHOD_NOT_FOUND {
            RpcError::refused(format!("{manager} can't stream terminals"))
        } else {
            e
        }
    })
}

fn spawn(c: &TerminalCommand) -> Result<tokio::process::Child, RpcError> {
    let mut cmd = tokio::process::Command::new(&c.command);
    cmd.args(&c.args).envs(&c.env);
    for k in &c.env_remove {
        cmd.env_remove(k);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| RpcError::refused(format!("starting {}: {e}", c.command)))
}

/// Control plane → node: stream a terminal here into the stream it paired.
pub async fn serve(node: Arc<Node>, req: TerminalServe) -> Result<Value, RpcError> {
    let c = command(&node, &req.session, req.mode, req.cols, req.rows).await?;
    let mut child = spawn(&c)?;
    let ws = crate::tools::connect_stream(&node, &req.stream_id, "serve")
        .await
        .map_err(|e| RpcError::refused(e.to_string()))?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(RpcError::internal("the terminal command's pipes didn't open"));
    };
    tracing::info!(session = %req.session, caller = %req.caller, mode = ?req.mode, "terminal opened");
    let observe = req.mode == TerminalMode::Observe;
    tokio::spawn(async move {
        // Watching only: nothing the caller sends reaches the terminal, whatever the manager would allow. Its stdin
        // stays open until the end, so the command doesn't take end-of-file for a goodbye.
        if observe {
            crate::tools::pipe(stdout, tokio::io::sink(), ws).await;
            drop(stdin);
        } else {
            crate::tools::pipe(stdout, stdin, ws).await;
        }
        let _ = child.kill().await;
        tracing::info!(session = %req.session, caller = %req.caller, "terminal closed");
    });
    Ok(serde_json::json!({}))
}

/// A local caller's terminal: one of this node's sessions straight from its command, or another node's through the
/// control plane. Answers `{"ok":true}` or `{"error":…}` first, then the stream.
pub async fn handle<R, W>(node: Arc<Node>, req: OpenRequest, reader: R, mut writer: W)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (name, target_node) = crate::deliver::parse_address(&req.terminal);
    let here = target_node.is_none_or(|n| n == node.name);
    if here {
        match command(&node, name, req.mode, req.cols, req.rows).await.and_then(|c| spawn(&c)) {
            Ok(mut child) => {
                let (Some(mut stdin), Some(mut stdout)) = (child.stdin.take(), child.stdout.take()) else { return };
                if writer.write_all(b"{\"ok\":true}\n").await.is_err() {
                    return;
                }
                let mut reader = reader;
                let observe = req.mode == TerminalMode::Observe;
                let up = async move {
                    if observe {
                        // Watching only: read and drop what the caller sends; the end of it still ends the stream.
                        let _ = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await;
                        drop(stdin);
                    } else {
                        let _ = tokio::io::copy(&mut reader, &mut stdin).await;
                    }
                };
                let down = async move {
                    let _ = tokio::io::copy(&mut stdout, &mut writer).await;
                };
                tokio::select! { _ = up => {}, _ = down => {} }
                let _ = child.kill().await;
            }
            Err(e) => {
                let _ = writer.write_all(format!("{}\n", serde_json::json!({"error": e.message})).as_bytes()).await;
            }
        }
        return;
    }
    let target_node = target_node.unwrap_or_default().to_string();
    match open_remote(&node, &req, name, &target_node).await {
        Ok(ws) => {
            if writer.write_all(b"{\"ok\":true}\n").await.is_ok() {
                crate::tools::pipe(reader, writer, ws).await;
            }
        }
        Err(e) => {
            let _ = writer.write_all(format!("{}\n", serde_json::json!({"error": e})).as_bytes()).await;
        }
    }
}

async fn open_remote(
    node: &Node,
    req: &OpenRequest,
    session: &str,
    target_node: &str,
) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, String> {
    let caller = match &req.session_id {
        Some(id) => node.registry.get(id).await,
        None => None,
    };
    let caller = match (caller, &req.address) {
        (Some(c), _) => Some(c),
        (None, Some(a)) => node.registry.by_address(a).await,
        (None, None) => None,
    };
    let caller = caller.as_ref().and_then(|c| c.tab().map(str::to_string)).unwrap_or_else(|| "-".into());
    let peer = node.link.peer().await.ok_or_else(|| format!("{} isn't linked to a control plane now", node.name))?;
    let opened: ToolOpened = peer
        .request_within(
            l::method::TERMINAL_OPEN,
            &TerminalOpen {
                session: session.into(),
                node: target_node.into(),
                caller,
                mode: req.mode,
                cols: req.cols,
                rows: req.rows,
                justification: req.why.clone(),
            },
            Duration::from_secs(30),
        )
        .await
        .map_err(|e| e.message)?;
    crate::tools::connect_stream(node, &opened.stream_id, "use").await.map_err(|e| e.to_string())
}
