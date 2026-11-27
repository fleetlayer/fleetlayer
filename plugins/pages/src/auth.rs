//! Who is looking, and what they may read: the one seam sign-in goes through.
//!
//! Two ways in, either or both configured:
//!
//! - **Fleetlayer's sign-in** (`oidc.rs`): a person, from the ID token. What they may read is asked of the node per
//!   request, `_fleetlayer.dev/access/check {tenant, who: <sub>, can: read, to: page:<space>/… | app:<space>/<name>}`,
//!   and remembered for a minute. When the check can't be made, the answer is "can't tell" and the page says so: it
//!   never falls open.
//! - **The login key**, the hub's kind of login: one long random key compared in constant time, five failures in 15
//!   minutes lock that client address out for 15 minutes. Whoever has the key reads every space. It stays until a
//!   deployment drops `login_file`.
//!
//! Either way the session is a cookie (HttpOnly, Secure, SameSite=Strict) lasting 12 hours, kept in memory.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, header};
use fleetlayer_plugin::Peer;
use fleetlayer_protocol::ext;
use fleetlayer_protocol::link::{AccessCheck, AccessDecision};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

pub const COOKIE: &str = "pages_session";
const SESSION: Duration = Duration::from_secs(12 * 3600);
const LOCKOUT_FAILURES: usize = 5;
const LOCKOUT: Duration = Duration::from_secs(15 * 60);
/// How long an access decision is remembered.
const DECISION: Duration = Duration::from_secs(60);

/// A person signed in through Fleetlayer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    /// The person's ID (the token's `sub`), never reused.
    pub sub: String,
    /// The tenant they signed in to (`fleetlayer.dev/tenant`).
    pub tenant: String,
    pub handle: Option<String>,
    pub name: Option<String>,
}

/// Someone signed in.
#[derive(Debug, Clone)]
pub struct Viewer {
    /// None for the holder of the login key, who reads everything.
    pub person: Option<Person>,
}

impl Viewer {
    /// What `/_fl/me` reports: the person's ID, or None for the login key.
    pub fn principal(&self) -> Option<String> {
        self.person.as_ref().map(|p| p.sub.clone())
    }
}

/// An access decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    Yes,
    No,
    /// The check couldn't be made (the control plane is down): say so, and don't show the page.
    CantTell(String),
}

pub struct LoginKey {
    key: String,
    failures: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
}

/// A test's decision from the target alone.
#[cfg(test)]
pub type Decide = Box<dyn Fn(&str) -> Result<bool, String> + Send + Sync>;

/// Asks whether a person may do something.
pub enum Checker {
    /// The node this plugin runs under (`_fleetlayer.dev/access/check`, grant `access.check`).
    Node(Peer),
    /// For tests: decides from the target alone.
    #[cfg(test)]
    Fixed(Decide),
}

impl Checker {
    async fn check(&self, person: &Person, target: &str) -> Result<bool, String> {
        match self {
            Checker::Node(peer) => {
                let ask = AccessCheck {
                    tenant: person.tenant.clone(),
                    who: person.sub.clone(),
                    can: "read".into(),
                    to: target.to_string(),
                };
                peer.request_within::<_, AccessDecision>(ext::method::ACCESS_CHECK, &ask, Duration::from_secs(10))
                    .await
                    .map(|d| d.allowed)
                    .map_err(|e| e.message)
            }
            #[cfg(test)]
            Checker::Fixed(f) => f(target),
        }
    }
}

pub struct Auth {
    pub key: Option<LoginKey>,
    pub oidc: Option<crate::oidc::Oidc>,
    checker: Option<Checker>,
    sessions: Mutex<HashMap<String, (Viewer, Instant)>>,
    decisions: Mutex<HashMap<(String, String), (bool, Instant)>>,
}

impl LoginKey {
    pub fn new(key: String) -> anyhow::Result<Self> {
        let key = key.trim().to_string();
        anyhow::ensure!(key.len() >= 32, "the login key must be at least 32 characters");
        Ok(LoginKey { key, failures: Mutex::default() })
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
}

impl Auth {
    /// Sign-in through Fleetlayer needs a checker (the node); the login key needs none.
    pub fn new(
        key: Option<LoginKey>,
        oidc: Option<crate::oidc::Oidc>,
        checker: Option<Checker>,
    ) -> anyhow::Result<Auth> {
        anyhow::ensure!(key.is_some() || oidc.is_some(), "set signin, login_file, or both");
        anyhow::ensure!(
            oidc.is_none() || checker.is_some(),
            "signin needs a Fleetlayer node to ask about access: run pages under a node, or use login_file only"
        );
        Ok(Auth { key, oidc, checker, sessions: Mutex::default(), decisions: Mutex::default() })
    }

    pub async fn start(&self, viewer: Viewer) -> String {
        let sid = new_id();
        self.sessions.lock().await.insert(sid.clone(), (viewer, Instant::now()));
        sid
    }

    pub async fn end(&self, headers: &HeaderMap) -> Option<Viewer> {
        let sid = session_id(headers)?;
        self.sessions.lock().await.remove(&sid).map(|(v, _)| v)
    }

    pub async fn viewer(&self, headers: &HeaderMap) -> Option<Viewer> {
        let sid = session_id(headers)?;
        let mut s = self.sessions.lock().await;
        s.retain(|_, (_, t)| t.elapsed() < SESSION);
        let (v, _) = s.get(&sid)?;
        // A login-key session ends when the deployment drops the key.
        (v.person.is_some() || self.key.is_some()).then(|| v.clone())
    }

    /// Whether the viewer may read `target` (`page:<space>/*`, `page:<space>/<folder>`, `app:<space>/<name>`).
    pub async fn may(&self, viewer: &Viewer, target: &str) -> Access {
        let Some(person) = &viewer.person else { return Access::Yes };
        let Some(checker) = &self.checker else { return Access::No };
        let k = (person.sub.clone(), target.to_string());
        if let Some((ok, at)) = self.decisions.lock().await.get(&k)
            && at.elapsed() < DECISION
        {
            return if *ok { Access::Yes } else { Access::No };
        }
        match checker.check(person, target).await {
            Ok(ok) => {
                let mut d = self.decisions.lock().await;
                d.retain(|_, (_, at)| at.elapsed() < DECISION);
                d.insert(k, (ok, Instant::now()));
                if ok { Access::Yes } else { Access::No }
            }
            Err(e) => {
                tracing::warn!(who = %person.sub, target, "the access check failed: {e}");
                Access::CantTell(e)
            }
        }
    }
}

pub fn session_id(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    cookie.split(';').find_map(|c| c.trim().strip_prefix(&format!("{COOKIE}=")).map(str::to_string))
}

/// The session cookie. `secure` is false only when the site is served over plain HTTP on loopback (a laptop's
/// `pages.localhost`), where browsers refuse Secure cookies on a subdomain.
pub fn cookie(sid: &str, secure: bool) -> String {
    let s = if secure { "; Secure" } else { "" };
    format!("{COOKIE}={sid}; HttpOnly{s}; SameSite=Strict; Path=/; Max-Age={}", SESSION.as_secs())
}

pub fn clear_cookie(secure: bool) -> String {
    let s = if secure { "; Secure" } else { "" };
    format!("{COOKIE}=; HttpOnly{s}; SameSite=Strict; Path=/; Max-Age=0")
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

    use std::sync::Arc;

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
        let auth = Auth::new(Some(k), None, None).unwrap();
        let sid = auth.start(Viewer { person: None }).await;
        let mut h = HeaderMap::new();
        assert!(auth.viewer(&h).await.is_none());
        h.insert(header::COOKIE, format!("other=1; {COOKIE}={sid}").parse().unwrap());
        assert!(auth.viewer(&h).await.is_some());
        assert_eq!(auth.may(&Viewer { person: None }, "page:x/*").await, Access::Yes);
    }

    fn alice() -> Viewer {
        Viewer {
            person: Some(Person { sub: "p_1".into(), tenant: "home".into(), handle: Some("alice".into()), name: None }),
        }
    }

    #[tokio::test]
    async fn a_person_reads_what_the_check_allows_and_nothing_when_it_cant_be_asked() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let checker = Checker::Fixed(Box::new(move |t: &str| {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match t {
                "page:personal/*" => Ok(true),
                "page:down/*" => Err("the control plane didn't answer".into()),
                _ => Ok(false),
            }
        }));
        let f = crate::oidc::tests::FakeIssuer::start().await;
        let oidc = crate::oidc::Oidc::new(&f.url, "pages", "s", "https://p.example").unwrap();
        let auth = Auth::new(None, Some(oidc), Some(checker)).unwrap();
        assert_eq!(auth.may(&alice(), "page:personal/*").await, Access::Yes);
        assert_eq!(auth.may(&alice(), "page:personal/*").await, Access::Yes);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1, "remembered for a minute");
        assert_eq!(auth.may(&alice(), "page:work/*").await, Access::No);
        assert!(matches!(auth.may(&alice(), "page:down/*").await, Access::CantTell(_)));
        // Without the key, a key session isn't a session.
        let sid = auth.start(Viewer { person: None }).await;
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, format!("{COOKIE}={sid}").parse().unwrap());
        assert!(auth.viewer(&h).await.is_none());
    }

    #[tokio::test]
    async fn signin_without_a_node_is_refused() {
        let f = crate::oidc::tests::FakeIssuer::start().await;
        let oidc = crate::oidc::Oidc::new(&f.url, "pages", "s", "https://p.example").unwrap();
        assert!(Auth::new(None, Some(oidc), None).is_err());
        assert!(Auth::new(None, None, None).is_err());
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
