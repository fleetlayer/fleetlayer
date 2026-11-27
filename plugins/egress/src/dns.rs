//! The local resolver a restricted role's DNS questions are redirected to: it logs each name and forwards those on
//! the role's list to the real resolvers, and answers REFUSED for the rest once enforced (which also closes DNS as a
//! way to send data out in the names asked).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

use crate::audit::Line;
use crate::engine::Engine;

const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(3);

/// The first question of a DNS message: (name, type, where the question ends).
pub fn question(msg: &[u8]) -> Option<(String, u16, usize)> {
    if msg.len() < 12 || u16::from_be_bytes([msg[4], msg[5]]) == 0 {
        return None;
    }
    let mut at = 12;
    let mut labels = vec![];
    loop {
        let n = *msg.get(at)? as usize;
        at += 1;
        if n == 0 {
            break;
        }
        if n > 63 {
            return None; // compression pointers don't appear in a question's first name
        }
        let label = msg.get(at..at + n)?;
        labels.push(String::from_utf8_lossy(label).to_ascii_lowercase());
        at += n;
        if at > 12 + 255 {
            return None;
        }
    }
    let qtype = u16::from_be_bytes([*msg.get(at)?, *msg.get(at + 1)?]);
    msg.get(at + 3)?;
    Some((labels.join("."), qtype, at + 4))
}

/// A REFUSED answer to `msg`, carrying its question.
pub fn refused(msg: &[u8], question_end: usize) -> Vec<u8> {
    let mut out = msg[..question_end].to_vec();
    out[2] = 0x80 | (msg[2] & 0x79); // QR, the opcode and RD kept
    out[3] = 0x80 | 5; // RA, REFUSED
    out[4..6].copy_from_slice(&1u16.to_be_bytes());
    out[6..12].fill(0);
    out
}

/// The nameservers in /etc/resolv.conf.
pub fn system_upstreams() -> Vec<SocketAddr> {
    std::fs::read_to_string("/etc/resolv.conf")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver"))
        .filter_map(|ip| ip.trim().split('%').next()?.parse::<std::net::IpAddr>().ok())
        .map(|ip| SocketAddr::new(ip, 53))
        .collect()
}

#[derive(Clone)]
pub struct Resolver {
    pub engine: Arc<Engine>,
    /// Who sent a question from this address (user name). None when it can't be told.
    pub who: Arc<dyn Fn(SocketAddr) -> Option<String> + Send + Sync>,
    /// Who opened a TCP connection from this address.
    pub who_tcp: Arc<dyn Fn(SocketAddr) -> Option<String> + Send + Sync>,
}

impl Resolver {
    async fn upstreams(&self) -> Vec<SocketAddr> {
        let s = self.engine.settings().await;
        if s.dns_upstream.is_empty() { system_upstreams() } else { s.dns_upstream.clone() }
    }

    /// The answer to one message from `user`: forwarded, refused, or nothing (unreadable, or no resolver answered).
    async fn answer(&self, user: Option<String>, msg: &[u8], tcp: bool) -> Option<Vec<u8>> {
        let (name, qtype, end) = question(msg)?;
        let d = self.engine.decide_name(user.as_deref(), name.trim_end_matches('.')).await;
        self.log(&user, &name, qtype, &d).await;
        if !d.go {
            return Some(refused(msg, end));
        }
        let ups = self.upstreams().await;
        for up in ups {
            let r = if tcp { forward_tcp(up, msg).await } else { forward_udp(up, msg).await };
            if let Ok(a) = r {
                *self.engine.dns_failures.lock().await = 0;
                return Some(a);
            }
        }
        *self.engine.dns_failures.lock().await += 1;
        None
    }

    async fn log(&self, user: &Option<String>, name: &str, qtype: u16, d: &crate::engine::Decision) {
        let u = user.clone().unwrap_or_else(|| "unknown".into());
        if !d.restricted {
            return; // trusted users' questions aren't redirected here; nothing to audit
        }
        {
            let mut seen = self.engine.dns_logged.lock().await;
            let key = (u.clone(), name.to_string(), d.word);
            if seen.get(&key).is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
                return;
            }
            if seen.len() > 10_000 {
                seen.clear();
            }
            seen.insert(key, Instant::now());
        }
        self.engine
            .record(Line {
                at: Timestamp::now(),
                user: u,
                role: d.role.clone(),
                via: "dns".into(),
                host: name.to_string(),
                port: None,
                decision: d.word.into(),
                reason: format!("{} ({})", d.reason, qtype_name(qtype)),
                up: 0,
                down: 0,
                ms: 0,
            })
            .await;
    }

    pub async fn serve_udp(self, sock: UdpSocket) {
        let sock = Arc::new(sock);
        let mut buf = vec![0u8; 4096];
        loop {
            let Ok((n, peer)) = sock.recv_from(&mut buf).await else { continue };
            let msg = buf[..n].to_vec();
            let (s, sock) = (self.clone(), sock.clone());
            tokio::spawn(async move {
                let user = (s.who)(peer);
                if let Some(a) = s.answer(user, &msg, false).await {
                    let _ = sock.send_to(&a, peer).await;
                }
            });
        }
    }

    pub async fn serve_tcp(self, listener: TcpListener) {
        loop {
            let Ok((stream, peer)) = listener.accept().await else { continue };
            let s = self.clone();
            tokio::spawn(async move {
                let user = (s.who_tcp)(peer);
                let _ = s.tcp_conn(stream, user).await;
            });
        }
    }

    async fn tcp_conn(&self, mut stream: TcpStream, user: Option<String>) -> anyhow::Result<()> {
        loop {
            let mut len = [0u8; 2];
            if tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut len)).await.is_err() {
                return Ok(());
            }
            let mut msg = vec![0u8; u16::from_be_bytes(len) as usize];
            tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut msg)).await??;
            let Some(a) = self.answer(user.clone(), &msg, true).await else { return Ok(()) };
            stream.write_all(&(a.len() as u16).to_be_bytes()).await?;
            stream.write_all(&a).await?;
        }
    }
}

async fn forward_udp(up: SocketAddr, msg: &[u8]) -> anyhow::Result<Vec<u8>> {
    let bind: SocketAddr = if up.is_ipv4() { "0.0.0.0:0".parse()? } else { "[::]:0".parse()? };
    let sock = UdpSocket::bind(bind).await?;
    sock.connect(up).await?;
    sock.send(msg).await?;
    let mut buf = vec![0u8; 4096];
    loop {
        let n = tokio::time::timeout(UPSTREAM_TIMEOUT, sock.recv(&mut buf)).await??;
        // Only the answer to this question (same ID).
        if n >= 2 && buf[..2] == msg[..2] {
            return Ok(buf[..n].to_vec());
        }
    }
}

async fn forward_tcp(up: SocketAddr, msg: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut s = tokio::time::timeout(UPSTREAM_TIMEOUT, TcpStream::connect(up)).await??;
    s.write_all(&(msg.len() as u16).to_be_bytes()).await?;
    s.write_all(msg).await?;
    let mut len = [0u8; 2];
    tokio::time::timeout(UPSTREAM_TIMEOUT, s.read_exact(&mut len)).await??;
    let mut a = vec![0u8; u16::from_be_bytes(len) as usize];
    tokio::time::timeout(UPSTREAM_TIMEOUT, s.read_exact(&mut a)).await??;
    Ok(a)
}

fn qtype_name(t: u16) -> String {
    match t {
        1 => "A".into(),
        28 => "AAAA".into(),
        5 => "CNAME".into(),
        15 => "MX".into(),
        16 => "TXT".into(),
        33 => "SRV".into(),
        65 => "HTTPS".into(),
        12 => "PTR".into(),
        n => format!("type {n}"),
    }
}

/// A query for `name` (type A), for tests.
#[cfg(test)]
pub fn query(id: u16, name: &str) -> Vec<u8> {
    let mut m = id.to_be_bytes().to_vec();
    m.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in name.trim_end_matches('.').split('.') {
        m.push(label.len() as u8);
        m.extend_from_slice(label.as_bytes());
    }
    m.extend_from_slice(&[0, 0, 1, 0, 1]);
    m
}
