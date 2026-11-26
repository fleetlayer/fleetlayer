//! A JSON-RPC 2.0 peer over a pair of byte streams, one message per line, as `docs/protocol.md` section 1 describes.
//! Both sides send requests: [`Peer::request`] and [`Peer::notify`] send; a [`Handler`] answers what arrives.
//!
//! A plugin calls [`serve_stdio`]; the node connects to each plugin's child process with [`connect`].

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use fleetlayer_protocol::rpc::{Kind, Message, RpcError, code};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};

pub use fleetlayer_protocol as protocol;

/// Answers what the other side sends. Each request and notification runs in its own task, so a slow one never holds
/// up the rest.
#[async_trait]
pub trait Handler: Send + Sync + 'static {
    async fn request(&self, peer: &Peer, method: &str, params: Value) -> Result<Value, RpcError>;

    /// Notifications have no answer. The default ignores them, as the protocol says unknown ones must be.
    async fn notification(&self, _peer: &Peer, _method: &str, _params: Value) {}
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>>;

/// One end of a connection. Cheap to clone; every clone sends on the same connection.
#[derive(Clone)]
pub struct Peer {
    out: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: Arc<AtomicI64>,
}

/// How long [`Peer::request`] waits by default.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

impl Peer {
    /// Send a request and wait for its answer, up to [`DEFAULT_TIMEOUT`].
    pub async fn request<P: Serialize, R: DeserializeOwned>(&self, method: &str, params: &P) -> Result<R, RpcError> {
        self.request_within(method, params, DEFAULT_TIMEOUT).await
    }

    pub async fn request_within<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
        timeout: Duration,
    ) -> Result<R, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let params = serde_json::to_value(params).map_err(RpcError::internal)?;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        if !self.send(&Message::request(id, method, params)) {
            self.pending.lock().await.remove(&id);
            return Err(closed());
        }
        let answer = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Err(closed()),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(RpcError::new(
                    code::INTERNAL_ERROR,
                    format!("{method} didn't answer within {}s", timeout.as_secs()),
                ))
            }
        }?;
        serde_json::from_value(answer).map_err(|e| RpcError::internal(format!("reading the answer to {method}: {e}")))
    }

    /// Send a notification. False when the connection is gone.
    pub fn notify<P: Serialize>(&self, method: &str, params: &P) -> bool {
        match serde_json::to_value(params) {
            Ok(params) => self.send(&Message::notification(method, params)),
            Err(_) => false,
        }
    }

    pub fn is_closed(&self) -> bool {
        self.out.is_closed()
    }

    fn send(&self, m: &Message) -> bool {
        match serde_json::to_string(m) {
            Ok(line) => self.out.send(line).is_ok(),
            Err(_) => false,
        }
    }
}

fn closed() -> RpcError {
    RpcError::new(code::CLOSED, "the connection closed before an answer came")
}

/// Parse a request's params into `T`, as an invalid-params error when they don't fit.
pub fn params<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(RpcError::invalid_params)
}

/// Serialize an answer.
pub fn answer<T: Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(RpcError::internal)
}

/// Connect a peer over `reader` and `writer`. Returns the peer and a task that ends when the other side closes
/// `reader` (or sends something that isn't a line of UTF-8). Pending requests then fail with [`code::CLOSED`].
pub fn connect<R, W, H>(reader: R, writer: W, handler: Arc<H>) -> (Peer, tokio::task::JoinHandle<()>)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    H: Handler,
{
    let (out, mut rx) = mpsc::unbounded_channel::<String>();
    let peer = Peer { out, pending: Pending::default(), next_id: Arc::new(AtomicI64::new(1)) };

    let mut writer = writer;
    let write_task = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if writer.write_all(line.as_bytes()).await.is_err()
                || writer.write_all(b"\n").await.is_err()
                || writer.flush().await.is_err()
            {
                break;
            }
        }
    });

    let reader_peer = peer.clone();
    let task = tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            dispatch(&reader_peer, &handler, &line).await;
        }
        // The other side is gone: fail whatever is still waiting, and stop writing.
        for (_, tx) in reader_peer.pending.lock().await.drain() {
            let _ = tx.send(Err(closed()));
        }
        write_task.abort();
    });
    (peer, task)
}

async fn dispatch<H: Handler>(peer: &Peer, handler: &Arc<H>, line: &str) {
    let msg: Message = match serde_json::from_str(line) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("not a JSON-RPC message ({e}): {}", truncate(line));
            peer.send(&Message::response(Value::Null, Err(RpcError::new(code::PARSE_ERROR, e.to_string()))));
            return;
        }
    };
    match msg.kind() {
        Kind::Request { id, method } => {
            let (id, method) = (id.clone(), method.to_string());
            let params = msg.params.unwrap_or(Value::Null);
            let (peer, handler) = (peer.clone(), handler.clone());
            tokio::spawn(async move {
                let result = handler.request(&peer, &method, params).await;
                peer.send(&Message::response(id, result));
            });
        }
        Kind::Notification { method } => {
            let method = method.to_string();
            let params = msg.params.unwrap_or(Value::Null);
            let (peer, handler) = (peer.clone(), handler.clone());
            tokio::spawn(async move { handler.notification(&peer, &method, params).await });
        }
        // A null result reads as absent, so an id with neither result nor error is still an answer (`result: null`).
        Kind::Response { .. } | Kind::Invalid if msg.method.is_none() && msg.id.is_some() => {
            let Some(id) = msg.id.as_ref().and_then(Value::as_i64) else { return };
            if let Some(tx) = peer.pending.lock().await.remove(&id) {
                let _ = tx.send(match msg.error {
                    Some(e) => Err(e),
                    None => Ok(msg.result.unwrap_or(Value::Null)),
                });
            }
        }
        _ => tracing::warn!("an invalid JSON-RPC message: {}", truncate(line)),
    }
}

fn truncate(s: &str) -> &str {
    match s.char_indices().nth(200) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Connect a peer over message frames rather than a byte stream (a WebSocket's text frames): each frame is one
/// JSON-RPC message. `incoming` yields the frames that arrive; frames to send go to `outgoing`. The peer's task ends
/// when `incoming` closes.
pub fn connect_frames<H: Handler>(
    mut incoming: mpsc::Receiver<String>,
    outgoing: mpsc::Sender<String>,
    handler: Arc<H>,
) -> (Peer, tokio::task::JoinHandle<()>) {
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    let (their_read, mut their_write) = tokio::io::split(theirs);
    // Frames in: one line each. JSON never needs a raw newline (inside strings it's escaped), so any is whitespace.
    let inbound = tokio::spawn(async move {
        while let Some(frame) = incoming.recv().await {
            let line = frame.replace(['\n', '\r'], " ");
            if their_write.write_all(line.as_bytes()).await.is_err() || their_write.write_all(b"\n").await.is_err() {
                break;
            }
        }
        let _ = their_write.shutdown().await;
    });
    // Lines out: one frame each.
    tokio::spawn(async move {
        let mut lines = BufReader::new(their_read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if outgoing.send(line).await.is_err() {
                break;
            }
        }
        inbound.abort();
    });
    let (r, w) = tokio::io::split(ours);
    connect(r, w, handler)
}

/// Serve a plugin on stdin and stdout until the node closes stdin. Log to stderr, never stdout.
pub async fn serve_stdio<H: Handler>(handler: Arc<H>) {
    let (_peer, task) = connect(tokio::io::stdin(), tokio::io::stdout(), handler);
    let _ = task.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Echo;

    #[async_trait]
    impl Handler for Echo {
        async fn request(&self, peer: &Peer, method: &str, params: Value) -> Result<Value, RpcError> {
            match method {
                "echo" => Ok(params),
                "null" => Ok(Value::Null),
                // Asks the other side something before answering: requests go both ways at once.
                "ask_back" => peer.request::<_, Value>("echo", &json!({"back": true})).await,
                "slow" => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    Ok(json!("slow"))
                }
                _ => Err(RpcError::method_not_found(method)),
            }
        }
    }

    fn pair() -> (Peer, Peer) {
        let (a, b) = tokio::io::duplex(1 << 16);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let (pa, _) = connect(ar, aw, Arc::new(Echo));
        let (pb, _) = connect(br, bw, Arc::new(Echo));
        (pa, pb)
    }

    #[tokio::test]
    async fn requests_go_both_ways_and_errors_come_back() {
        let (a, _b) = pair();
        let v: Value = a.request("echo", &json!({"x": 1})).await.unwrap();
        assert_eq!(v, json!({"x": 1}));
        let v: Value = a.request("ask_back", &json!({})).await.unwrap();
        assert_eq!(v, json!({"back": true}));
        let v: Value = a.request("null", &json!({})).await.unwrap();
        assert_eq!(v, Value::Null);
        let e = a.request::<_, Value>("nope", &json!({})).await.unwrap_err();
        assert_eq!(e.code, code::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn a_slow_request_doesnt_hold_up_a_fast_one() {
        let (a, _b) = pair();
        let (empty, one) = (json!({}), json!(1));
        let slow = a.request::<_, Value>("slow", &empty);
        let fast = a.request::<_, Value>("echo", &one);
        let (slow, fast) = tokio::join!(slow, fast);
        assert_eq!((slow.unwrap(), fast.unwrap()), (json!("slow"), json!(1)));
    }

    #[tokio::test]
    async fn a_closed_connection_fails_pending_requests() {
        let (a, b) = pair();
        let waiting = tokio::spawn({
            let a = a.clone();
            async move { a.request::<_, Value>("slow", &json!({})).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(b);
        // b's tasks still hold its streams until they notice; closing ends both directions eventually.
        let r = tokio::time::timeout(Duration::from_secs(2), waiting).await.unwrap().unwrap();
        // Either the slow answer arrived before b's side closed, or the request failed as closed.
        if let Err(e) = r {
            assert_eq!(e.code, code::CLOSED);
        }
    }

    #[tokio::test]
    async fn peers_talk_over_frames() {
        let (a_out, b_in) = mpsc::channel(16);
        let (b_out, a_in) = mpsc::channel(16);
        let (a, _) = connect_frames(a_in, a_out, Arc::new(Echo));
        let (_b, _) = connect_frames(b_in, b_out, Arc::new(Echo));
        let v: Value = a.request("echo", &json!({"text": "two\nlines"})).await.unwrap();
        assert_eq!(v, json!({"text": "two\nlines"}));
        let v: Value = a.request("ask_back", &json!({})).await.unwrap();
        assert_eq!(v, json!({"back": true}));
    }

    #[tokio::test]
    async fn a_timeout_is_reported() {
        let (a, _b) = pair();
        let e = a.request_within::<_, Value>("slow", &json!({}), Duration::from_millis(20)).await.unwrap_err();
        assert!(e.message.contains("didn't answer"), "{e}");
    }
}
