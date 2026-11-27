//! Who is looking, and which spaces they may read: the one seam sign-in goes through.
//!
//! Until Fleetlayer's single sign-on ships, there's the hub's kind of login: one long random key compared in constant
//! time, five failures in 15 minutes lock that client address out for 15 minutes, and a session cookie (HttpOnly,
//! Secure, SameSite=Strict) lasting 12 hours, kept in memory. Whoever has the key reads every space.
//!
//! With sign-on, a `Viewer` is a person (`person:<name>` from the ID token) and `may_read` asks the node
//! `_fleetlayer.dev/access/check {who, can: "read", to: "page:<space>/<folder>"}`.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, header};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

pub const COOKIE: &str = "pages_session";
const SESSION: Duration = Duration::from_secs(12 * 3600);
const LOCKOUT_FAILURES: usize = 5;
const LOCKOUT: Duration = Duration::from_secs(15 * 60);

/// Someone signed in.
#[derive(Debug, Clone)]
pub struct Viewer {
    /// `person:<name>` once sign-in goes through Fleetlayer; None for the holder of the login key.
    #[allow(dead_code)] // read once sign-in goes through Fleetlayer
    pub principal: Option<String>,
}

pub struct LoginKey {
    key: String,
    sessions: Mutex<HashMap<String, Instant>>,
    failures: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
}

pub enum Auth {
    LoginKey(LoginKey),
}

impl LoginKey {
    pub fn new(key: String) -> anyhow::Result<Self> {
        let key = key.trim().to_string();
        anyhow::ensure!(key.len() >= 32, "the login key must be at least 32 characters");
        Ok(LoginKey { key, sessions: Mutex::default(), failures: Mutex::default() })
    }

    pub async fn locked_out(&self, ip: IpAddr) -> bool {
        let mut f = self.failures.lock().await;
        let q = f.entry(ip).or_default();
        while q.front().is_some_and(|t| t.elapsed() > LOCKOUT) {
            q.pop_front();
        }
        q.len() >= LOCKOUT_FAILURES
    }

    pub async fn failed(&self, ip: IpAddr) {
        self.failures.lock().await.entry(ip).or_default().push_back(Instant::now());
    }

    /// Compares hashes, so the time taken doesn't depend on where the values differ.
    pub fn check(&self, given: &str) -> bool {
        let a = Sha256::digest(given.trim().as_bytes());
        let b = Sha256::digest(self.key.as_bytes());
        a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }

    pub async fn start(&self) -> String {
        let sid = new_id();
        self.sessions.lock().await.insert(sid.clone(), Instant::now());
        sid
    }

    pub async fn end(&self, headers: &HeaderMap) {
        if let Some(sid) = session_id(headers) {
            self.sessions.lock().await.remove(&sid);
        }
    }

    async fn valid(&self, headers: &HeaderMap) -> bool {
        let Some(sid) = session_id(headers) else { return false };
        let mut s = self.sessions.lock().await;
        s.retain(|_, t| t.elapsed() < SESSION);
        s.contains_key(&sid)
    }
}

impl Auth {
    pub async fn viewer(&self, headers: &HeaderMap) -> Option<Viewer> {
        match self {
            Auth::LoginKey(k) => k.valid(headers).await.then_some(Viewer { principal: None }),
        }
    }

    /// Whether the viewer may read a space. With the login key, every space.
    pub async fn may_read(&self, _viewer: &Viewer, _space: &str) -> bool {
        match self {
            Auth::LoginKey(_) => true,
        }
    }
}

pub fn session_id(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    cookie.split(';').find_map(|c| c.trim().strip_prefix(&format!("{COOKIE}=")).map(str::to_string))
}

pub fn cookie(sid: &str) -> String {
    format!("{COOKIE}={sid}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={}", SESSION.as_secs())
}

pub fn clear_cookie() -> String {
    format!("{COOKIE}=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0")
}

/// 32 random bytes from the kernel (`/dev/urandom` on Linux and macOS), hex.
pub fn new_id() -> String {
    use std::io::Read;
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).expect("reading /dev/urandom");
    hex::encode(b)
}

/// The client's address: from `X-Forwarded-For` only when the connection comes from a trusted proxy (the right-most
/// address that isn't one), else the connection's own.
pub fn client_ip(peer: IpAddr, headers: &HeaderMap, trusted: &[IpAddr]) -> IpAddr {
    if !trusted.contains(&peer) {
        return peer;
    }
    let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) else { return peer };
    xff.split(',')
        .rev()
        .filter_map(|s| s.trim().parse::<IpAddr>().ok())
        .find(|ip| !trusted.contains(ip))
        .unwrap_or(peer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn five_failures_lock_an_address_out() {
        let k = LoginKey::new("k".repeat(40)).unwrap();
        let ip: IpAddr = "10.0.0.9".parse().unwrap();
        for _ in 0..5 {
            assert!(!k.locked_out(ip).await);
            k.failed(ip).await;
        }
        assert!(k.locked_out(ip).await);
        assert!(!k.locked_out("10.0.0.8".parse().unwrap()).await);
    }

    #[tokio::test]
    async fn a_session_cookie_signs_in() {
        let k = LoginKey::new("k".repeat(40)).unwrap();
        assert!(k.check(&format!(" {} ", "k".repeat(40))));
        assert!(!k.check("nope"));
        let sid = k.start().await;
        let auth = Auth::LoginKey(k);
        let mut h = HeaderMap::new();
        assert!(auth.viewer(&h).await.is_none());
        h.insert(header::COOKIE, format!("other=1; {COOKIE}={sid}").parse().unwrap());
        assert!(auth.viewer(&h).await.is_some());
    }

    #[test]
    fn short_keys_are_refused() {
        assert!(LoginKey::new("short".into()).is_err());
    }

    #[test]
    fn the_client_address_comes_from_a_trusted_proxy_only() {
        let proxy: IpAddr = "10.0.0.2".parse().unwrap();
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "1.2.3.4, 10.0.0.2".parse().unwrap());
        assert_eq!(client_ip(proxy, &h, &[proxy]), "1.2.3.4".parse::<IpAddr>().unwrap());
        assert_eq!(client_ip("10.0.0.7".parse().unwrap(), &h, &[proxy]), "10.0.0.7".parse::<IpAddr>().unwrap());
    }
}
