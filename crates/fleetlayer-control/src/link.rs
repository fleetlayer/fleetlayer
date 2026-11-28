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
    /// What each node shares, by (tenant, node), from `link/tools`, for `fleetlayer me`. Kept when the node unlinks,
    /// so its tools show as offline; rebuilt as nodes link.
    pub tools: Mutex<HashMap<(String, String), Vec<l::ToolInfo>>>,
    /// The commands each node shares, by (tenant, node), from the same `link/tools` (`fleetlayer commands`).
    pub commands: Mutex<HashMap<(String, String), Vec<l::CommandInfo>>>,
    next: AtomicU64,
    /// True once the control plane is shutting down: links and streams close with "restarting" so nodes reconnect.
    closing: tokio::sync::watch::Sender<bool>,
}

/// Resolves once `closing` turns true (without holding its value across an await, which isn't `Send`).
pub async fn until_closing(closing: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = closing.wait_for(|c| *c).await;
}

/// The close frame links and streams get when the control plane shuts down: 1012, "service restart", so a node
/// reconnects at once.
pub fn restarting() -> Message {
    Message::Close(Some(axum::extract::ws::CloseFrame {
        code: axum::extract::ws::close_code::RESTART,
        reason: "the control plane is restarting".into(),
    }))
}

impl Links {
    /// Start shutting down: every link and stream closes.
    pub fn close_all(&self) {
        self.closing.send_replace(true);
    }

    /// A receiver that turns true when the control plane starts shutting down.
    pub fn closing(&self) -> tokio::sync::watch::Receiver<bool> {
        self.closing.subscribe()
    }

    /// Resolves once the control plane is shutting down.
    pub async fn closed(&self) {
        until_closing(&mut self.closing()).await;
    }

    /// Tell every linked node the control plane now answers at `url`, holding `proof` (`link/moved`). How many were
    /// told.
    pub async fn tell_moved(&self, url: &str, proof: &str) -> usize {
        let m = l::Moved { url: url.to_string(), proof: Some(proof.to_string()) };
        let nodes = self.nodes.lock().await;
        nodes.values().filter(|c| c.peer.notify(l::method::MOVED, &m)).count()
    }

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
    let mut closing = app.links.closing();
    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                frame = out_rx.recv() => match frame {
                    Some(frame) => {
                        if sink.send(Message::Text(frame.into())).await.is_err() {
                            return;
                        }
                    }
                    None => break,
                },
                _ = crate::link::until_closing(&mut closing) => {
                    let _ = sink.send(restarting()).await;
                    break;
                }
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
    let shutting_down = tokio::select! {
        _ = task => false,
        _ = pinger => {
            tracing::warn!(node = %name, "node missed {MISSED_PINGS} pings; dropping its link");
            false
        }
        _ = app.links.closed() => true,
    };
    reader.abort();
    if shutting_down {
        // Let the close frame out, so the node knows to reconnect.
        let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
    } else {
        writer.abort();
    }
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

    /// The node's tenant's version 2 policy, which names the roles.
    async fn policy(&self) -> Result<Arc<crate::policy::Compiled>, RpcError> {
        self.app
            .authz
            .policy(&self.app.db, &self.tenant)
            .await
            .map_err(RpcError::internal)?
            .ok_or_else(|| RpcError::refused("this tenant has no version 2 access policy, so no roles have windows"))
    }

    /// A node subscribes only its own sessions.
    fn own(&self, target: &str) -> Result<(), RpcError> {
        match crate::route::split_target(target) {
            Some((_, node)) if node == self.node => Ok(()),
            _ => Err(RpcError::refused(format!("node {} can act only for its own sessions, not {target}", self.node))),
        }
    }
}

/// Sync deliveries still owed where `node` is the source or the target: `(bundle, source, target)`.
pub async fn owed(app: &App, tenant: &str, node: &str) -> Vec<(String, String, String)> {
    use sqlx::Row;
    sqlx::query("SELECT bundle, source, target FROM sync_pending WHERE tenant = ? AND (target = ? OR source = ?)")
        .bind(tenant)
        .bind(node)
        .bind(node)
        .fetch_all(&app.db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect()
}

/// Ask each online source that owes `node` a bundle (or that `node` owes one to an online target) to seal it again.
pub async fn send_owed(app: &App, tenant: &str, node: &str) {
    for (bundle, source, target) in owed(app, tenant, node).await {
        if app.links.peer(tenant, &target).await.is_none() {
            continue;
        }
        if let Some(peer) = app.links.peer(tenant, &source).await {
            peer.notify(l::method::SYNC_WANTED, &json!({"bundle": bundle, "node": target}));
        }
    }
}

/// The longest a call target may be, and the most targets one call may name.
const TARGET_MAX: usize = 300;
const TARGETS_MAX: usize = 100;

/// `link/tool_call`: the node serving a tool stream asks whether its caller may make a call. The caller and the tool
/// are the stream's, as recorded when it was opened, and only the node serving it may ask.
pub(crate) async fn tool_call(
    app: &App,
    tenant: &str,
    node: &str,
    c: l::ToolCall,
) -> Result<l::ToolCallDecision, RpcError> {
    use sqlx::Row;
    let row = sqlx::query(
        "SELECT tool, caller FROM tool_connections WHERE tenant = ? AND stream_id = ? AND server_node = ? AND closed_at IS NULL",
    )
    .bind(tenant)
    .bind(&c.stream_id)
    .bind(node)
    .fetch_optional(&app.db)
    .await
    .map_err(RpcError::internal)?;
    let Some(row) = row else {
        return Err(RpcError::refused(format!("{node} serves no open tool stream with that ID")));
    };
    let (tool, caller): (String, String) = (row.get(0), row.get(1));
    if let Some(id) = &c.peek {
        return peek_approval(app, tenant, &caller, id).await;
    }
    if c.targets.is_empty() || c.targets.len() > TARGETS_MAX {
        return Err(RpcError::invalid_params(format!("a call names 1 to {TARGETS_MAX} targets")));
    }
    if c.args_hash.len() > 128 || c.detail.as_ref().is_some_and(|d| d.len() > 8192) {
        return Err(RpcError::invalid_params("argsHash or detail is too long"));
    }
    let mut targets = Vec::with_capacity(c.targets.len());
    for t in &c.targets {
        if t.is_empty() || t.len() > TARGET_MAX || t.contains('@') || t.chars().any(char::is_control) {
            return Err(RpcError::invalid_params(format!(
                "a call target is 1 to {TARGET_MAX} characters, with no @ or control characters"
            )));
        }
        targets.push(format!("tool:{tool}/{t}@{node}"));
    }
    let call = crate::authz::CallContext { args_hash: &c.args_hash, detail: c.detail.as_deref(), audit: None };
    let mut reasons = vec![];
    let mut approval = None;
    let mut to_spend = vec![];
    // Every target first, spending nothing; then the approvals `for: once` it relies on, all or none.
    for target in &targets {
        let out =
            crate::authz::check_call(app, tenant, &caller, "call", target, call, c.justification.as_deref()).await?;
        if let Some(e) = out.refusal {
            // The first refusal decides; a wait for approval names the request.
            let decision = if out.pending.is_some() { "pending" } else { "denied" };
            return Ok(l::ToolCallDecision { decision: decision.into(), approval: out.pending, reason: e.message });
        }
        reasons.push(format!("{target}: {}", out.because.join(", ")));
        approval = approval.or(out.pending);
        to_spend.extend(out.to_spend);
    }
    to_spend.sort();
    to_spend.dedup();
    if !crate::approvals::spend_all(&app.db, tenant, &to_spend).await.map_err(RpcError::internal)? {
        let reason =
            "another call with the same arguments used its approval just now: make the call again to ask again";
        for target in &targets {
            let e = crate::audit::Entry {
                decision: "denied".into(),
                subject: caller.clone(),
                verb: "call".into(),
                target: target.clone(),
                reason: reason.into(),
            };
            app.audit.record(&app.db, tenant, e).await.map_err(RpcError::internal)?;
        }
        return Ok(l::ToolCallDecision { decision: "denied".into(), approval: None, reason: reason.into() });
    }
    Ok(l::ToolCallDecision { decision: "allowed".into(), approval, reason: reasons.join("; ") })
}

/// A shared command's name: what node.yaml may call one (the node checks the same).
pub fn valid_command_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && n.starts_with(|c: char| c.is_ascii_alphanumeric())
}

/// `link/command_call`: the node serving a command stream asks whether its caller may run the command with the
/// arguments it was given. The caller and the command are the stream's, as recorded when it was opened, and only the
/// node serving it may ask. Decided as verb `run` on `command:<name>@<node>`; the detail (the program and arguments it
/// will run, and its files whole) goes to the person when asked, and the audit text (files by hash) into the audit log.
pub(crate) async fn command_call(
    app: &App,
    tenant: &str,
    node: &str,
    c: l::ToolCall,
) -> Result<l::ToolCallDecision, RpcError> {
    use sqlx::Row;
    let row = sqlx::query(
        "SELECT tool, caller FROM tool_connections WHERE tenant = ? AND stream_id = ? AND server_node = ? AND closed_at IS NULL",
    )
    .bind(tenant)
    .bind(&c.stream_id)
    .bind(node)
    .fetch_optional(&app.db)
    .await
    .map_err(RpcError::internal)?;
    let Some(row) = row else {
        return Err(RpcError::refused(format!("{node} serves no open command stream with that ID")));
    };
    let (stream, caller): (String, String) = (row.get(0), row.get(1));
    let Some(command) = stream.strip_prefix("command:").filter(|n| valid_command_name(n)) else {
        return Err(RpcError::refused(format!("{node} serves no open command stream with that ID")));
    };
    if let Some(id) = &c.peek {
        return peek_approval(app, tenant, &caller, id).await;
    }
    if !c.targets.is_empty() {
        return Err(RpcError::invalid_params("a command run names no targets: the stream names the command"));
    }
    if c.args_hash.is_empty() || c.args_hash.len() > 128 {
        return Err(RpcError::invalid_params("argsHash is 1 to 128 characters"));
    }
    // What the audit log keeps is always there; the detail the person sees (with a script whole) may not be, when the
    // run's files can't be shown whole: then an `ask:` rule refuses it rather than put a part of it to the person.
    let Some(audit) = c.audit.as_deref().filter(|d| !d.is_empty() && d.len() <= 8192) else {
        return Err(RpcError::invalid_params("a command run carries its audit text (1 to 8192 bytes)"));
    };
    if c.detail.as_ref().is_some_and(|d| d.is_empty() || d.len() > 8192) {
        return Err(RpcError::invalid_params("a command run's detail is 1 to 8192 bytes"));
    }
    let target = format!("command:{command}@{node}");
    let call = crate::authz::CallContext { args_hash: &c.args_hash, detail: c.detail.as_deref(), audit: Some(audit) };
    let out = crate::authz::check_call(app, tenant, &caller, "run", &target, call, c.justification.as_deref()).await?;
    if let Some(e) = out.refusal {
        let decision = if out.pending.is_some() { "pending" } else { "denied" };
        return Ok(l::ToolCallDecision { decision: decision.into(), approval: out.pending, reason: e.message });
    }
    let mut to_spend = out.to_spend;
    to_spend.sort();
    to_spend.dedup();
    if !crate::approvals::spend_all(&app.db, tenant, &to_spend).await.map_err(RpcError::internal)? {
        let reason = "another run with the same arguments used its approval just now: run it again to ask again";
        let e = crate::audit::Entry {
            decision: "denied".into(),
            subject: caller.clone(),
            verb: "run".into(),
            target: target.clone(),
            reason: reason.into(),
        };
        app.audit.record(&app.db, tenant, e).await.map_err(RpcError::internal)?;
        return Ok(l::ToolCallDecision { decision: "denied".into(), approval: None, reason: reason.into() });
    }
    Ok(l::ToolCallDecision {
        decision: "allowed".into(),
        approval: None,
        reason: format!("{target}: {}", out.because.join(", ")),
    })
}

/// A peek at the stream caller's approval request: `pending`, `ready` once approved, or `denied`.
async fn peek_approval(app: &App, tenant: &str, caller: &str, id: &str) -> Result<l::ToolCallDecision, RpcError> {
    use sqlx::Row;
    let row = sqlx::query("SELECT status, expires_at FROM approvals WHERE tenant = ? AND id = ? AND subject = ?")
        .bind(tenant)
        .bind(id)
        .bind(caller)
        .fetch_optional(&app.db)
        .await
        .map_err(RpcError::internal)?;
    let Some(row) = row else { return Err(RpcError::refused(format!("no approval request {id} for {caller}"))) };
    let (status, expires): (String, Option<i64>) = (row.get(0), row.get(1));
    let live = expires.is_some_and(|e| e > crate::db::now_ms());
    let (decision, reason) = match status.as_str() {
        "pending" => ("pending", format!("waiting for the person to approve request {id}")),
        "approved" if live => ("ready", format!("request {id} is approved")),
        "denied" => ("denied", format!("the person refused request {id}")),
        _ => ("denied", format!("request {id} has lapsed: make the call again to ask again")),
    };
    Ok(l::ToolCallDecision { decision: decision.into(), approval: Some(id.to_string()), reason })
}

/// When the person approves a `sync` request (from the hub, the API or `fleetlayer-control approvals approve`, which
/// may run as another process), tell the source node at once, so the refused version goes out without waiting for its
/// next retry. Checked every 15 seconds.
pub async fn sync_approved(app: Arc<App>) {
    use sqlx::Row;
    let mut since = db::now_ms();
    loop {
        tokio::time::sleep(Duration::from_secs(15)).await;
        let now = db::now_ms();
        let rows = sqlx::query(
            "SELECT tenant, subject, target FROM approvals WHERE verb = 'sync' AND status = 'approved'
             AND decided_at > ? AND decided_at <= ?",
        )
        .bind(since)
        .bind(now)
        .fetch_all(&app.db)
        .await;
        let rows = match rows {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("checking new sync approvals: {e}");
                continue;
            }
        };
        since = now;
        for r in rows {
            let (tenant, subject, target): (String, String, String) = (r.get(0), r.get(1), r.get(2));
            if let Some((source, bundle, node)) = approved_sync(&subject, &target)
                && let Some(peer) = app.links.peer(&tenant, source).await
            {
                tracing::info!(bundle, source, target = node, "a sync was approved: asking the source to send it");
                peer.notify(l::method::SYNC_WANTED, &json!({"bundle": bundle, "node": node}));
            }
        }
    }
}

/// `-@<source>` and `sync:<bundle>@<target>` → (source, bundle, target).
fn approved_sync<'a>(subject: &'a str, target: &'a str) -> Option<(&'a str, &'a str, &'a str)> {
    let source = subject.strip_prefix("-@")?;
    let (bundle, node) = target.strip_prefix("sync:")?.rsplit_once('@')?;
    Some((source, bundle, node))
}

#[async_trait]
impl Handler for FromNode {
    async fn request(&self, _peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
        let (app, tenant) = (&self.app, self.tenant.as_str());
        // Fail closed: a method that isn't classified for authorization isn't served.
        if method != ext::method::PING && !crate::authz::LINK_METHODS.iter().any(|(m, _)| *m == method) {
            tracing::warn!(node = %self.node, method, "refused a link method that isn't classified for authorization");
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
                // Bundles owed to or by this node while one side was offline: ask each source that's online to seal
                // a fresh copy for each target that's online.
                let (app2, tenant2, me) = (app.clone(), tenant.to_string(), self.node.clone());
                let watched = h.windows.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    send_owed(&app2, &tenant2, &me).await;
                    // Windows it's in or watches: open ones, and closed ones it hasn't confirmed.
                    if let Err(e) = crate::windows::on_link(&app2, &tenant2, &me, &watched).await {
                        tracing::warn!(node = me, "telling a node about role windows: {e}");
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
                // A plugin's event is from `plugin:<name>@<node>`, so the policy can let a plugin publish where the
                // node's agents may not. The node can claim any of its plugins: it's the trust boundary.
                let (who, source) = publisher(&self.node, p.get("caller").and_then(Value::as_str))?;
                let e: Event = params(p)?;
                crate::authz::check(app, tenant, &who, "publish", &format!("channel:{}", e.channel)).await?;
                let out = crate::route::publish(app, tenant, e, &source).await?;
                answer(&out)
            }
            l::method::SUBSCRIBE => {
                let s: Subscription = params(p)?;
                self.own(&s.target)?;
                crate::authz::check(app, tenant, &s.target, "subscribe", &format!("channel:{}", s.channel)).await?;
                crate::hooks::subscribe(app, tenant, &s).await
            }
            l::method::UNSUBSCRIBE => {
                let s: Subscription = params(p)?;
                self.own(&s.target)?;
                let removed = db::unsubscribe(&app.db, tenant, &s).await.map_err(RpcError::internal)?;
                Ok(json!({"removed": removed}))
            }
            l::method::EVENTS => {
                let r: l::EventsRequest = params(p)?;
                self.own(&r.target)?;
                let limit = i64::from(r.limit.unwrap_or(50).clamp(1, 500));
                let events = db::missed_events(&app.db, tenant, &r.target, r.channel.as_deref(), r.all, r.peek, limit)
                    .await
                    .map_err(RpcError::internal)?;
                Ok(json!({"events": events}))
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
                if t.tool.is_empty() || t.tool.contains([':', '@', '/']) {
                    return Err(RpcError::invalid_params("a tool's name has no : @ or /"));
                }
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
            l::method::TOOL_CALL => {
                let c: l::ToolCall = params(p)?;
                answer(&tool_call(app, tenant, &self.node, c).await?)
            }
            l::method::COMMAND_OPEN => {
                let c: l::CommandOpen = params(p)?;
                if !valid_command_name(&c.command) {
                    return Err(RpcError::invalid_params(
                        "a command's name is 1 to 64 letters, digits, - and _, starting with a letter or digit",
                    ));
                }
                let caller = format!("{}@{}", c.caller, self.node);
                let target = format!("command:{}@{}", c.command, c.node);
                // Refused outright here; asked (and audited with its arguments) when the serving node checks the run
                // it was given (`link/command_call`), so the person sees what it will run.
                if matches!(crate::authz::dry(app, tenant, &caller, "run", &target).await, crate::authz::Dry::Denied) {
                    crate::authz::check_why(app, tenant, &caller, "run", &target, c.justification.as_deref()).await?;
                }
                let server = app
                    .links
                    .peer(tenant, &c.node)
                    .await
                    .ok_or_else(|| RpcError::not_here(format!("node {} is offline", c.node)))?;
                let id = app
                    .streams
                    .create(&app.db, tenant, &format!("command:{}", c.command), &self.node, &c.node, &caller)
                    .await
                    .map_err(RpcError::internal)?;
                let (app2, id2) = (app.clone(), id.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    app2.streams.forget(&id2).await;
                });
                let serve = l::CommandServe {
                    stream_id: id.clone(),
                    command: c.command.clone(),
                    caller,
                    justification: c.justification.clone(),
                };
                if let Err(e) =
                    server.request_within::<_, Value>(l::method::COMMAND_SERVE, &serve, Duration::from_secs(20)).await
                {
                    app.streams.forget(&id).await;
                    crate::streams::finish(app, &id, 0, 0, &format!("refused: {}", e.message)).await;
                    return Err(e);
                }
                answer(&l::ToolOpened { stream_id: id })
            }
            l::method::COMMAND_CALL => {
                let c: l::ToolCall = params(p)?;
                answer(&command_call(app, tenant, &self.node, c).await?)
            }
            l::method::SESSION_CLOSE | l::method::SESSION_ANSWER | l::method::SESSION_COMMAND => {
                let r: l::SessionActRequest = params(p)?;
                let expect = match method {
                    l::method::SESSION_CLOSE => "close",
                    l::method::SESSION_COMMAND => "command",
                    _ => "answer",
                };
                if r.action.verb() != expect {
                    return Err(RpcError::invalid_params(format!("{method} carries action {expect}")));
                }
                answer(&crate::route::act_session(app, tenant, &self.node, r).await?)
            }
            l::method::TRANSCRIPT_SEARCH => {
                answer(&crate::route::search_transcripts(app, tenant, &self.node, params(p)?).await?)
            }
            l::method::TRANSCRIPT_READ => {
                answer(&crate::route::read_transcript(app, tenant, &self.node, params(p)?).await?)
            }
            l::method::SESSION_START => {
                answer(&crate::route::start_session(app, tenant, &self.node, params(p)?).await?)
            }
            l::method::SESSION_HANDOFF => answer(&crate::handoffs::start(app, tenant, &self.node, params(p)?).await?),
            l::method::HANDOFF_GET => answer(&crate::handoffs::get(app, tenant, &self.node, params(p)?).await?),
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
                // `{"summaries": false}` leaves them out: a plugin polling for status changes needs only the sessions.
                let summaries = p.get("summaries").and_then(Value::as_bool).unwrap_or(true);
                let mut fleet = crate::api::fleet_with(app, tenant, summaries).await.map_err(RpcError::internal)?;
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
            l::method::ME => {
                let req: l::MeRequest = params(p)?;
                answer(&crate::me::answer(app, tenant, &self.node, &req.caller).await?)
            }
            l::method::ACCESS_CHECK => {
                let mut req: l::AccessCheck = params(p)?;
                if req.tenant.is_empty() {
                    req.tenant = tenant.to_string();
                }
                if req.to.starts_with("sync:") {
                    // A bundle this node would send: decided where its delivery is (this link's tenant).
                    return answer(&crate::authz::sync_check(app, tenant, &self.node, &req).await?);
                }
                answer(&crate::authz::access_check(app, &self.node, &req).await?)
            }
            l::method::PAGES_OPEN => answer(&crate::pages::open(app, tenant, &self.node, params(p)?).await?),
            l::method::ROLE_ACTIVATE => {
                let req: l::RoleActivate = params(p)?;
                let who = format!("{}@{}", req.caller, self.node);
                let policy = self.policy().await?;
                answer(
                    &crate::windows::request(
                        app,
                        tenant,
                        &policy,
                        &who,
                        &req.role,
                        &req.hold,
                        req.justification.as_deref(),
                    )
                    .await?,
                )
            }
            l::method::ROLE_DEACTIVATE => {
                let req: l::RoleDeactivate = params(p)?;
                let who = format!("{}@{}", req.caller, self.node);
                let policy = self.policy().await?;
                if !policy.may_ask_window(&req.role, &who) {
                    return Err(RpcError::refused(format!(
                        "{who} may not close {}'s window: only its own agents and its `asked_by:` may",
                        req.role
                    )));
                }
                let closed =
                    crate::windows::deactivate(app, tenant, &req.role, &who).await.map_err(RpcError::internal)?;
                answer(&l::RoleActivated {
                    role: req.role.clone(),
                    state: if closed { "closing" } else { "off" }.into(),
                    message: if closed {
                        format!(
                            "closed {}'s window: its nodes are closing its sessions and removing what was delivered",
                            req.role
                        )
                    } else {
                        format!("{}'s window wasn't open", req.role)
                    },
                    ..Default::default()
                })
            }
            l::method::ROLE_WINDOWS => {
                let req: l::RoleWindowsRequest = params(p)?;
                let who = format!("{}@{}", req.caller, self.node);
                let policy = self.policy().await?;
                let windows =
                    crate::windows::list_for(&app.db, tenant, &policy, Some(&who)).await.map_err(RpcError::internal)?;
                answer(&l::RoleWindows { windows })
            }
            l::method::WINDOW_CLOSED => {
                let c: l::WindowClosed = params(p)?;
                crate::windows::confirmed(app, &self.node, &c).await?;
                Ok(json!({}))
            }
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
            l::method::TOOLS => match serde_json::from_value::<l::Tools>(p) {
                Ok(t) => {
                    let key = (tenant.to_string(), node.to_string());
                    self.app.links.tools.lock().await.insert(key.clone(), t.tools);
                    self.app.links.commands.lock().await.insert(key, t.commands);
                    Ok(())
                }
                Err(e) => Err(e.into()),
            },
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

/// Who publishes an event `node` sends, for the policy, and the source its subscribers read: the node itself
/// (`-@<node>`), or one of its plugins when the event names one as `caller` (`plugin:<name>`).
fn publisher(node: &str, caller: Option<&str>) -> Result<(String, String), RpcError> {
    match caller {
        None => Ok((format!("-@{node}"), format!("node {node}"))),
        Some(c) => match c.strip_prefix("plugin:").filter(|n| {
            !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        }) {
            Some(n) => Ok((format!("plugin:{n}@{node}"), format!("{n} on {node}"))),
            None => Err(RpcError::invalid_params("caller is plugin:<name>")),
        },
    }
}

#[cfg(test)]
mod sync_tests {
    use super::*;

    #[test]
    fn a_plugins_event_is_published_as_the_plugin() {
        assert_eq!(publisher("agents", None).unwrap(), ("-@agents".into(), "node agents".into()));
        assert_eq!(
            publisher("agents", Some("plugin:matrix")).unwrap(),
            ("plugin:matrix@agents".into(), "matrix on agents".into())
        );
        assert!(publisher("agents", Some("planning")).is_err(), "only a plugin may be named");
        assert!(publisher("agents", Some("plugin:x@other")).is_err(), "never another node's");
        assert!(publisher("agents", Some("plugin:")).is_err());
    }

    #[test]
    fn an_approved_sync_names_its_source_bundle_and_target() {
        assert_eq!(
            approved_sync("-@laptop", "sync:file-sync/skills@server"),
            Some(("laptop", "file-sync/skills", "server"))
        );
        assert_eq!(approved_sync("ci@laptop", "sync:file-sync/skills@server"), None);
        assert_eq!(approved_sync("-@laptop", "tool:browser@server"), None);
    }

    #[tokio::test]
    async fn deliveries_owed_by_a_source_are_found_when_it_links() {
        let db = crate::db::open_memory().await.unwrap();
        let t = crate::db::DEFAULT_TENANT;
        for (b, src, tgt) in
            [("file-sync/a", "laptop", "server"), ("file-sync/b", "server", "laptop"), ("file-sync/c", "pi", "vps")]
        {
            sqlx::query(
                "INSERT INTO sync_pending (tenant, bundle, source, target, version, since) VALUES (?, ?, ?, ?, 1, 0)",
            )
            .bind(t)
            .bind(b)
            .bind(src)
            .bind(tgt)
            .execute(&db)
            .await
            .unwrap();
        }
        let app = App {
            db,
            links: Default::default(),
            hook_limits: Default::default(),
            admin_token_hash: None,
            streams: Default::default(),
            authz: Default::default(),
            audit: crate::audit::Audit::new(None),
        };
        let mut owed = owed(&app, t, "laptop").await;
        owed.sort();
        assert_eq!(
            owed,
            [
                ("file-sync/a".to_string(), "laptop".to_string(), "server".to_string()),
                ("file-sync/b".to_string(), "server".to_string(), "laptop".to_string())
            ]
        );
    }
}
