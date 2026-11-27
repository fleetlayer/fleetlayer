//! The node's link to the control plane: one outbound WebSocket (so it works behind NAT, with no port open on the
//! machine), JSON-RPC both ways, reconnecting with backoff. The node pushes its sessions and their changes; the
//! control plane asks it to deliver messages and events to its sessions.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fleetlayer_plugin::{Handler, Peer, answer, params};
use fleetlayer_protocol::ext::{self, Deliver};
use fleetlayer_protocol::link::{self as l, DeliverRequest, Hello, SessionGone, SessionPush, SessionsPush, Welcome};
use fleetlayer_protocol::rpc::RpcError;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::Node;
use crate::config::ControlConfig;
use crate::sessions::Change;

const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Notifications held while the link is down (summaries), sent in order when it's back. The oldest go first when
/// it's full: the plugin's files keep everything anyway.
const HOLD_LIMIT: usize = 5000;

#[derive(Default)]
pub struct Link {
    peer: Mutex<Option<Peer>>,
    last_error: Mutex<Option<String>>,
    held: Mutex<std::collections::VecDeque<(String, Value)>>,
    /// The local address the link goes out from, while linked: which network this machine is on now.
    local_ip: Mutex<Option<std::net::IpAddr>>,
}

impl Link {
    pub async fn peer(&self) -> Option<Peer> {
        self.peer.lock().await.clone().filter(|p| !p.is_closed())
    }

    /// Send a notification to the control plane now, or hold it until the link is back.
    pub async fn send_or_hold(&self, method: &str, params: Value) {
        if let Some(peer) = self.peer().await
            && peer.notify(method, &params)
        {
            return;
        }
        let mut held = self.held.lock().await;
        if held.len() == HOLD_LIMIT {
            held.pop_front();
        }
        held.push_back((method.to_string(), params));
    }

    async fn flush(&self, peer: &Peer) {
        let mut held = self.held.lock().await;
        while let Some((m, p)) = held.pop_front() {
            if !peer.notify(&m, &p) {
                held.push_front((m, p));
                return;
            }
        }
    }

    /// The local address the link goes out from, while linked.
    pub async fn local_ip(&self) -> Option<std::net::IpAddr> {
        self.peer().await?;
        *self.local_ip.lock().await
    }

    pub async fn state(&self) -> Value {
        json!({"connected": self.peer().await.is_some(), "lastError": *self.last_error.lock().await})
    }
}

/// Stay linked for as long as the node runs.
pub async fn run(node: Arc<Node>, cfg: ControlConfig) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match connect_once(&node, &cfg).await {
            Ok(()) => {
                backoff = Duration::from_secs(1);
                tracing::warn!("the control plane link closed; reconnecting");
            }
            Err(e) => {
                tracing::warn!("linking to the control plane: {e}");
                *node.link.last_error.lock().await = Some(e.to_string());
            }
        }
        *node.link.peer.lock().await = None;
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn connect_once(node: &Arc<Node>, cfg: &ControlConfig) -> anyhow::Result<()> {
    let token = std::fs::read_to_string(&cfg.token_file)
        .map_err(|e| anyhow::anyhow!("reading the node token {}: {e}", cfg.token_file.display()))?;
    let mut req = cfg.url.as_str().into_client_request()?;
    req.headers_mut().insert("authorization", format!("Bearer {}", token.trim()).parse()?);
    let (ws, _) = tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(req))
        .await
        .map_err(|_| anyhow::anyhow!("no answer from {} within 15s", cfg.url))??;
    let local = match ws.get_ref() {
        tokio_tungstenite::MaybeTlsStream::Plain(s) => s.local_addr().ok(),
        tokio_tungstenite::MaybeTlsStream::Rustls(s) => s.get_ref().0.local_addr().ok(),
        _ => None,
    };
    *node.link.local_ip.lock().await = local.map(|a| a.ip());
    let (mut sink, mut stream) = ws.split();
    let (in_tx, in_rx) = mpsc::channel::<String>(256);
    let (out_tx, mut out_rx) = mpsc::channel::<String>(256);
    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if sink.send(Message::Text(frame.into())).await.is_err() {
                break;
            }
        }
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
    let (peer, task) = fleetlayer_plugin::connect_frames(in_rx, out_tx, Arc::new(FromControl { node: node.clone() }));

    let hello = Hello {
        protocol: fleetlayer_protocol::PROTOCOL,
        version: env!("CARGO_PKG_VERSION").into(),
        machine: node.machine.clone(),
        user: node.user.clone(),
        public_key: Some(node.public_key.clone()),
    };
    let welcome: Welcome = peer.request(l::method::HELLO, &hello).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    if welcome.node != node.name {
        tracing::warn!(
            "the control plane knows this node as {}, and node.yaml calls it {}; set name: {} in node.yaml",
            welcome.node,
            node.name,
            welcome.node
        );
    }
    tracing::info!(url = %cfg.url, node = %welcome.node, "linked to the control plane");
    *node.link.peer.lock().await = Some(peer.clone());
    *node.link.last_error.lock().await = None;

    // The whole list first, then every change. A watcher that falls behind sends the whole list again.
    let mut changes = node.registry.subscribe();
    push_all(node, &peer).await;
    node.link.flush(&peer).await;
    crate::readings::resubscribe(node, &peer).await;
    crate::sync::relinked(node).await;
    crate::pages::announce_to(node, &peer).await;
    crate::me::announce_to(node, &peer).await;
    let pusher = {
        let (node, peer) = (node.clone(), peer.clone());
        tokio::spawn(async move {
            loop {
                match changes.recv().await {
                    Ok(Change::Reset) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        push_all(&node, &peer).await
                    }
                    Ok(Change::Updated { session }) => {
                        peer.notify(
                            l::method::SESSION,
                            &SessionPush { session: serde_json::to_value(&session).unwrap_or_default() },
                        );
                    }
                    Ok(Change::Removed { session_id }) => {
                        peer.notify(l::method::SESSION_GONE, &SessionGone { session_id });
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        })
    };
    let _ = task.await;
    pusher.abort();
    reader.abort();
    writer.abort();
    Ok(())
}

async fn push_all(node: &Node, peer: &Peer) {
    let sessions = node.registry.all().await.iter().filter_map(|s| serde_json::to_value(s).ok()).collect();
    peer.notify(l::method::SESSIONS, &SessionsPush { sessions });
}

struct FromControl {
    node: Arc<Node>,
}

#[async_trait]
impl Handler for FromControl {
    async fn request(&self, _peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
        match method {
            ext::method::PING => Ok(json!({})),
            l::method::SYNC_DELIVER => crate::sync::deliver(&self.node, params(p)?).await,
            l::method::TOOL_SERVE => crate::tools::serve(self.node.clone(), params(p)?).await,
            l::method::TERMINAL_SERVE => crate::terminals::serve(self.node.clone(), params(p)?).await,
            l::method::PAGES_SERVE => crate::pages::serve(self.node.clone(), params(p)?).await,
            l::method::SESSION_SERVE => crate::start::serve(&self.node, params(p)?).await,
            l::method::CATCHUP => {
                self.node.ask_observer(ext::method::SUMMARY_CATCHUP, &p, Duration::from_secs(480)).await
            }
            l::method::DELIVER => {
                let r: DeliverRequest = params(p)?;
                let session = crate::deliver::resolve(&self.node, &r.name).await?;
                let d = Deliver {
                    session_id: session.session_id.clone(),
                    address: session.meta.address.clone().unwrap_or_default(),
                    delivery_id: r.delivery_id,
                    kind: r.kind,
                    subkind: r.subkind,
                    untrusted: r.untrusted,
                    from: r.from,
                    text: r.text,
                };
                answer(&crate::deliver::deliver(&self.node, &session, &d).await?)
            }
            _ => Err(RpcError::method_not_found(method)),
        }
    }

    async fn notification(&self, _peer: &Peer, method: &str, p: Value) {
        if method == l::method::READING_UPDATE {
            crate::readings::update(&self.node, &p).await;
            return;
        }
        if method == l::method::SYNC_WANTED
            && let (Some(bundle), Some(node)) = (p["bundle"].as_str(), p["node"].as_str())
        {
            crate::sync::wanted(&self.node, bundle, node).await;
        }
    }
}
