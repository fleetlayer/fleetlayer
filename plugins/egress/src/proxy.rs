//! The proxy. Three TCP listeners on loopback:
//! - the proxy port, for clients that use `HTTPS_PROXY`/`HTTP_PROXY` (CONNECT, or a plain HTTP request in absolute
//!   form), which also answers the local API agents use (`/status`, `/why`, `/ask`);
//! - the TLS port, where the firewall redirects a restricted user's port-443 connections: the host comes from the
//!   ClientHello's server name, read and passed on untouched (never decrypted);
//! - the HTTP port, for redirected port-80 connections: the host comes from the `Host` header.
//!
//! Every connection is decided by its Unix user's role, written to the audit log with its bytes and time, and connected
//! to the host by name (resolved here), so a client can't name an allowed host and reach another address.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::audit::Line;
use crate::engine::{Decision, Engine};
use crate::hosts::{Target, parse_authority};
use crate::sni::{self, Sni};

/// The host name the local API answers on through the proxy (`http://egress.fleetlayer/status`).
pub const API_HOST: &str = "egress.fleetlayer";
const HEAD_MAX: usize = 16 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Who a local connection from `peer` comes from: (user name, the address it first asked for). The second argument
/// is the address the client must be connected to, for a connection made to a listener directly (not redirected).
/// None when it can't be told, which is refused.
pub type Who = Arc<dyn Fn(SocketAddr, Option<SocketAddr>) -> Option<(String, SocketAddr)> + Send + Sync>;

/// The real lookup: /proc/net and /etc/passwd.
pub fn proc_who(engine: Arc<Engine>) -> Who {
    Arc::new(move |peer, to| {
        let owner = crate::procnet::tcp(peer, to)?;
        // A uid nobody owns (no user, no user's subordinate range) is nobody: refused.
        let name = engine.users.lock().ok()?.name(owner.uid)?;
        Some((name, owner.remote))
    })
}

#[derive(Clone)]
pub struct Server {
    pub engine: Arc<Engine>,
    pub who: Who,
    /// The listeners' own addresses, never dialled (a loop through the proxy).
    pub own: Arc<Vec<SocketAddr>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Proxy,
    Tls,
    Http,
}

impl Server {
    pub async fn serve(self, listener: TcpListener, kind: Kind) {
        loop {
            let Ok((stream, peer)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            };
            let s = self.clone();
            tokio::spawn(async move {
                let _ = stream.set_nodelay(true);
                let r = match kind {
                    Kind::Proxy => s.proxy_conn(stream, peer).await,
                    Kind::Tls => s.tls_conn(stream, peer).await,
                    Kind::Http => s.http_conn(stream, peer).await,
                };
                if let Err(e) = r {
                    tracing::debug!("{kind:?} connection from {peer}: {e}");
                }
            });
        }
    }

    fn who(&self, peer: SocketAddr, to: Option<SocketAddr>) -> (Option<String>, Option<SocketAddr>) {
        match (self.who)(peer, to) {
            Some((u, r)) => (Some(u), Some(r)),
            None => (None, None),
        }
    }

    async fn proxy_conn(&self, mut client: TcpStream, peer: SocketAddr) -> anyhow::Result<()> {
        // Clients connect to this port directly: their socket must point at it.
        let (user, _) = self.who(peer, client.local_addr().ok());
        let (buf, head_len) = read_head(&mut client).await?;
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut headers);
        req.parse(&buf[..head_len])?;
        let method = req.method.unwrap_or("").to_string();
        let path = req.path.unwrap_or("").to_string();
        if method == "CONNECT" {
            let Some(t) = parse_authority(&path, 443) else {
                return respond(&mut client, 400, "text/plain", "CONNECT needs host:port\n").await;
            };
            let d = self.engine.decide(user.as_deref(), &t).await;
            if !d.go {
                self.log(&user, &d, "connect", &t, 0, 0, 0).await;
                return respond(&mut client, 403, "text/plain", &refusal_text(&t, &d)).await;
            }
            return self.tunnel(client, user, d, "connect", t, &buf[head_len..], true, None).await;
        }
        // A request to the proxy itself (origin form, or to the API's host name): the local API.
        let api_path = if path.starts_with('/') {
            Some(path.clone())
        } else {
            path.strip_prefix(&format!("http://{API_HOST}")).map(|p| if p.is_empty() { "/".into() } else { p.into() })
        };
        if let Some(p) = api_path {
            let body_len: usize = req
                .headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case("content-length"))
                .and_then(|h| std::str::from_utf8(h.value).ok()?.trim().parse().ok())
                .unwrap_or(0);
            let mut body = buf[head_len..].to_vec();
            if body_len > 64 * 1024 {
                return respond(&mut client, 413, "text/plain", "too large\n").await;
            }
            while body.len() < body_len {
                let mut chunk = [0u8; 4096];
                let n = tokio::time::timeout(READ_TIMEOUT, client.read(&mut chunk)).await??;
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..n]);
            }
            return self.api(&mut client, user, &method, &p, &body).await;
        }
        let Some(rest) = path.strip_prefix("http://") else {
            return respond(&mut client, 400, "text/plain", "a proxy request needs CONNECT or an http:// URL\n").await;
        };
        let authority = rest.split(['/', '?']).next().unwrap_or("");
        let Some(t) = parse_authority(authority, 80) else {
            return respond(&mut client, 400, "text/plain", "bad host\n").await;
        };
        let d = self.engine.decide(user.as_deref(), &t).await;
        if !d.go {
            self.log(&user, &d, "http", &t, 0, 0, 0).await;
            return respond(&mut client, 403, "text/plain", &refusal_text(&t, &d)).await;
        }
        self.one_request(client, user, d, t, &buf, head_len).await
    }

    async fn tls_conn(&self, mut client: TcpStream, peer: SocketAddr) -> anyhow::Result<()> {
        let (user, remote) = self.who(peer, None);
        let mut buf = Vec::with_capacity(4096);
        let sni = loop {
            match sni::parse(&buf) {
                Sni::NeedMore if buf.len() < sni::MAX_HELLO => {}
                Sni::NeedMore => break Sni::NotTls,
                other => break other,
            }
            let mut chunk = [0u8; 4096];
            let n = tokio::time::timeout(READ_TIMEOUT, client.read(&mut chunk)).await??;
            if n == 0 {
                anyhow::bail!("closed before a ClientHello");
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let port = remote.map(|r| r.port()).unwrap_or(443);
        let t = match sni {
            Sni::Name(n) => parse_authority(&n, port).map(|t| Target { port, ..t }),
            // No name: all there is to go on is the address the client dialled.
            Sni::NoName => remote.map(|r| Target { host: crate::hosts::unmap(r.ip()).to_string(), port }),
            _ => None,
        };
        let Some(t) = t else {
            let host = remote.map(|r| r.ip().to_string()).unwrap_or_default();
            let d = self.engine.decide(user.as_deref(), &Target { host: host.clone(), port }).await;
            if d.restricted {
                let line = Line {
                    at: Timestamp::now(),
                    user: user.clone().unwrap_or_else(|| "unknown".into()),
                    role: d.role.clone(),
                    via: "tls".into(),
                    host,
                    port: Some(port),
                    decision: "denied".into(),
                    reason: "not a TLS connection with a server name".into(),
                    up: 0,
                    down: 0,
                    ms: 0,
                };
                self.engine.record(line).await;
                return Ok(());
            }
            // A user outside every role isn't redirected here; if one is, pass it to the address it dialled.
            let Some(r) = remote else { return Ok(()) };
            let t = Target { host: r.ip().to_string(), port };
            return self.tunnel(client, user, d, "tls", t, &buf, false, None).await;
        };
        let d = self.engine.decide(user.as_deref(), &t).await;
        if !d.go {
            self.log(&user, &d, "tls", &t, 0, 0, 0).await;
            let _ = client.write_all(&sni::ACCESS_DENIED_ALERT).await;
            return Ok(());
        }
        self.tunnel(client, user, d, "tls", t, &buf, false, None).await
    }

    async fn http_conn(&self, mut client: TcpStream, peer: SocketAddr) -> anyhow::Result<()> {
        let (user, remote) = self.who(peer, None);
        let (buf, head_len) = read_head(&mut client).await?;
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut headers);
        req.parse(&buf[..head_len])?;
        let port = remote.map(|r| r.port()).unwrap_or(80);
        let host = req
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case("host"))
            .and_then(|h| std::str::from_utf8(h.value).ok())
            .map(str::to_string);
        let t = match host {
            Some(h) => parse_authority(&h, port).map(|t| Target { port, ..t }),
            None => remote.map(|r| Target { host: crate::hosts::unmap(r.ip()).to_string(), port }),
        };
        let Some(t) = t else {
            return respond(&mut client, 400, "text/plain", "no Host header\n").await;
        };
        let d = self.engine.decide(user.as_deref(), &t).await;
        if !d.go {
            self.log(&user, &d, "http", &t, 0, 0, 0).await;
            return respond(&mut client, 403, "text/plain", &refusal_text(&t, &d)).await;
        }
        self.one_request(client, user, d, t, &buf, head_len).await
    }

    /// Pass one plain HTTP request to its host, and only that one: a later request on the same connection could name
    /// another host and still reach this one's address (a shared CDN). The request goes up with `Connection: close`
    /// and its body bounded by `Content-Length`; a chunked body can't be bounded without parsing it, so it's refused.
    async fn one_request(
        &self,
        mut client: TcpStream,
        user: Option<String>,
        d: Decision,
        t: Target,
        buf: &[u8],
        head_len: usize,
    ) -> anyhow::Result<()> {
        let (head, body_len) = match one_request_head(&buf[..head_len]) {
            Ok(x) => x,
            Err((code, why)) => return respond(&mut client, code, "text/plain", why).await,
        };
        let rest = &buf[head_len..];
        let now = rest.len().min(body_len as usize);
        let mut first = head;
        first.extend_from_slice(&rest[..now]);
        self.tunnel(client, user, d, "http", t, &first, false, Some(body_len - now as u64)).await
    }

    /// Connect to the host by name, send what the client already sent, and copy both ways until either side closes.
    #[allow(clippy::too_many_arguments)]
    async fn tunnel(
        &self,
        mut client: TcpStream,
        user: Option<String>,
        mut d: Decision,
        via: &str,
        t: Target,
        first: &[u8],
        connect_reply: bool,
        client_limit: Option<u64>,
    ) -> anyhow::Result<()> {
        let started = Instant::now();
        let upstream = match self.dial(&t, &d).await {
            Ok(s) => s,
            Err(e) => {
                let blocked = e.starts_with("refused:");
                if blocked {
                    d.word = if d.mode == crate::settings::Mode::Log { "would_deny" } else { "denied" };
                    d.reason = e.trim_start_matches("refused: ").to_string();
                }
                self.log(&user, &d, via, &t, 0, 0, 0).await;
                match (via, blocked) {
                    ("tls", true) => {
                        let _ = client.write_all(&sni::ACCESS_DENIED_ALERT).await;
                    }
                    ("tls", false) => {}
                    (_, true) => respond(&mut client, 403, "text/plain", &refusal_text(&t, &d)).await?,
                    (_, false) => respond(&mut client, 502, "text/plain", &format!("{t}: {e}\n")).await?,
                }
                return Ok(());
            }
        };
        let mut upstream = upstream;
        let _ = upstream.set_nodelay(true);
        if connect_reply {
            client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await?;
        }
        if !first.is_empty() {
            upstream.write_all(first).await?;
        }
        let (up, down) = match client_limit {
            None => tokio::io::copy_bidirectional(&mut client, &mut upstream).await.unwrap_or((0, 0)),
            // Plain HTTP: the rest of this one request's body goes up, and nothing after it; the answer comes back
            // until the server closes (it was asked to, with `Connection: close`).
            Some(n) => {
                let (mut cr, mut cw) = client.split();
                let (mut ur, mut uw) = upstream.split();
                let upward = async { tokio::io::copy(&mut (&mut cr).take(n), &mut uw).await.unwrap_or(0) };
                let downward = async {
                    let got = tokio::io::copy(&mut ur, &mut cw).await.unwrap_or(0);
                    let _ = cw.shutdown().await;
                    got
                };
                tokio::join!(upward, downward)
            }
        };
        let ms = started.elapsed().as_millis() as u64;
        self.log(&user, &d, via, &t, up + first.len() as u64, down, ms).await;
        Ok(())
    }

    /// Resolve and connect, keeping a restricted role off private addresses it wasn't given. An Err starting
    /// "refused:" is a decision; anything else is the network.
    async fn dial(&self, t: &Target, d: &Decision) -> Result<TcpStream, String> {
        let addrs: Vec<SocketAddr> = match t.ip() {
            Some(ip) => vec![SocketAddr::new(ip, t.port)],
            None => tokio::net::lookup_host((t.host.as_str(), t.port))
                .await
                .map_err(|e| format!("can't resolve {}: {e}", t.host))?
                .collect(),
        };
        let own = |a: &SocketAddr| {
            self.own
                .iter()
                .any(|o| o.port() == a.port() && (crate::hosts::unmap(a.ip()).is_loopback() || o.ip() == a.ip()))
        };
        let usable: Vec<SocketAddr> = addrs
            .iter()
            .copied()
            .filter(|a| !own(a) && (d.address_ok(a.ip()) || d.mode == crate::settings::Mode::Log))
            .collect();
        if usable.is_empty() {
            if addrs.is_empty() {
                return Err(format!("{} has no addresses", t.host));
            }
            return Err(format!(
                "refused: {} resolves to a private address ({}): list the host by its exact name, or the range under \
                 the role's networks",
                t.host,
                addrs[0].ip()
            ));
        }
        if d.mode == crate::settings::Mode::Log && !usable.iter().any(|a| d.address_ok(a.ip())) && d.word == "allowed" {
            tracing::info!("{} resolves to a private address: refused once enforced", t.host);
        }
        let mut last = String::new();
        for a in usable {
            match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(a)).await {
                Ok(Ok(s)) => return Ok(s),
                Ok(Err(e)) => last = format!("{a}: {e}"),
                Err(_) => last = format!("{a}: timed out"),
            }
        }
        Err(last)
    }

    #[allow(clippy::too_many_arguments)]
    async fn log(&self, user: &Option<String>, d: &Decision, via: &str, t: &Target, up: u64, down: u64, ms: u64) {
        self.engine
            .record(Line {
                at: Timestamp::now(),
                user: user.clone().unwrap_or_else(|| "unknown".into()),
                role: d.role.clone(),
                via: via.into(),
                host: t.host.clone(),
                port: Some(t.port),
                decision: d.word.into(),
                reason: d.reason.clone(),
                up,
                down,
                ms,
            })
            .await;
    }

    async fn api(
        &self,
        client: &mut TcpStream,
        user: Option<String>,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> anyhow::Result<()> {
        let Some(user) = user else {
            return respond(client, 403, "text/plain", "couldn't tell which Unix user this comes from\n").await;
        };
        let (route, query) = path.split_once('?').unwrap_or((path, ""));
        let settings = self.engine.settings().await;
        let role = settings.role_of_user(&user);
        match (method, route) {
            ("GET", "/status") => {
                let state = self.engine.state.lock().await;
                let rs = role.and_then(|r| state.roles.get(&r.name)).cloned().unwrap_or_default();
                let v = json!({
                    "user": user,
                    "role": role.map(|r| &r.name),
                    "mode": role.map(|r| r.mode.word()).unwrap_or(settings.mode.word()),
                    "node": role.and_then(|r| r.node.clone()),
                    "presets": role.map(|r| r.presets.clone()).unwrap_or_default(),
                    "hosts": role.map(|r| r.patterns.iter().map(|(p, s)| json!({"host": p.to_string(), "from": s.words()})).collect::<Vec<_>>()).unwrap_or_default(),
                    "networks": role.map(|r| r.networks.iter().map(|c| format!("{}/{}", c.addr, c.prefix)).collect::<Vec<_>>()).unwrap_or_default(),
                    "approved": rs.approved.keys().collect::<Vec<_>>(),
                    "pending": rs.pending.values().map(|p| json!({"target": p.target, "request": p.id, "asked_at": p.asked_at})).collect::<Vec<_>>(),
                    "problem": self.engine.settings_problem.lock().await.clone(),
                });
                respond(client, 200, "application/json", &format!("{v}\n")).await
            }
            ("GET", "/why") => {
                let host = query.split('&').find_map(|kv| kv.strip_prefix("host=")).map(percent_decode);
                let host = host.as_deref().and_then(crate::hosts::normalise_host);
                let lines = self.engine.why(&user, host.as_deref()).await;
                respond(client, 200, "application/json", &format!("{}\n", serde_json::to_string(&lines)?)).await
            }
            ("POST", "/ask") => {
                #[derive(Deserialize)]
                struct Ask {
                    host: String,
                    #[serde(default)]
                    why: Option<String>,
                }
                // The role's node files the request (`fleetlayer-egress ask`); this only looks the answer up.
                let Ok(a) = serde_json::from_slice::<Ask>(body) else {
                    return respond(client, 400, "text/plain", "send {\"host\": \"…\"}\n").await;
                };
                let Some(t) = parse_authority(&a.host, 443) else {
                    return respond(client, 400, "text/plain", "host isn't a host name, name:port or IP address\n")
                        .await;
                };
                if let Some(w) = &a.why
                    && let Err(e) = crate::check_why(w)
                {
                    return respond(client, 400, "text/plain", &format!("{e}\n")).await;
                }
                let r = self.engine.ask(&user, &t, a.why.as_deref().unwrap_or("").trim()).await;
                respond(client, 200, "application/json", &format!("{}\n", serde_json::to_string(&r)?)).await
            }
            _ => {
                respond(client, 404, "text/plain", "the egress API has GET /status, GET /why?host=, POST /ask\n").await
            }
        }
    }
}

/// Read up to the end of an HTTP request head: (everything read, the head's length).
async fn read_head(s: &mut TcpStream) -> anyhow::Result<(Vec<u8>, usize)> {
    let mut buf = Vec::with_capacity(2048);
    loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            return Ok((buf, i + 4));
        }
        anyhow::ensure!(buf.len() < HEAD_MAX, "request head too large");
        let mut chunk = [0u8; 4096];
        let n = tokio::time::timeout(READ_TIMEOUT, s.read(&mut chunk)).await??;
        anyhow::ensure!(n > 0, "closed mid-request");
        buf.extend_from_slice(&chunk[..n]);
    }
}

async fn respond(s: &mut TcpStream, status: u16, kind: &str, body: &str) -> anyhow::Result<()> {
    let word = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        411 => "Length Required",
        413 => "Payload Too Large",
        _ => "Bad Gateway",
    };
    let marker = if status == 403 { "X-Fleetlayer-Egress: refused\r\n" } else { "" };
    let head = format!(
        "HTTP/1.1 {status} {word}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n{marker}Connection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).await?;
    s.write_all(body.as_bytes()).await?;
    let _ = s.shutdown().await;
    Ok(())
}

/// A plain HTTP request head rebuilt to be the connection's only request: `Connection: close` in place of any
/// connection headers, and the body's length. Err: the status and words to refuse it with.
pub fn one_request_head(head: &[u8]) -> Result<(Vec<u8>, u64), (u16, &'static str)> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    if !matches!(req.parse(head), Ok(httparse::Status::Complete(_))) {
        return Err((400, "a request head the proxy couldn't read\n"));
    }
    let mut out = format!("{} {} HTTP/1.1\r\n", req.method.unwrap_or("GET"), req.path.unwrap_or("/")).into_bytes();
    let mut body_len: Option<u64> = None;
    for h in req.headers.iter() {
        let name = h.name.to_ascii_lowercase();
        match name.as_str() {
            "transfer-encoding" => {
                return Err((
                    411,
                    "plain HTTP through Fleetlayer egress needs a Content-Length (no chunked bodies): use HTTPS\n",
                ));
            }
            "content-length" => {
                let n = std::str::from_utf8(h.value).ok().and_then(|v| v.trim().parse::<u64>().ok());
                match (n, body_len) {
                    (Some(n), None) => body_len = Some(n),
                    (Some(n), Some(m)) if n == m => {}
                    _ => return Err((400, "a bad or repeated Content-Length\n")),
                }
            }
            "connection" | "proxy-connection" | "keep-alive" | "upgrade" => continue,
            _ => {}
        }
        out.extend_from_slice(h.name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(h.value);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    Ok((out, body_len.unwrap_or(0)))
}

/// What an agent reads when a host is refused: why, and how to ask.
pub fn refusal_text(t: &Target, d: &Decision) -> String {
    format!(
        "Fleetlayer egress refused {t}: {}.\nIf your work needs this host, ask for it with:\n  fleetlayer-egress ask {} \
         --why \"<what it's for>\"\nA person approves it, and it stays allowed from then on.\n",
        d.reason,
        if t.port == 443 || t.port == 80 { t.display_host() } else { t.to_string() }
    )
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}
