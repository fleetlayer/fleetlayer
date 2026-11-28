//! Who is looking, and what they may do.
//!
//! - **Fleetlayer's sign-in**: a person, from the ID token. Each request asks the node whether they may still sign in
//!   to the dashboard (`sign_in` on `service:dashboard`), and an action asks whether they may `admin` it; the answers
//!   are remembered for a minute. When the node can't be asked, the answer is "can't tell" and nothing is shown or
//!   done: it never falls open.
//! - **A login key**, for a dashboard without the sign-in service: one long random key compared in constant time, five
//!   failures in 15 minutes lock that address out for 15 minutes. Whoever has it may do everything the dashboard's own
//!   token allows.
//!
//! Either way the session is a cookie (HttpOnly, Secure, SameSite=Strict) lasting 12 hours, kept in memory, with a
//! CSRF token for its forms.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, header};
use fleetlayer_oidc::{Oidc, Person};
use fleetlayer_plugin::Peer;
use fleetlayer_protocol::ext;
use fleetlayer_protocol::link::{AccessCheck, AccessDecision};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

pub const COOKIE: &str = "dashboard_session";
const SESSION: Duration = Duration::from_secs(12 * 3600);
const LOCKOUT_FAILURES: usize = 5;
const LOCKOUT: Duration = Duration::from_secs(15 * 60);
const DECISION: Duration = Duration::from_secs(60);

pub use fleetlayer_oidc::new_id;

#[derive(Debug, Clone)]
pub struct Viewer {
    /// None for the login key's holder (and the demo), who may do everything the token allows.
    pub person: Option<Person>,
    pub csrf: String,
    /// When the person last used their passkey (the ID token's `auth_time`, seconds), for changes that are hard to
    /// undo; None for the login key.
    pub auth_time: Option<i64>,
}

impl Viewer {
    /// Whether they used their passkey within the last `secs` seconds (the login key's holder always counts).
    pub fn fresh(&self, secs: i64) -> bool {
        match (&self.person, self.auth_time) {
            (None, _) => true,
            (Some(_), Some(t)) => jiff::Timestamp::now().as_second() - t <= secs,
            (Some(_), None) => false,
        }
    }

    pub fn label(&self) -> String {
        match &self.person {
            Some(p) => p.name.clone().or_else(|| p.handle.clone()).unwrap_or_else(|| p.sub.clone()),
            None => "Login key".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    Yes,
    No,
    CantTell(String),
}

pub struct LoginKey {
    key: String,
    failures: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
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

    pub fn check(&self, given: &str) -> bool {
        let a = Sha256::digest(given.trim().as_bytes());
        let b = Sha256::digest(self.key.as_bytes());
        a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

#[cfg(test)]
pub type Decide = Box<dyn Fn(&str, &str) -> Result<bool, String> + Send + Sync>;

pub enum Checker {
    /// The node this plugin runs under (`_fleetlayer.dev/access/check`, grant `access.check`).
    Node(Peer),
    /// For tests: decides from the verb and target.
    #[cfg(test)]
    Fixed(Decide),
}

impl Checker {
    async fn check(&self, person: &Person, can: &str, target: &str) -> Result<bool, String> {
        match self {
            Checker::Node(peer) => {
                let ask = AccessCheck {
                    tenant: person.tenant.clone(),
                    who: person.sub.clone(),
                    can: can.into(),
                    to: target.into(),
                    ..Default::default()
                };
                peer.request_within::<_, AccessDecision>(ext::method::ACCESS_CHECK, &ask, Duration::from_secs(10))
                    .await
                    .map(|d| d.allowed)
                    .map_err(|e| e.message)
            }
            #[cfg(test)]
            Checker::Fixed(f) => f(can, target),
        }
    }
}

pub struct Auth {
    pub key: Option<LoginKey>,
    pub oidc: Option<Oidc>,
    checker: Option<Checker>,
    /// `service:<client>` in the policy.
    service: String,
    /// The demo: everyone is let in, as the login key's holder.
    open: bool,
    sessions: Mutex<HashMap<String, (Viewer, Instant)>>,
    decisions: Mutex<HashMap<(String, String), (bool, Instant)>>,
}

impl Auth {
    pub fn new(
        key: Option<LoginKey>,
        oidc: Option<Oidc>,
        checker: Option<Checker>,
        client: String,
    ) -> anyhow::Result<Auth> {
        anyhow::ensure!(key.is_some() || oidc.is_some(), "set signin, login_file, or both");
        anyhow::ensure!(
            oidc.is_none() || checker.is_some(),
            "signin needs a Fleetlayer node to ask about access: run the dashboard under a node, or use login_file"
        );
        Ok(Auth {
            key,
            oidc,
            checker,
            service: format!("service:{client}"),
            open: false,
            sessions: Mutex::default(),
            decisions: Mutex::default(),
        })
    }

    /// The demo's: no sign-in at all.
    pub fn open() -> Auth {
        Auth {
            key: None,
            oidc: None,
            checker: None,
            service: "service:dashboard".into(),
            open: true,
            sessions: Mutex::default(),
            decisions: Mutex::default(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub async fn start(&self, person: Option<Person>) -> String {
        self.start_at(person, None).await
    }

    /// A session for someone who used their passkey at `auth_time` (seconds).
    pub async fn start_at(&self, person: Option<Person>, auth_time: Option<i64>) -> String {
        let sid = new_id();
        let v = Viewer { person, csrf: new_id(), auth_time };
        let mut s = self.sessions.lock().await;
        s.retain(|_, (_, t)| t.elapsed() < SESSION);
        s.insert(sid.clone(), (v, Instant::now()));
        sid
    }

    pub async fn end(&self, headers: &HeaderMap) -> Option<Viewer> {
        let sid = session_id(headers)?;
        self.sessions.lock().await.remove(&sid).map(|(v, _)| v)
    }

    pub async fn end_signed_out(&self, out: &fleetlayer_oidc::SignedOut) -> usize {
        let mut s = self.sessions.lock().await;
        let before = s.len();
        s.retain(|_, (v, _)| !v.person.as_ref().is_some_and(|p| out.ends(p)));
        before - s.len()
    }

    pub async fn viewer(&self, headers: &HeaderMap) -> Option<Viewer> {
        if self.open {
            return Some(Viewer { person: None, csrf: "demo".into(), auth_time: None });
        }
        let sid = session_id(headers)?;
        let mut s = self.sessions.lock().await;
        s.retain(|_, (_, t)| t.elapsed() < SESSION);
        let (v, _) = s.get(&sid)?;
        (v.person.is_some() || self.key.is_some()).then(|| v.clone())
    }

    async fn may(&self, viewer: &Viewer, can: &str) -> Access {
        let Some(person) = &viewer.person else { return Access::Yes };
        let Some(checker) = &self.checker else { return Access::No };
        let k = (person.sub.clone(), can.to_string());
        if let Some((ok, at)) = self.decisions.lock().await.get(&k)
            && at.elapsed() < DECISION
        {
            return if *ok { Access::Yes } else { Access::No };
        }
        match checker.check(person, can, &self.service).await {
            Ok(ok) => {
                let mut d = self.decisions.lock().await;
                d.retain(|_, (_, at)| at.elapsed() < DECISION);
                d.insert(k, (ok, Instant::now()));
                if ok { Access::Yes } else { Access::No }
            }
            Err(e) => {
                tracing::warn!(who = %person.sub, can, "the access check failed: {e}");
                Access::CantTell(e)
            }
        }
    }

    /// May they look (`sign_in` on the dashboard's service)? Asked on every page, so taking someone's access away
    /// takes effect within a minute, not when their session ends.
    pub async fn may_view(&self, viewer: &Viewer) -> Access {
        self.may(viewer, "sign_in").await
    }

    /// May they act (`admin` on the dashboard's service): approve requests, close windows?
    pub async fn may_act(&self, viewer: &Viewer) -> Access {
        self.may(viewer, "admin").await
    }
}

pub fn session_id(headers: &HeaderMap) -> Option<String> {
    cookie_value(headers, COOKIE)
}

pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let c = headers.get(header::COOKIE)?.to_str().ok()?;
    c.split(';').find_map(|p| p.trim().strip_prefix(&format!("{name}=")).map(str::to_string))
}

pub fn cookie(sid: &str, secure: bool) -> String {
    let s = if secure { "; Secure" } else { "" };
    format!("{COOKIE}={sid}; HttpOnly{s}; SameSite=Strict; Path=/; Max-Age={}", SESSION.as_secs())
}

pub fn clear_cookie(secure: bool) -> String {
    let s = if secure { "; Secure" } else { "" };
    format!("{COOKIE}=; HttpOnly{s}; SameSite=Strict; Path=/; Max-Age=0")
}

/// The client's address: from `X-Forwarded-For` only when the connection comes from a trusted proxy.
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

    fn alice() -> Viewer {
        Viewer {
            person: Some(Person {
                sub: "p_1".into(),
                tenant: "home".into(),
                handle: Some("alice".into()),
                name: None,
                sid: None,
            }),
            csrf: "c".into(),
            auth_time: None,
        }
    }

    #[tokio::test]
    async fn a_person_sees_and_acts_as_the_policy_says_and_nothing_when_it_cant_be_asked() {
        let f = fleetlayer_oidc::testing::FakeIssuer::start().await;
        let oidc = Oidc::new(&f.url, "dashboard", "s", "https://d.example").unwrap();
        let checker = Checker::Fixed(Box::new(|can: &str, to: &str| {
            assert_eq!(to, "service:dashboard");
            match can {
                "sign_in" => Ok(true),
                _ => Ok(false),
            }
        }));
        let auth = Auth::new(None, Some(oidc), Some(checker), "dashboard".into()).unwrap();
        assert_eq!(auth.may_view(&alice()).await, Access::Yes);
        assert_eq!(auth.may_act(&alice()).await, Access::No);

        let oidc = Oidc::new(&f.url, "dashboard", "s", "https://d.example").unwrap();
        let down = Checker::Fixed(Box::new(|_: &str, _: &str| Err("no answer".into())));
        let auth = Auth::new(None, Some(oidc), Some(down), "dashboard".into()).unwrap();
        assert!(matches!(auth.may_view(&alice()).await, Access::CantTell(_)));
        assert!(matches!(auth.may_act(&alice()).await, Access::CantTell(_)));
    }

    #[tokio::test]
    async fn a_login_key_session_ends_when_the_key_is_gone_and_signin_needs_a_node() {
        let f = fleetlayer_oidc::testing::FakeIssuer::start().await;
        let oidc = Oidc::new(&f.url, "dashboard", "s", "https://d.example").unwrap();
        assert!(Auth::new(None, Some(oidc), None, "dashboard".into()).is_err());
        assert!(Auth::new(None, None, None, "dashboard".into()).is_err());

        let auth = Auth::new(Some(LoginKey::new("k".repeat(40)).unwrap()), None, None, "dashboard".into()).unwrap();
        let sid = auth.start(None).await;
        let mut h = HeaderMap::new();
        assert!(auth.viewer(&h).await.is_none());
        h.insert(header::COOKIE, format!("x=1; {COOKIE}={sid}").parse().unwrap());
        let v = auth.viewer(&h).await.unwrap();
        assert_eq!(auth.may_act(&v).await, Access::Yes);
        assert!(auth.end(&h).await.is_some());
        assert!(auth.viewer(&h).await.is_none());
    }

    #[tokio::test]
    async fn five_failures_lock_an_address_out() {
        let k = LoginKey::new("k".repeat(40)).unwrap();
        let ip: IpAddr = "10.0.0.9".parse().unwrap();
        for _ in 0..5 {
            assert!(!k.locked_out(ip).await);
            k.failed(ip).await;
        }
        assert!(k.locked_out(ip).await);
        assert!(k.check(&format!(" {} ", "k".repeat(40))));
        assert!(!k.check("nope"));
    }
}
