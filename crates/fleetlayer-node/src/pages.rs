//! `fleetlayer pages push` (`docs/plugins/pages.md`, "From another machine"). On the pushing machine, the CLI writes
//! one JSON line to `node.tools.sock` (`{"pages": <space>, "name", …}`); this node asks the control plane
//! (`link/pages_open`), which checks `write` on `page:<space>/<name>` and asks the node hosting the space to receive it
//! (`link/pages_serve`). That node runs its pages plugin's `receive` command with its stdio on the stream, so the page
//! format stays the plugin's business. A pages plugin says which spaces it hosts (`_fleetlayer.dev/pages/spaces`),
//! and the node passes the list on (`link/pages_spaces`) when it links and whenever it changes.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use fleetlayer_plugin::Peer;
use fleetlayer_protocol::link::{self as l, PagesOpen, PagesServe, ToolOpened};
use fleetlayer_protocol::rpc::RpcError;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::Node;

/// Every space this node's plugins host, sorted.
pub async fn spaces(node: &Node) -> Vec<String> {
    let mut all = vec![];
    for p in &node.plugins {
        all.extend(p.pages_spaces().await);
    }
    all.sort();
    all.dedup();
    all
}

/// Tells the control plane which spaces this node hosts, if it's linked (else it's sent when it links).
pub async fn announce(node: &Node) {
    if let Some(peer) = node.link.peer().await {
        announce_to(node, &peer).await;
    }
}

pub async fn announce_to(node: &Node, peer: &Peer) {
    peer.notify(l::method::PAGES_SPACES, &json!({ "spaces": spaces(node).await }));
}

// ---------------------------------------------------------------- receiving, on the node that hosts the space

pub async fn serve(node: Arc<Node>, req: PagesServe) -> Result<Value, RpcError> {
    let mut plugin = None;
    for p in &node.plugins {
        if p.pages_spaces().await.contains(&req.space) {
            plugin = Some(p.clone());
            break;
        }
    }
    let plugin =
        plugin.ok_or_else(|| RpcError::not_here(format!("{} hosts no pages space {}", node.name, req.space)))?;
    let mut args = vec!["receive".to_string(), "--space".into(), req.space.clone(), "--name".into(), req.name.clone()];
    args.extend(["--caller".into(), req.caller.clone()]);
    if req.new {
        args.push("--new".into());
    }
    if let Some(d) = &req.date {
        args.extend(["--date".into(), d.clone()]);
    }
    let config = serde_json::to_string(&plugin.config.config).map_err(RpcError::internal)?;
    let mut child = tokio::process::Command::new(&plugin.config.command)
        .args(&args)
        .env("FLEETLAYER_PAGES_CONFIG", config)
        .current_dir(&node.home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| RpcError::refused(format!("starting {} receive: {e}", plugin.config.command)))?;
    let ws = crate::tools::connect_stream(&node, &req.stream_id, "serve")
        .await
        .map_err(|e| RpcError::refused(e.to_string()))?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(RpcError::internal("the receiver's pipes didn't open"));
    };
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!("{line}");
            }
        });
    }
    tracing::info!(space = %req.space, name = %req.name, caller = %req.caller, "receiving a page push");
    tokio::spawn(async move {
        crate::tools::pipe(stdout, stdin, ws).await;
        // A push the other side abandoned mustn't leave the receiver waiting for bytes.
        if tokio::time::timeout(Duration::from_secs(10), child.wait()).await.is_err() {
            let _ = child.kill().await;
        }
    });
    Ok(json!({}))
}

// ---------------------------------------------------------------- pushing, on the CLI's node

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenRequest {
    /// The space.
    pub pages: String,
    pub name: String,
    #[serde(default)]
    pub new: bool,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub why: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
}

/// One push from the CLI: `{"ok":true}` and then the stream's bytes, or `{"error"}`.
pub async fn handle<R, W>(node: Arc<Node>, req: OpenRequest, r: R, mut w: W)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    match open(&node, req).await {
        Ok(ws) => {
            if w.write_all(b"{\"ok\":true}\n").await.is_ok() {
                crate::tools::pipe(r, w, ws).await;
            }
        }
        Err(e) => {
            let _ = w.write_all(format!("{}\n", json!({ "error": e })).as_bytes()).await;
        }
    }
}

async fn open(
    node: &Node,
    req: OpenRequest,
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
    let open =
        PagesOpen { space: req.pages, name: req.name, caller, new: req.new, date: req.date, justification: req.why };
    let opened: ToolOpened =
        peer.request_within(l::method::PAGES_OPEN, &open, Duration::from_secs(30)).await.map_err(|e| {
            if e.code == fleetlayer_protocol::rpc::code::METHOD_NOT_FOUND {
                "the control plane is too old for pages push: update it".to_string()
            } else {
                e.message
            }
        })?;
    crate::tools::connect_stream(node, &opened.stream_id, "use").await.map_err(|e| e.to_string())
}
