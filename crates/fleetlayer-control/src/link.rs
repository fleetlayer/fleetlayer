//! Nodes' links: one WebSocket per node at `/v1/link`, authenticated by the node's token, carrying JSON-RPC both ways
//! (`docs/link.md`). The control plane keeps each connected node's peer, so messages and events can be delivered to
//! sessions on it.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use fleetlayer_plugin::{Handler, Peer, answer, params};
use fleetlayer_protocol::ext::{self, Event};
use fleetlayer_protocol::link::{self as l, Hello, SendRequest, SessionGone, SessionPush, SessionsPush, Welcome};
use fleetlayer_protocol::rpc::RpcError;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

use crate::App;
use crate::db::{self, Subscription};

const PING_EVERY: Duration = Duration::from_secs(30);
const MISSED_PINGS: u32 = 3;

pub struct NodeConn {
    pub peer: Peer,
    pub conn: u64,
}

#[derive(Default)]
pub struct Links {
    pub nodes: Mutex<HashMap<(String, String), NodeConn>>,
    /// The reading kinds each linked node subscribed to.
    pub reading_kinds: Mutex<HashMap<(String, String), Vec<String>>>,
    /// Which node hosts each pages space, by (tenant, space), from `link/pages_spaces`. Kept when the node unlinks, so
    /// a push can say the space's node is offline.
    pub pages: Mutex<HashMap<(String, String), String>>,
    next: AtomicU64,
}

impl Links {
    pub async fn peer(&self, tenant: &str, node: &str) -> Option<Peer> {
        self.nodes.lock().await.get(&(tenant.to_string(), node.to_string())).map(|c| c.peer.clone())
    }

    pub async fn online(&self, tenant: &str) -> Vec<String> {
        self.nodes.lock().await.keys().filter(|(t, _)| t == tenant).map(|(_, n)| n.clone()).collect()
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get("authorization")?.to_str().ok()?.strip_prefix("Bearer ")
}

pub async fn upgrade(State(app): State<Arc<App>>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    let Some(token) = bearer(&headers) else {
        return (StatusCode::UNAUTHORIZED, "a node token is needed").into_response();
    };
    let node = match db::node_for_token(&app.db, token).await {
        Ok(Some(n)) => n,
        Ok(None) => return (StatusCode::UNAUTHORIZED, "unknown node token").into_response(),
        Err(e) => {
            tracing::error!("looking up a node token: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    ws.on_upgrade(move |socket| run(app, node, socket))
}

async fn run(app: Arc<App>, (tenant, name): (String, String), socket: WebSocket) {
    let conn = app.links.next.fetch_add(1, Ordering::Relaxed);
    let (mut sink, mut stream) = socket.split();
    let (in_tx, in_rx) = mpsc::channel::<String>(256);
    let (out_tx, mut out_rx) = mpsc::channel::<String>(256);
    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if sink.send(Message::Text(frame.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });
    let reader = tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            match msg {
                Message::Text(t) => {
                    if in_tx.send(t.to_string()).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });
    let handler = Arc::new(FromNode { app: app.clone(), tenant: tenant.clone(), node: name.clone() });
    let (peer, task) = fleetlayer_plugin::connect_frames(in_rx, out_tx, handler);

    if let Some(old) =
        app.links.nodes.lock().await.insert((tenant.clone(), name.clone()), NodeConn { peer: peer.clone(), conn })
    {
        tracing::info!(node = %name, "a new link replaces the old one");
        drop(old);
    }
    tracing::info!(node = %name, tenant = %tenant, "node linked");

    let pinger = {
        let peer = peer.clone();
        let (db, tenant, name) = (app.db.clone(), tenant.clone(), name.clone());
        tokio::spawn(async move {
            let mut missed = 0;
            loop {
                tokio::time::sleep(PING_EVERY).await;
                match peer.request_within::<_, Value>(ext::method::PING, &json!({}), Duration::from_secs(10)).await {
                    Ok(_) => {
                        missed = 0;
                        // "Last seen" follows a linked node, not only the moment it linked.
                        if let Err(e) = db::node_alive(&db, &tenant, &name).await {
                            tracing::warn!(node = %name, "recording that a node answered: {e}");
                        }
                    }
                    Err(_) => {
                        missed += 1;
                        if missed >= MISSED_PINGS {
                            return;
                        }
                    }
                }
            }
        })
    };
    tokio::select! {
        _ = task => {}
        _ = pinger => tracing::warn!(node = %name, "node missed {MISSED_PINGS} pings; dropping its link"),
    }
    reader.abort();
    writer.abort();
    let mut nodes = app.links.nodes.lock().await;
    if nodes.get(&(tenant.clone(), name.clone())).is_some_and(|c| c.conn == conn) {
        nodes.remove(&(tenant.clone(), name.clone()));
        app.links.reading_kinds.lock().await.remove(&(tenant.clone(), name.clone()));
    }
    tracing::info!(node = %name, "node unlinked");
}

struct FromNode {
    app: Arc<App>,
    tenant: String,
    node: String,
}

impl FromNode {
    /// This node as a subject, for requests not made by one of its sessions.
    fn me(&self) -> String {
        format!("-@{}", self.node)
    }

    /// A node subscribes only its own sessions.
    fn own(&self, target: &str) -> Result<(), RpcError> {
        match crate::route::split_target(target) {
            Some((_, node)) if node == self.node => Ok(()),
            _ => {
                Err(RpcError::refused(format!("node {} can subscribe only its own sessions, not {target}", self.node)))
            }
        }
    }
}

#[async_trait]
impl Handler for FromNode {
    async fn request(&self, _peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
        let (app, tenant) = (&self.app, self.tenant.as_str());
        // Fail closed: a method that isn't classified for authorization isn't served.
        if method != ext::method::PING && !crate::authz::LINK_METHODS.iter().any(|(m, _)| *m == method) {
            return Err(RpcError::method_not_found(method));
        }
        match method {
            ext::method::PING => Ok(json!({})),
            l::method::HELLO => {
                let h: Hello = params(p)?;
                if h.protocol != fleetlayer_protocol::PROTOCOL {
                    return Err(RpcError::refused(format!(
                        "this control plane speaks protocol {}, the node {}",
                        fleetlayer_protocol::PROTOCOL,
                        h.protocol
                    )));
                }
                if let Some(key) = &h.public_key {
                    db::check_key(&app.db, tenant, &self.node, key)
                        .await
                        .map_err(RpcError::internal)?
                        .map_err(RpcError::refused)?;
                }
                db::node_seen(&app.db, tenant, &self.node, &h.machine, &h.user, &h.version)
                    .await
                    .map_err(RpcError::internal)?;
                // Bundles this node missed while offline: ask each source to seal a fresh copy.
                let (app2, tenant2, me) = (app.clone(), tenant.to_string(), self.node.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let rows = sqlx::query("SELECT bundle, source FROM sync_pending WHERE tenant = ? AND target = ?")
                        .bind(&tenant2)
                        .bind(&me)
                        .fetch_all(&app2.db)
                        .await
                        .unwrap_or_default();
                    for r in rows {
                        use sqlx::Row;
                        let (bundle, source): (String, String) = (r.get(0), r.get(1));
                        if let Some(peer) = app2.links.peer(&tenant2, &source).await {
                            peer.notify(l::method::SYNC_WANTED, &json!({"bundle": bundle, "node": me}));
                        }
                    }
                });
                answer(&Welcome { node: self.node.clone() })
            }
            l::method::SEND => {
                let s: SendRequest = params(p)?;
                let delivered = crate::route::send(app, tenant, &self.node, s).await?;
                answer(&delivered)
            }
            l::method::PUBLISH => {
                let e: Event = params(p)?;
                crate::authz::check(app, tenant, &self.me(), "publish", &format!("channel:{}", e.channel)).await?;
                let out = crate::route::publish(app, tenant, e, &format!("node {}", self.node)).await?;
                answer(&out)
            }
            l::method::SUBSCRIBE => {
                let s: Subscription = params(p)?;
                self.own(&s.target)?;
                crate::authz::check(app, tenant, &s.target, "subscribe", &format!("channel:{}", s.channel)).await?;
                db::subscribe(&app.db, tenant, &s).await.map_err(RpcError::internal)?;
                Ok(json!({}))
            }
            l::method::UNSUBSCRIBE => {
                let s: Subscription = params(p)?;
                self.own(&s.target)?;
                let removed = db::unsubscribe(&app.db, tenant, &s).await.map_err(RpcError::internal)?;
                Ok(json!({"removed": removed}))
            }
            l::method::SUBSCRIPTIONS => {
                let mut subs = db::subscriptions(&app.db, tenant).await.map_err(RpcError::internal)?;
                let me = self.me();
                let mut visible = vec![];
                for s in subs.drain(..) {
                    if s.target.ends_with(&format!("@{}", self.node))
                        || crate::authz::may_read(app, tenant, &me, &s.target).await
                    {
                        visible.push(s);
                    }
                }
                answer(&json!({"subscriptions": visible}))
            }
            l::method::SYNC_PUBLISH => {
                let s: l::SyncPublishSealed = params(p)?;
                let mut results = vec![];
                for copy in s.copies {
                    let target = format!("sync:{}@{}", s.bundle, copy.node);
                    let from = format!("-@{}", self.node);
                    let r = if let Err(e) = crate::authz::check(app, tenant, &from, "sync", &target).await {
                        Err(e.message)
                    } else {
                        match app.links.peer(tenant, &copy.node).await {
                            None => {
                                sqlx::query(
                                    "INSERT INTO sync_pending (tenant, bundle, source, target, version, since) VALUES (?, ?, ?, ?, ?, ?)
                                     ON CONFLICT (tenant, bundle, target) DO UPDATE SET source = excluded.source, version = excluded.version",
                                )
                                .bind(tenant)
                                .bind(&s.bundle)
                                .bind(&self.node)
                                .bind(&copy.node)
                                .bind(s.version as i64)
                                .bind(crate::db::now_ms())
                                .execute(&app.db)
                                .await
                                .map_err(RpcError::internal)?;
                                Err(format!("node {} is offline; it gets this version when it's back", copy.node))
                            }
                            Some(peer) => {
                                let d = l::SyncDeliver {
                                    bundle: s.bundle.clone(),
                                    version: s.version,
                                    from: self.node.clone(),
                                    sealed: copy.sealed,
                                };
                                match peer
                                    .request_within::<_, Value>(l::method::SYNC_DELIVER, &d, Duration::from_secs(60))
                                    .await
                                {
                                    Ok(v) if v["applied"].as_bool() == Some(true) || v["reason"].is_string() => {
                                        let _ = sqlx::query(
                                            "DELETE FROM sync_pending WHERE tenant = ? AND bundle = ? AND target = ?",
                                        )
                                        .bind(tenant)
                                        .bind(&s.bundle)
                                        .bind(&copy.node)
                                        .execute(&app.db)
                                        .await;
                                        Ok(())
                                    }
                                    Ok(v) => Err(v["error"].as_str().unwrap_or("not applied").to_string()),
                                    Err(e) => Err(e.message),
                                }
                            }
                        }
                    };
                    results.push(l::TargetResult { target: copy.node, ok: r.is_ok(), error: r.err() });
                }
                answer(&json!({"results": results}))
            }
            l::method::READING_SUBSCRIBE | l::method::READING_LATEST => {
                let kinds: Vec<String> =
                    serde_json::from_value(p["kinds"].clone()).map_err(RpcError::invalid_params)?;
                if method == l::method::READING_SUBSCRIBE {
                    app.links.reading_kinds.lock().await.insert((tenant.to_string(), self.node.clone()), kinds.clone());
                }
                let me = self.me();
                let mut out = vec![];
                for r in crate::readings::latest(&app.db, tenant, &kinds).await.map_err(RpcError::internal)? {
                    let src = r["node"].as_str().unwrap_or_default().to_string();
                    if src == self.node || crate::authz::may_read(app, tenant, &me, &format!("-@{src}")).await {
                        out.push(r);
                    }
                }
                answer(&json!({"readings": out}))
            }
            l::method::TOOL_OPEN => {
                let t: l::ToolOpen = params(p)?;
                let caller = format!("{}@{}", t.caller, self.node);
                let target = format!("tool:{}@{}", t.tool, t.node);
                crate::authz::check_why(app, tenant, &caller, "use_tool", &target, t.justification.as_deref()).await?;
                let server = app
                    .links
                    .peer(tenant, &t.node)
                    .await
                    .ok_or_else(|| RpcError::not_here(format!("node {} is offline", t.node)))?;
                let id = app
                    .streams
                    .create(&app.db, tenant, &t.tool, &self.node, &t.node, &caller)
                    .await
                    .map_err(RpcError::internal)?;
                // A stream nobody opens is forgotten.
                let (app2, id2) = (app.clone(), id.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    app2.streams.forget(&id2).await;
                });
                let serve = l::ToolServe { stream_id: id.clone(), tool: t.tool.clone(), caller };
                if let Err(e) =
                    server.request_within::<_, Value>(l::method::TOOL_SERVE, &serve, Duration::from_secs(20)).await
                {
                    app.streams.forget(&id).await;
                    crate::streams::finish(app, &id, 0, 0, &format!("refused: {}", e.message)).await;
                    return Err(e);
                }
                answer(&l::ToolOpened { stream_id: id })
            }
            l::method::TERMINAL_OPEN => {
                let t: l::TerminalOpen = params(p)?;
                let caller = format!("{}@{}", t.caller, self.node);
                let target = format!("{}@{}", t.session, t.node);
                let verb = t.mode.verb();
                crate::authz::check_why(app, tenant, &caller, verb, &target, t.justification.as_deref()).await?;
                let server = app
                    .links
                    .peer(tenant, &t.node)
                    .await
                    .ok_or_else(|| RpcError::not_here(format!("node {} is offline", t.node)))?;
                let id = app
                    .streams
                    .create(&app.db, tenant, &format!("terminal:{}", t.session), &self.node, &t.node, &caller)
                    .await
                    .map_err(RpcError::internal)?;
                let (app2, id2) = (app.clone(), id.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    app2.streams.forget(&id2).await;
                });
                let serve = l::TerminalServe {
                    stream_id: id.clone(),
                    session: t.session.clone(),
                    mode: t.mode,
                    cols: t.cols,
                    rows: t.rows,
                    caller,
                };
                if let Err(e) =
                    server.request_within::<_, Value>(l::method::TERMINAL_SERVE, &serve, Duration::from_secs(20)).await
                {
                    app.streams.forget(&id).await;
                    crate::streams::finish(app, &id, 0, 0, &format!("refused: {}", e.message)).await;
                    return Err(e);
                }
                answer(&l::ToolOpened { stream_id: id })
            }
            l::method::SUMMARIES_SEARCH => {
                let q = p["q"].as_str().unwrap_or_default();
                let limit = p["limit"].as_i64().unwrap_or(20).clamp(1, 200);
                let hits = if let Some(s) = p["session"].as_str() {
                    crate::summaries::timeline(&app.db, tenant, s, p["limit"].as_i64().unwrap_or(10_000)).await
                } else if let Some(m) = p["mentions"].as_str() {
                    crate::summaries::mentions(&app.db, tenant, m, limit).await
                } else {
                    crate::summaries::search(&app.db, tenant, q, limit).await
                }
                .map_err(RpcError::internal)?;
                // Only the sessions this node may read (and its own).
                let me = self.me();
                let mut visible = vec![];
                for h in hits {
                    let node = h["node"].as_str().unwrap_or_default().to_string();
                    let name = h["tab"].as_str().or(h["sessionId"].as_str()).unwrap_or_default().to_string();
                    if node == self.node || crate::authz::may_read(app, tenant, &me, &format!("{name}@{node}")).await {
                        visible.push(h);
                    }
                }
                answer(&json!({"summaries": visible}))
            }
            l::method::FLEET => {
                let mut fleet = crate::api::fleet(app, tenant).await.map_err(RpcError::internal)?;
                let me = self.me();
                if let Some(list) = fleet["sessions"].as_array_mut() {
                    let all = std::mem::take(list);
                    for s in all {
                        let node = s["node"].as_str().unwrap_or_default().to_string();
                        let name =
                            s["meta"]["tab"].as_str().or(s["sessionId"].as_str()).unwrap_or_default().to_string();
                        let target = format!("{name}@{node}");
                        if node == self.node || crate::authz::may_see(app, tenant, &me, &target).await {
                            list.push(s);
                        }
                    }
                }
                // Nodes too: one this node may read nothing of (not even `-@<node>`) isn't shown at all.
                if let Some(nodes) = fleet["nodes"].as_array_mut() {
                    let all = std::mem::take(nodes);
                    for n in all {
                        let name = n["name"].as_str().unwrap_or_default().to_string();
                        if name == self.node || crate::authz::may_read(app, tenant, &me, &format!("-@{name}")).await {
                            nodes.push(n);
                        }
                    }
                }
                answer(&fleet)
            }
            l::method::ACCESS_CHECK => {
                let req: l::AccessCheck = params(p)?;
                answer(&crate::authz::access_check(app, &self.node, &req).await?)
            }
            l::method::PAGES_OPEN => answer(&crate::pages::open(app, tenant, &self.node, params(p)?).await?),
            _ => Err(RpcError::method_not_found(method)),
        }
    }

    async fn notification(&self, _peer: &Peer, method: &str, p: Value) {
        let (db, tenant, node) = (&self.app.db, self.tenant.as_str(), self.node.as_str());
        let r = match method {
            l::method::SESSIONS => match serde_json::from_value::<SessionsPush>(p) {
                Ok(s) => db::replace_sessions(db, tenant, node, &s.sessions).await,
                Err(e) => Err(e.into()),
            },
            l::method::SESSION => match serde_json::from_value::<SessionPush>(p) {
                Ok(s) => db::upsert_session(db, tenant, node, &s.session).await,
                Err(e) => Err(e.into()),
            },
            l::method::SESSION_GONE => match serde_json::from_value::<SessionGone>(p) {
                Ok(s) => db::remove_session(db, tenant, node, &s.session_id).await,
                Err(e) => Err(e.into()),
            },
            l::method::READINGS => {
                let readings = p["readings"].as_array().cloned().unwrap_or_default();
                crate::readings::publish(&self.app, tenant, node, readings).await
            }
            l::method::PAGES_SPACES => {
                let spaces: Vec<String> = serde_json::from_value(p["spaces"].clone()).unwrap_or_default();
                crate::pages::claim(&self.app, tenant, node, &spaces).await;
                Ok(())
            }
            l::method::SUMMARY => {
                match serde_json::from_value::<fleetlayer_protocol::summary::SummaryEvent>(p["event"].clone()) {
                    Ok(e) => crate::summaries::store(db, tenant, node, &e).await,
                    Err(e) => Err(e.into()),
                }
            }
            _ => Ok(()),
        };
        if let Err(e) = r {
            tracing::warn!(node, method, "{e}");
        }
    }
}
