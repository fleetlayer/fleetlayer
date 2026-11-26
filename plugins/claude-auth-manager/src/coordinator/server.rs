//! The coordinator API on its Unix socket (`<rotator_dir>/coordinator.sock`), HTTP/1.1 with JSON bodies, as the
//! rotation plugin and the summaries module expect it (`docs/coordinator-api.md` in the rotator). Also the lock
//! file that keeps one monitor per user, shared with the TypeScript coordinator, and a small client for following a
//! coordinator that still holds it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{Method, StatusCode, Uri, header};
use axum::response::Response as HttpResponse;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use super::core::{Coordinator, Deps, Request};

pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join("coordinator.sock")
}

pub fn lock_path(dir: &Path) -> PathBuf {
    dir.join("coordinator.lock")
}

pub fn state_path(dir: &Path) -> PathBuf {
    dir.join("state.json")
}

pub fn log_path(dir: &Path) -> PathBuf {
    dir.join("coordinator.log")
}

fn is_running(pid: i32) -> bool {
    pid > 0
        && match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
            Ok(()) => true,
            Err(e) => e == nix::errno::Errno::EPERM,
        }
}

/// Takes the lock file, as the TypeScript coordinator does: Ok(None) once taken, Ok(Some(pid)) while another live
/// process holds it. A lock left by a process that died is taken over.
pub fn take_lock(file: &Path) -> std::io::Result<Option<i32>> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let me = std::process::id() as i32;
    for _ in 0..2 {
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(file) {
            Ok(mut f) => {
                f.write_all(me.to_string().as_bytes())?;
                return Ok(None);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = std::fs::read_to_string(file).ok().and_then(|t| t.trim().parse::<i32>().ok()).unwrap_or(0);
                if holder > 0 && holder != me && is_running(holder) {
                    return Ok(Some(holder));
                }
                if holder == me {
                    return Ok(None);
                }
                let _ = std::fs::remove_file(file);
            }
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other(format!("could not take {}", file.display())))
}

/// Opens the API on `dir/coordinator.sock` (the directory mode 0700, the socket 0600).
pub fn bind(dir: &Path) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let sock = socket_path(dir);
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Ends the long poll's registration when the request ends, answered or dropped.
struct PollGuard<D: Deps> {
    coordinator: Coordinator<D>,
    session: String,
}

impl<D: Deps> Drop for PollGuard<D> {
    fn drop(&mut self) {
        self.coordinator.poll_closed(&self.session);
    }
}

fn json_response(status: u16, body: &Value) -> HttpResponse {
    let text = body.to_string();
    let mut res = HttpResponse::new(Body::from(text));
    *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    res.headers_mut().insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/json"));
    res
}

async fn answer<D: Deps>(
    State(coordinator): State<Coordinator<D>>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> HttpResponse {
    let query: HashMap<String, String> =
        uri.query().map(|q| form_urlencoded::parse(q.as_bytes()).into_owned().collect()).unwrap_or_default();
    let body = if body.is_empty() {
        None
    } else {
        match serde_json::from_slice::<Value>(&body) {
            Ok(v) => Some(v),
            Err(_) => return json_response(400, &json!({ "error": "The body is not JSON." })),
        }
    };
    let is_poll = method == Method::GET && uri.path() == "/state" && query.contains_key("since");
    let _guard = match query.get("session") {
        Some(session) if is_poll => {
            coordinator.poll_opened(session);
            Some(PollGuard { coordinator: coordinator.clone(), session: session.clone() })
        }
        _ => None,
    };
    let res =
        coordinator.handle(Request { method: method.to_string(), path: uri.path().to_string(), query, body }).await;
    // The node runs the API for as long as it runs: a coordinator's `cli.ts stop` or upgrade can't end it.
    if res.shutdown {
        return json_response(
            409,
            &json!({ "error": "Quietdeck's node serves this API, and keeps it running: it doesn't stop on request." }),
        );
    }
    json_response(res.status, &res.body)
}

/// Serves the API until `stopped` resolves.
pub async fn serve<D: Deps>(
    listener: UnixListener,
    coordinator: Coordinator<D>,
    stopped: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let router =
        Router::new().fallback(answer::<D>).layer(DefaultBodyLimit::max(1024 * 1024)).with_state(coordinator.clone());
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    let shutdown = async move {
        stopped.await;
        // Wake every long poll, so the server isn't held open for 55 s.
        coordinator.stop();
        let _ = done_tx.send(());
    };
    let server = axum::serve(listener, router).with_graceful_shutdown(shutdown);
    let serving = tokio::spawn(async move { server.await });
    let _ = done_rx.await;
    // Connections still open after two seconds are cut, as the TypeScript coordinator does.
    match tokio::time::timeout(Duration::from_secs(2), serving).await {
        Ok(Ok(result)) => result,
        _ => Ok(()),
    }
}

/// One request to a coordinator on its socket: the status and JSON body. Connection: close, so the body ends with
/// the connection.
pub async fn call(
    sock: &Path,
    method: &str,
    path: &str,
    body: Option<&Value>,
    timeout: Duration,
) -> std::io::Result<(u16, Value)> {
    let work = async {
        let mut stream = UnixStream::connect(sock).await?;
        let body = body.map(Value::to_string).unwrap_or_default();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: coordinator\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).await?;
        let split =
            raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| std::io::Error::other("no HTTP response"))?;
        let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
        let rest = &raw[split + 4..];
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| std::io::Error::other("no HTTP status"))?;
        let is_chunked = head.lines().any(|l| l.starts_with("transfer-encoding:") && l.contains("chunked"));
        let payload = if is_chunked { dechunk(rest) } else { rest.to_vec() };
        let value = serde_json::from_slice(&payload).unwrap_or(Value::Null);
        Ok((status, value))
    };
    tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out"))?
}

fn dechunk(mut s: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(eol) = s.windows(2).position(|w| w == b"\r\n") {
        let size = String::from_utf8_lossy(&s[..eol]);
        let Ok(n) = usize::from_str_radix(size.trim(), 16) else {
            break;
        };
        let rest = &s[eol + 2..];
        if n == 0 || rest.len() < n {
            break;
        }
        out.extend_from_slice(&rest[..n]);
        s = rest[n..].strip_prefix(b"\r\n").unwrap_or(&rest[n..]);
    }
    out
}
