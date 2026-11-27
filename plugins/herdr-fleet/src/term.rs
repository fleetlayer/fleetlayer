//! Reading and typing into another machine's sessions, through the node's terminal streams (`docs/protocol.md`,
//! "Terminals"): the same stream `fleetlayer attach` opens, checked by the access policy (`read` to watch, `attach` to
//! type). A stream stays open while it's used and closes after a quiet minute and a half, so a client that reads a
//! screen every few seconds doesn't open a stream each time.
//!
//! Screens are read with the stream's `terminal.scrollback` command; typing uses `terminal.keys` and `terminal.paste`
//! in `input` mode (herdr's `observe --input`), which never resizes the pane or takes it from the person's own window.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};

/// How long a stream may sit unused before it's closed.
const IDLE: Duration = Duration::from_secs(90);
/// How long a scrollback read may take.
const READ_TIMEOUT: Duration = Duration::from_secs(15);
/// Keys and pastes have no reply when they work, only `terminal.error` when they don't: wait this long for one.
const ERROR_WAIT: Duration = Duration::from_millis(400);
/// The size pooled streams are opened at: the smallest herdr renders. Only frames follow it, and they're thrown away
/// here; scrollback comes back at the pane's own width whatever the observer's size (herdr 0.9.5), so a tiny observer
/// costs about a seventeenth of a 120 by 40 one in frames for a busy agent.
const POOL_COLS: u16 = 20;
const POOL_ROWS: u16 = 5;
/// How many lines a `visible` read returns when the client doesn't say: the pane's height isn't known here.
pub const VISIBLE_LINES: u32 = 50;
/// What the access policy's audit log says these streams are for.
const WHY: &str = "a herdr client (such as a phone app) reading or typing into this session through herdr-fleet";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    Observe,
    Input,
}

impl Mode {
    fn wire(self) -> &'static str {
        match self {
            Mode::Observe => "observe",
            Mode::Input => "input",
        }
    }
}

/// A failure, with the herdr error code a client expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub code: String,
    pub message: String,
}

impl Failure {
    pub fn new(code: &str, message: impl Into<String>) -> Failure {
        Failure { code: code.into(), message: message.into() }
    }
}

struct Stream {
    tx: mpsc::Sender<String>,
    pending: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    alive: AtomicBool,
    last_used: Mutex<Instant>,
}

/// Open streams, by session address and mode.
pub struct Pool {
    tools_socket: PathBuf,
    streams: Mutex<HashMap<(String, Mode), Arc<Stream>>>,
    next_id: AtomicU64,
}

/// Open a terminal on the node's tools socket: answers the stream's lines after the node's `{"ok":true}`.
pub async fn open_raw(
    tools_socket: &std::path::Path,
    address: &str,
    mode: &str,
    cols: u16,
    rows: u16,
) -> Result<(tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>, tokio::net::unix::OwnedWriteHalf), Failure> {
    let stream = tokio::net::UnixStream::connect(tools_socket).await.map_err(|e| {
        Failure::new("server_unavailable", format!("can't reach the Fleetlayer node ({}): {e}", tools_socket.display()))
    })?;
    let (r, mut w) = stream.into_split();
    let req = json!({"terminal": address, "mode": mode, "cols": cols, "rows": rows, "why": WHY});
    w.write_all(format!("{req}\n").as_bytes())
        .await
        .map_err(|e| Failure::new("server_unavailable", format!("the Fleetlayer node hung up: {e}")))?;
    let mut lines = BufReader::new(r).lines();
    let first = lines.next_line().await.ok().flatten().unwrap_or_default();
    let v: Value = serde_json::from_str(&first).unwrap_or(Value::Null);
    if v["ok"].as_bool() == Some(true) {
        return Ok((lines, w));
    }
    let message = v["error"].as_str().unwrap_or("the Fleetlayer node didn't open the terminal").to_string();
    Err(Failure::new(code_for(&message), message))
}

/// The herdr code for a Fleetlayer refusal: one waiting for the person's approval, or refused, keeps its sentence.
fn code_for(message: &str) -> &'static str {
    let m = message.to_lowercase();
    if m.contains("isn't running") || m.contains("no session") || m.contains("not found") {
        "agent_not_found"
    } else if m.contains("offline") || m.contains("isn't linked") {
        "server_unavailable"
    } else {
        "command_failed"
    }
}

impl Pool {
    pub fn new(tools_socket: PathBuf) -> Arc<Pool> {
        let pool = Arc::new(Pool { tools_socket, streams: Mutex::new(HashMap::new()), next_id: AtomicU64::new(1) });
        let p = pool.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                p.reap().await;
            }
        });
        pool
    }

    pub async fn open_count(&self) -> usize {
        self.streams.lock().await.len()
    }

    async fn reap(&self) {
        let mut streams = self.streams.lock().await;
        let mut gone = vec![];
        for (k, s) in streams.iter() {
            if !s.alive.load(Ordering::Relaxed) || s.last_used.lock().await.elapsed() > IDLE {
                gone.push(k.clone());
            }
        }
        for k in gone {
            // Dropping the sender ends the writer, which closes the stream.
            streams.remove(&k);
        }
    }

    async fn stream(&self, address: &str, mode: Mode) -> Result<Arc<Stream>, Failure> {
        let key = (address.to_string(), mode);
        {
            let streams = self.streams.lock().await;
            if let Some(s) = streams.get(&key)
                && s.alive.load(Ordering::Relaxed)
            {
                *s.last_used.lock().await = Instant::now();
                return Ok(s.clone());
            }
        }
        // Opened without holding the pool: an open that waits for the person's approval mustn't hold up other reads.
        let (mut lines, mut w) = open_raw(&self.tools_socket, address, mode.wire(), POOL_COLS, POOL_ROWS).await?;
        let (tx, mut rx) = mpsc::channel::<String>(64);
        let s = Arc::new(Stream {
            tx,
            pending: Mutex::new(HashMap::new()),
            alive: AtomicBool::new(true),
            last_used: Mutex::new(Instant::now()),
        });
        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if w.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
            // The pool let it go: end the stream politely.
            let _ = w.write_all(b"{\"type\":\"terminal.release\"}\n").await;
        });
        let reader = s.clone();
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                match v["type"].as_str() {
                    Some("terminal.scrollback") | Some("terminal.error") => {
                        if let Some(id) = v["id"].as_str()
                            && let Some(tx) = reader.pending.lock().await.remove(id)
                        {
                            let _ = tx.send(v);
                        }
                    }
                    Some("terminal.closed") => break,
                    _ => {}
                }
            }
            reader.alive.store(false, Ordering::Relaxed);
            reader.pending.lock().await.clear();
        });
        let mut streams = self.streams.lock().await;
        if let Some(other) = streams.get(&key)
            && other.alive.load(Ordering::Relaxed)
        {
            // Another request opened one meanwhile: use it, and let this one close.
            return Ok(other.clone());
        }
        streams.insert(key, s.clone());
        Ok(s)
    }

    /// Send a command and wait for its reply (or, with `quiet_ok`, take silence for success).
    async fn command(&self, address: &str, mode: Mode, mut cmd: Value, quiet_ok: bool) -> Result<Value, Failure> {
        let s = self.stream(address, mode).await?;
        let id = format!("hf{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        cmd["id"] = json!(id);
        let (tx, rx) = oneshot::channel();
        s.pending.lock().await.insert(id.clone(), tx);
        if s.tx.send(format!("{cmd}\n")).await.is_err() {
            s.alive.store(false, Ordering::Relaxed);
            return Err(Failure::new("server_unavailable", format!("the terminal stream to {address} closed")));
        }
        let wait = if quiet_ok { ERROR_WAIT } else { READ_TIMEOUT };
        match tokio::time::timeout(wait, rx).await {
            Ok(Ok(v)) if v["type"] == "terminal.error" => Err(Failure::new(
                v["code"].as_str().unwrap_or("command_failed"),
                v["message"].as_str().unwrap_or("the terminal refused it").to_string(),
            )),
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => Err(Failure::new("server_unavailable", format!("the terminal stream to {address} closed"))),
            Err(_) if quiet_ok => {
                s.pending.lock().await.remove(&id);
                Ok(Value::Null)
            }
            Err(_) => {
                s.pending.lock().await.remove(&id);
                Err(Failure::new("timeout", format!("{address} didn't answer within {} seconds", wait.as_secs())))
            }
        }
    }

    /// The last `lines` rendered rows of the session's terminal, scrollback included: `(text, truncated)`.
    pub async fn read(&self, address: &str, lines: u32, ansi: bool) -> Result<(String, bool), Failure> {
        let cmd = json!({"type": "terminal.scrollback", "lines": lines.clamp(1, 10_000),
            "format": if ansi { "ansi" } else { "text" }});
        let v = self.command(address, Mode::Observe, cmd, false).await?;
        Ok((v["text"].as_str().unwrap_or_default().to_string(), v["truncated"].as_bool().unwrap_or(false)))
    }

    /// Press keys, with herdr's key names (`enter`, `esc`, `ctrl+c`, a single character).
    pub async fn keys(&self, address: &str, keys: &[String]) -> Result<(), Failure> {
        if keys.is_empty() {
            return Ok(());
        }
        self.command(address, Mode::Input, json!({"type": "terminal.keys", "keys": keys}), true).await.map(|_| ())
    }

    /// Paste text (typed text when the application hasn't turned paste mode on). No Enter.
    pub async fn paste(&self, address: &str, text: &str) -> Result<(), Failure> {
        self.command(address, Mode::Input, json!({"type": "terminal.paste", "text": text}), true).await.map(|_| ())
    }

    /// herdr's `agent prompt`: type the text, wait 300 ms, press Enter.
    pub async fn prompt(&self, address: &str, text: &str) -> Result<(), Failure> {
        self.paste(address, text).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        self.keys(address, &["enter".to_string()]).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in node: answers the open, replies to scrollback with what it was asked, records keys and pastes,
    /// and refuses keys named "bad".
    async fn fake_node(dir: &std::path::Path) -> (PathBuf, Arc<Mutex<Vec<String>>>) {
        let path = dir.join("node.tools.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let seen = Arc::new(Mutex::new(vec![]));
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let (s, _) = listener.accept().await.unwrap();
                let log = log.clone();
                tokio::spawn(async move {
                    let (r, mut w) = s.into_split();
                    let mut lines = BufReader::new(r).lines();
                    let open: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                    log.lock().await.push(format!("open {} {}", open["terminal"], open["mode"]));
                    if open["terminal"] == "nope@laptop" {
                        w.write_all(b"{\"error\":\"nope on laptop isn't running\"}\n").await.unwrap();
                        return;
                    }
                    w.write_all(b"{\"ok\":true}\n").await.unwrap();
                    w.write_all(b"{\"type\":\"terminal.opened\",\"mode\":\"observe\"}\n").await.unwrap();
                    while let Ok(Some(l)) = lines.next_line().await {
                        let v: Value = serde_json::from_str(&l).unwrap();
                        log.lock().await.push(format!(
                            "{} {}",
                            v["type"],
                            v.get("keys").or(v.get("text")).unwrap_or(&Value::Null)
                        ));
                        let reply = match v["type"].as_str() {
                            Some("terminal.scrollback") => Some(json!({"type": "terminal.scrollback", "id": v["id"],
                                "text": format!("{} lines", v["lines"]), "format": v["format"], "truncated": false})),
                            Some("terminal.keys") if v["keys"][0] == "bad" => Some(json!({"type": "terminal.error",
                                "id": v["id"], "code": "invalid_key", "message": "no key called bad"})),
                            _ => None,
                        };
                        if let Some(r) = reply {
                            w.write_all(format!("{r}\n").as_bytes()).await.unwrap();
                        }
                    }
                });
            }
        });
        (path, seen)
    }

    #[tokio::test]
    async fn reads_and_typing_reuse_one_stream_per_mode() {
        let dir = tempfile::tempdir().unwrap();
        let (sock, seen) = fake_node(dir.path()).await;
        let pool = Pool::new(sock);
        assert_eq!(pool.read("billing@laptop", 40, false).await.unwrap(), ("40 lines".into(), false));
        assert_eq!(pool.read("billing@laptop", 3, true).await.unwrap().0, "3 lines");
        pool.prompt("billing@laptop", "run the tests").await.unwrap();
        let e = pool.keys("billing@laptop", &["bad".into()]).await.unwrap_err();
        assert_eq!(e.code, "invalid_key");
        let e = pool.read("nope@laptop", 1, false).await.unwrap_err();
        assert_eq!(e.code, "agent_not_found");
        let seen = seen.lock().await.clone();
        assert_eq!(
            seen,
            [
                "open \"billing@laptop\" \"observe\"",
                "\"terminal.scrollback\" null",
                "\"terminal.scrollback\" null",
                "open \"billing@laptop\" \"input\"",
                "\"terminal.paste\" \"run the tests\"",
                "\"terminal.keys\" [\"enter\"]",
                "\"terminal.keys\" [\"bad\"]",
                "open \"nope@laptop\" \"observe\"",
            ]
        );
        assert_eq!(pool.open_count().await, 2);
    }
}
