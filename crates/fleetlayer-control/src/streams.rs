//! Streams to tools shared by nodes: the control plane joins two WebSockets, one from the caller's node (`use`) and
//! one from the node that runs the tool (`serve`), and copies frames between them. It reads nothing in them. Each
//! stream is opened once, by the two nodes it was made for, within 30 seconds, and logged in `tool_connections`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use tokio::sync::{Mutex, oneshot};

use crate::App;
use crate::db::{self, now_ms};

const PAIR_WITHIN: Duration = Duration::from_secs(30);

struct Pending {
    tenant: String,
    use_node: String,
    serve_node: String,
    /// The first side to arrive waits here for the other's socket.
    waiting: Option<(Side, oneshot::Sender<WebSocket>)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Use,
    Serve,
}

#[derive(Default)]
pub struct Streams {
    pending: Mutex<HashMap<String, Pending>>,
}

impl Streams {
    /// Make a stream for `use_node` to reach a tool on `serve_node`, and log it.
    pub async fn create(
        &self,
        db: &db::Db,
        tenant: &str,
        tool: &str,
        use_node: &str,
        serve_node: &str,
        caller: &str,
    ) -> anyhow::Result<String> {
        let id = db::new_secret();
        sqlx::query(
            "INSERT INTO tool_connections (tenant, stream_id, tool, server_node, caller, opened_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(tenant)
        .bind(&id)
        .bind(tool)
        .bind(serve_node)
        .bind(caller)
        .bind(now_ms())
        .execute(db)
        .await?;
        self.pending.lock().await.insert(
            id.clone(),
            Pending { tenant: tenant.into(), use_node: use_node.into(), serve_node: serve_node.into(), waiting: None },
        );
        Ok(id)
    }

    pub async fn forget(&self, id: &str) {
        self.pending.lock().await.remove(id);
    }
}

#[derive(serde::Deserialize)]
pub struct SideQuery {
    side: Side,
}

pub async fn upgrade(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(q): Query<SideQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(token) =
        headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "))
    else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Ok(Some((tenant, node))) = db::node_for_token(&app.db, token).await else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    {
        let pending = app.streams.pending.lock().await;
        let Some(p) = pending.get(&id) else { return (StatusCode::NOT_FOUND, "no such stream").into_response() };
        let expected = match q.side {
            Side::Use => &p.use_node,
            Side::Serve => &p.serve_node,
        };
        if p.tenant != tenant || *expected != node {
            return (StatusCode::FORBIDDEN, "this stream isn't for this node").into_response();
        }
    }
    ws.on_upgrade(move |socket| join(app, id, q.side, socket))
}

async fn join(app: Arc<App>, id: String, side: Side, socket: WebSocket) {
    let mut pending = app.streams.pending.lock().await;
    let Some(p) = pending.get_mut(&id) else { return };
    match p.waiting.take() {
        Some((other, tx)) if other != side => {
            // The other side is waiting: hand it this socket and let it run the copy.
            pending.remove(&id);
            drop(pending);
            let _ = tx.send(socket);
        }
        Some(w) => {
            // The same side twice: keep the first.
            p.waiting = Some(w);
        }
        None => {
            let (tx, rx) = oneshot::channel();
            p.waiting = Some((side, tx));
            drop(pending);
            let other = match tokio::time::timeout(PAIR_WITHIN, rx).await {
                Ok(Ok(s)) => s,
                _ => {
                    app.streams.forget(&id).await;
                    finish(&app, &id, 0, 0, "the other side never connected").await;
                    return;
                }
            };
            let (use_ws, serve_ws) = match side {
                Side::Use => (socket, other),
                Side::Serve => (other, socket),
            };
            let (to_tool, from_tool, why) = copy(use_ws, serve_ws, app.links.closing()).await;
            finish(&app, &id, to_tool, from_tool, why).await;
        }
    }
}

/// Copy frames both ways, unchanged (an empty binary frame is a node's end-of-input marker, passed on like any other).
/// Ends when both directions have ended, or 30 seconds after the first, or either socket closes. Returns the bytes
/// each way and why it ended. When the control plane shuts down (`closing`), both sides get a "restarting" close frame.
async fn copy(
    use_ws: WebSocket,
    serve_ws: WebSocket,
    closing: tokio::sync::watch::Receiver<bool>,
) -> (i64, i64, &'static str) {
    let (mut use_tx, mut use_rx) = use_ws.split();
    let (mut serve_tx, mut serve_rx) = serve_ws.split();
    let to_tool = Arc::new(AtomicI64::new(0));
    let from_tool = Arc::new(AtomicI64::new(0));
    let up = {
        let n = to_tool.clone();
        let mut closing = closing.clone();
        async move {
            loop {
                let m = tokio::select! {
                    m = use_rx.next() => m,
                    _ = crate::link::until_closing(&mut closing) => {
                        let _ = serve_tx.send(crate::link::restarting()).await;
                        break;
                    }
                };
                let Some(Ok(m)) = m else { break };
                if matches!(m, Message::Close(_)) {
                    break;
                }
                n.fetch_add(len(&m), Ordering::Relaxed);
                if serve_tx.send(m).await.is_err() {
                    break;
                }
            }
            let _ = serve_tx.close().await;
        }
    };
    let down = {
        let n = from_tool.clone();
        let mut closing = closing.clone();
        async move {
            loop {
                let m = tokio::select! {
                    m = serve_rx.next() => m,
                    _ = crate::link::until_closing(&mut closing) => {
                        let _ = use_tx.send(crate::link::restarting()).await;
                        break;
                    }
                };
                let Some(Ok(m)) = m else { break };
                if matches!(m, Message::Close(_)) {
                    break;
                }
                n.fetch_add(len(&m), Ordering::Relaxed);
                if use_tx.send(m).await.is_err() {
                    break;
                }
            }
            let _ = use_tx.close().await;
        }
    };
    tokio::pin!(up, down);
    let why = tokio::select! {
        _ = &mut up => {
            let _ = tokio::time::timeout(Duration::from_secs(30), &mut down).await;
            "the caller closed"
        }
        _ = &mut down => {
            let _ = tokio::time::timeout(Duration::from_secs(30), &mut up).await;
            "the tool closed"
        }
    };
    (to_tool.load(Ordering::Relaxed), from_tool.load(Ordering::Relaxed), why)
}

fn len(m: &Message) -> i64 {
    match m {
        Message::Binary(b) => b.len() as i64,
        Message::Text(t) => t.len() as i64,
        _ => 0,
    }
}

pub async fn finish(app: &App, id: &str, to_tool: i64, from_tool: i64, outcome: &str) {
    tracing::info!(stream = %&id[..8], to_tool, from_tool, outcome, "tool stream ended");
    let _ = sqlx::query(
        "UPDATE tool_connections SET closed_at = ?, bytes_to_tool = ?, bytes_from_tool = ?, outcome = ? WHERE stream_id = ?",
    )
    .bind(now_ms())
    .bind(to_tool)
    .bind(from_tool)
    .bind(outcome)
    .bind(id)
    .execute(&app.db)
    .await;
}

/// The latest tool connections, for `fleetlayer-control tools`.
pub async fn recent(db: &db::Db, tenant: &str, limit: i64) -> anyhow::Result<Vec<serde_json::Value>> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT tool, server_node, caller, opened_at, closed_at, bytes_to_tool, bytes_from_tool, outcome
         FROM tool_connections WHERE tenant = ? ORDER BY opened_at DESC LIMIT ?",
    )
    .bind(tenant)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "tool": r.get::<String, _>(0), "node": r.get::<String, _>(1), "caller": r.get::<String, _>(2),
                "openedAt": r.get::<i64, _>(3), "closedAt": r.get::<Option<i64>, _>(4),
                "bytesToTool": r.get::<i64, _>(5), "bytesFromTool": r.get::<i64, _>(6),
                "outcome": r.get::<Option<String>, _>(7),
            })
        })
        .collect())
}
