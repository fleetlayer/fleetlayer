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
/// The control plane pings every linked node every 30 seconds. A link that hears nothing at all for this long (no
/// ping, no other frame) died without saying so: a laptop that slept, a NAT that forgot the connection. The node then
/// drops it and links again, with the usual backoff, instead of waiting for a close that never comes.
pub const READ_DEADLINE: Duration = Duration::from_secs(100);

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
    /// The control plane's link URL in use now, when it differs from node.yaml's (it moved, and saving failed).
    url: Mutex<Option<String>>,
    /// Where the control plane said it moved to (`link/moved`): tried first, with the old address as the fallback,
    /// until the move is confirmed (then saved) or an hour passes (then dropped).
    moved_to: Mutex<Option<Moving>>,
}

#[derive(Debug, Clone)]
struct Moving {
    url: String,
    proof: String,
    since: std::time::Instant,
}

/// How long a `link/moved` address is tried before the node gives up on it and stays with the one it has.
const MOVED_TRY_FOR: Duration = Duration::from_secs(3600);

/// Whether the node follows a `link/moved` from `current` to `new`: a `ws://` or `wss://` link URL, never from
/// `wss://` down to plain `ws://`, and with a proof to check.
pub fn follows_move(current: &str, new: &str, proof: Option<&str>) -> Result<(), &'static str> {
    if !(new.starts_with("ws://") || new.starts_with("wss://")) {
        return Err("not a ws:// or wss:// address");
    }
    if current.starts_with("wss://") && new.starts_with("ws://") {
        return Err("it would drop TLS (wss:// to ws://)");
    }
    if proof.is_none_or(|p| p.len() < 32 || !p.bytes().all(|b| b.is_ascii_hexdigit())) {
        return Err("it carries no proof to check the new address against");
    }
    Ok(())
}

/// The control plane's web address for a link URL: `wss://c.example/v1/link` → `https://c.example`.
fn web_base(link: &str) -> String {
    let u = link.replacen("wss://", "https://", 1).replacen("ws://", "http://", 1);
    u.strip_suffix("/v1/link").unwrap_or(&u).trim_end_matches('/').to_string()
}

/// Whether the address holds the moved control plane's state: `Some(confirmed)`, or None (wrong place, or not up).
async fn check_proof(m: &Moving) -> Option<bool> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(5)).build().ok()?;
    // The proof is hex (`follows_move` checks), so it goes in the URL as it is.
    let r = client.get(format!("{}/v1/move-proof?nonce={}", web_base(&m.url), m.proof)).send().await.ok()?;
    if !r.status().is_success() {
        return None;
    }
    r.json::<Value>().await.ok()?["confirmed"].as_bool()
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

    /// The control plane's link URL now: a moved one once linked there, else None (node.yaml's).
    pub async fn url(&self) -> Option<String> {
        self.url.lock().await.clone()
    }

    async fn moved_to(&self) -> Option<Moving> {
        let mut m = self.moved_to.lock().await;
        if m.as_ref().is_some_and(|x| x.since.elapsed() > MOVED_TRY_FOR) {
            tracing::warn!("the control plane's move was never confirmed; staying with the address in node.yaml");
            *m = None;
        }
        m.clone()
    }

    pub async fn state(&self) -> Value {
        json!({"connected": self.peer().await.is_some(), "lastError": *self.last_error.lock().await})
    }
}

/// Stay linked for as long as the node runs. After `link/moved`, the new address is tried first, but only once it
/// shows the move's proof (so the token never goes anywhere else), and the address in node.yaml stays the fallback
/// until the move is confirmed: a move that's undone costs nothing.
pub async fn run(node: Arc<Node>, mut cfg: ControlConfig) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if let Some(u) = node.link.url().await {
            cfg.url = u;
        }
        let moving = node.link.moved_to().await.filter(|m| m.url != cfg.url);
        let mut urls = vec![];
        if let Some(m) = &moving {
            match check_proof(m).await {
                Some(_) => urls.push(m.url.clone()),
                None => {
                    tracing::info!(url = %m.url, "the control plane's new address doesn't show the move's proof yet")
                }
            }
        }
        urls.push(cfg.url.clone());
        for url in urls {
            let attempt = ControlConfig { url: url.clone(), ..cfg.clone() };
            match connect_once(&node, &attempt).await {
                Ok(()) => {
                    backoff = Duration::from_secs(1);
                    tracing::warn!("the control plane link closed; reconnecting");
                    break;
                }
                Err(e) => {
                    tracing::warn!(%url, "linking to the control plane: {e}");
                    *node.link.last_error.lock().await = Some(e.to_string());
                }
            }
        }
        *node.link.peer.lock().await = None;
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Linked at a moved control plane's address: wait for the move to be confirmed there (it passed its health check),
/// then keep the address, in node.yaml when it can be written. Until then node.yaml keeps the old one.
fn watch_confirmation(node: Arc<Node>, m: Moving) {
    tokio::spawn(async move {
        while m.since.elapsed() < MOVED_TRY_FOR {
            if node.link.moved_to.lock().await.as_ref().is_none_or(|x| x.url != m.url) {
                return;
            }
            if check_proof(&m).await == Some(true) {
                confirmed(&node, &m.url).await;
                return;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });
}

async fn confirmed(node: &Node, url: &str) {
    *node.link.moved_to.lock().await = None;
    *node.link.url.lock().await = Some(url.to_string());
    match crate::config::set_control_url(&node.config_path, url) {
        Ok(()) => {
            tracing::info!(%url, path = %node.config_path.display(), "the control plane moved: saved its new address")
        }
        Err(e) => tracing::warn!(
            %url,
            "the control plane moved, but its new address couldn't be saved to {} ({e}): set control.url there by hand",
            node.config_path.display()
        ),
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
    let scrub_node = node.clone();
    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            // Everything this node sends the control plane passes here: no delivered secret leaves in it.
            let frame = if scrub_node.secrets.scrubber().finds(&frame) {
                let context = crate::secrets::frame_context(&frame);
                crate::secrets::scrub(&scrub_node, &frame, &context).into_owned()
            } else {
                frame
            };
            if sink.send(Message::Text(frame.into())).await.is_err() {
                break;
            }
        }
    });
    let url = cfg.url.clone();
    let reader = tokio::spawn(async move {
        if read_frames(&mut stream, &in_tx, READ_DEADLINE).await == ReadEnd::Silent {
            tracing::warn!(%url, "nothing from the control plane for {}s; linking again", READ_DEADLINE.as_secs());
        }
    });
    let (peer, task) = fleetlayer_plugin::connect_frames(in_rx, out_tx, Arc::new(FromControl { node: node.clone() }));

    let hello = Hello {
        protocol: fleetlayer_protocol::PROTOCOL,
        version: env!("CARGO_PKG_VERSION").into(),
        machine: node.machine.clone(),
        user: node.user.clone(),
        public_key: Some(node.public_key.clone()),
        windows: node.windows.watched(),
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
    let moving = node.link.moved_to.lock().await.clone();
    if let Some(m) = moving.filter(|m| m.url == cfg.url) {
        watch_confirmation(node.clone(), m);
    }
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

#[derive(Debug, PartialEq, Eq)]
enum ReadEnd {
    /// The control plane closed the link, or it failed.
    Closed,
    /// Nothing arrived within the deadline.
    Silent,
}

/// Pass the link's text frames on until it closes or stays silent for `deadline`. Any frame counts as a sign of life
/// (a WebSocket ping too); only text frames carry JSON-RPC.
async fn read_frames<S, E>(stream: &mut S, to: &mpsc::Sender<String>, deadline: Duration) -> ReadEnd
where
    S: futures::Stream<Item = Result<Message, E>> + Unpin,
{
    loop {
        let msg = match tokio::time::timeout(deadline, stream.next()).await {
            Err(_) => return ReadEnd::Silent,
            Ok(Some(Ok(m))) => m,
            Ok(_) => return ReadEnd::Closed,
        };
        match msg {
            Message::Text(t) => {
                if to.send(t.to_string()).await.is_err() {
                    return ReadEnd::Closed;
                }
            }
            Message::Close(_) => return ReadEnd::Closed,
            _ => {}
        }
    }
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
            l::method::COMMAND_SERVE => crate::commands::serve(self.node.clone(), params(p)?).await,
            l::method::PAGES_SERVE => crate::pages::serve(self.node.clone(), params(p)?).await,
            l::method::SESSION_SERVE => crate::start::serve(&self.node, params(p)?).await,
            l::method::SESSION_ACT_SERVE => crate::act::serve(&self.node, params(p)?).await,
            l::method::TRANSCRIPT_SERVE => {
                let r: l::TranscriptServe = params(p)?;
                tracing::info!(session = %r.session, caller = %r.caller, "transcript read");
                answer(&crate::transcripts::read_here(&self.node, &r.session, &r.range).await?)
            }
            l::method::TRANSCRIPT_SEARCH_SERVE => {
                let r: l::TranscriptSearchServe = params(p)?;
                tracing::info!(caller = %r.caller, "transcript search");
                answer(&crate::transcripts::search_here(&self.node, &r.query).await?)
            }
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
            l::method::EVENT_DELIVER => {
                let r: l::EventDeliverRequest = params(p)?;
                crate::plugins::deliver_event(&self.node, &r.plugin, &r.delivery).await?;
                Ok(serde_json::json!({}))
            }
            _ => Err(RpcError::method_not_found(method)),
        }
    }

    async fn notification(&self, _peer: &Peer, method: &str, p: Value) {
        if method == l::method::MOVED {
            let Ok(m) = serde_json::from_value::<l::Moved>(p) else {
                tracing::warn!("ignored a link/moved it couldn't read");
                return;
            };
            let current = self.node.link.url().await.or_else(|| self.node.control.as_ref().map(|c| c.url.clone()));
            match follows_move(current.as_deref().unwrap_or(""), &m.url, m.proof.as_deref()) {
                Ok(()) => {
                    tracing::info!(url = %m.url, "the control plane is moving; trying its new address once it shows the move's proof");
                    let proof = m.proof.unwrap_or_default();
                    *self.node.link.moved_to.lock().await =
                        Some(Moving { url: m.url, proof, since: std::time::Instant::now() });
                }
                Err(why) => tracing::warn!(url = %m.url, "ignored link/moved: {why}"),
            }
            return;
        }
        if method == l::method::READING_UPDATE {
            crate::readings::update(&self.node, &p).await;
            return;
        }
        if method == l::method::WINDOW {
            match serde_json::from_value::<l::WindowNotice>(p) {
                // Closing sessions and running hooks takes a while: not on the link's reader.
                Ok(n) => drop(tokio::spawn(crate::window::notice(self.node.clone(), n))),
                Err(e) => tracing::warn!("a window notice the node can't read: {e}"),
            }
            return;
        }
        if method == l::method::SYNC_WANTED
            && let (Some(bundle), Some(node)) = (p["bundle"].as_str(), p["node"].as_str())
        {
            crate::sync::wanted(&self.node, bundle, node).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_move_is_followed_only_with_a_proof_and_never_down_to_plain_ws() {
        let proof = "ab".repeat(24);
        assert!(follows_move("ws://127.0.0.1:7600/v1/link", "ws://10.0.0.5:7600/v1/link", Some(&proof)).is_ok());
        assert!(follows_move("ws://127.0.0.1:7600/v1/link", "wss://c.example/v1/link", Some(&proof)).is_ok());
        assert!(follows_move("wss://c.example/v1/link", "ws://10.0.0.5:7600/v1/link", Some(&proof)).is_err());
        assert!(follows_move("ws://a/v1/link", "http://b/v1/link", Some(&proof)).is_err());
        assert!(follows_move("ws://a/v1/link", "ws://b/v1/link", None).is_err());
        assert!(follows_move("ws://a/v1/link", "ws://b/v1/link", Some("short")).is_err());
        assert!(follows_move("ws://a/v1/link", "ws://b/v1/link", Some(&"z&x=".repeat(12))).is_err(), "hex only");
        assert_eq!(web_base("wss://c.example/v1/link"), "https://c.example");
        assert_eq!(web_base("ws://10.0.0.5:7600/v1/link"), "http://10.0.0.5:7600");
    }

    /// A stub control plane that sends a frame and a ping, then goes quiet without closing: the node notices the
    /// silence within its deadline, while a link that's only quiet for less than that stays up.
    #[tokio::test]
    async fn a_link_that_goes_silent_is_dropped() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            ws.send(Message::Text("{\"jsonrpc\":\"2.0\",\"method\":\"x\"}".into())).await.unwrap();
            tokio::time::sleep(Duration::from_millis(150)).await;
            ws.send(Message::Ping(vec![1].into())).await.unwrap();
            // Then nothing, and no close: the connection stays open, as a dead NAT path would look.
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(ws);
        });
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/v1/link")).await.unwrap();
        let (_sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::channel(8);
        let started = std::time::Instant::now();
        let end = read_frames(&mut stream, &tx, Duration::from_millis(400)).await;
        assert_eq!(end, ReadEnd::Silent);
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(500), "the ping kept it alive past the first deadline: {took:?}");
        assert!(took < Duration::from_secs(5), "{took:?}");
        assert_eq!(rx.recv().await.as_deref(), Some("{\"jsonrpc\":\"2.0\",\"method\":\"x\"}"));
        server.abort();
    }

    #[tokio::test]
    async fn a_link_the_control_plane_closes_ends_as_closed() {
        let (tx, _rx) = mpsc::channel(8);
        let mut stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(Message::Close(None))]);
        assert_eq!(read_frames(&mut stream, &tx, Duration::from_secs(5)).await, ReadEnd::Closed);
        let mut ended = futures::stream::iter(Vec::<Result<Message, std::io::Error>>::new());
        assert_eq!(read_frames(&mut ended, &tx, Duration::from_secs(5)).await, ReadEnd::Closed);
    }

    #[test]
    fn the_deadline_outlasts_three_missed_pings() {
        // The control plane pings every 30 s: three gone by means the link is dead, not slow.
        assert!(READ_DEADLINE > Duration::from_secs(90));
    }
}
