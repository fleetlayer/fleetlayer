//! The decisions: which role a user is in, whether a host is on its list or approved, and asking the control plane
//! for a host that isn't. Everything the proxy and the resolver share.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use jiff::Timestamp;
use serde::Serialize;
use tokio::sync::{Mutex, RwLock};

use crate::audit::{Line, Log};
use crate::hosts::{Target, is_private};
use crate::settings::{Mode, Role, Settings};
use crate::state::{Approved, Pending, State, approval_id};

/// The control plane's answer to "may this role connect to this host?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Allowed,
    /// Waiting for a person; the request's ID when there is one.
    Waiting(Option<String>),
    Refused(String),
    /// Nobody asked from the role's node (or its approval lapsed).
    NotAsked,
}

/// Looks up whether a role's node may connect to a host (`_fleetlayer.dev/access/check` with `peek`: it never files a
/// request; the role's own node does that, for `fleetlayer-egress ask`), or a test's stand-in.
#[async_trait]
pub trait Asker: Send + Sync {
    async fn ask(&self, node: &str, target: &str, why: &str) -> Result<Answer, String>;
}

/// No node to ask (the plugin runs outside one, or before it's linked).
pub struct NoAsker;

#[async_trait]
impl Asker for NoAsker {
    async fn ask(&self, _node: &str, _target: &str, _why: &str) -> Result<Answer, String> {
        Err("the egress plugin isn't linked to a control plane now".into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Decision {
    /// Whether the connection goes ahead (true for anything in log mode).
    pub go: bool,
    /// What the rules say: `allowed`, `denied`, or `would_deny` in log mode.
    pub word: &'static str,
    pub reason: String,
    pub role: Option<String>,
    /// Whether the host may resolve to private addresses (it was named exactly, or approved by name).
    #[serde(skip)]
    pub private_ok: bool,
    #[serde(skip)]
    pub networks: Vec<crate::hosts::Cidr>,
    #[serde(skip)]
    pub restricted: bool,
    #[serde(skip)]
    pub mode: Mode,
}

pub struct Engine {
    pub settings: RwLock<Arc<Settings>>,
    pub state: Mutex<State>,
    pub state_path: PathBuf,
    pub log: Mutex<Log>,
    pub users: std::sync::Mutex<crate::procnet::Users>,
    pub asker: RwLock<Arc<dyn Asker>>,
    /// The latest decisions, for `why`.
    pub recent: Mutex<VecDeque<Line>>,
    /// The settings file's problem, when the last edit didn't parse (the last good settings keep deciding).
    pub settings_problem: Mutex<Option<String>>,
    /// DNS forwards that failed in a row.
    pub dns_failures: Mutex<u32>,
    /// When each (user, name, decision) was last logged, so a busy resolver doesn't flood the log.
    pub dns_logged: Mutex<HashMap<(String, String, &'static str), std::time::Instant>>,
    /// When each user last asked for a new host, so a hijacked agent can't flood its person with requests.
    pub asks: Mutex<HashMap<String, VecDeque<std::time::Instant>>>,
}

/// New host requests a user may file in an hour.
pub const ASKS_PER_HOUR: usize = 10;

const RECENT: usize = 500;

impl Engine {
    pub fn new(settings: Settings, state_dir: PathBuf) -> Arc<Engine> {
        let keep = settings.keep_days;
        let state_path = state_dir.join("state.json");
        Arc::new(Engine {
            settings: RwLock::new(Arc::new(settings)),
            state: Mutex::new(State::load(&state_path)),
            state_path,
            log: Mutex::new(Log::new(state_dir.join("audit"), keep)),
            users: std::sync::Mutex::default(),
            asker: RwLock::new(Arc::new(NoAsker)),
            recent: Mutex::default(),
            settings_problem: Mutex::default(),
            dns_failures: Mutex::default(),
            dns_logged: Mutex::default(),
            asks: Mutex::default(),
        })
    }

    pub async fn settings(&self) -> Arc<Settings> {
        self.settings.read().await.clone()
    }

    pub async fn user_name(&self, uid: u32) -> String {
        self.users.lock().ok().and_then(|mut u| u.name(uid)).unwrap_or_else(|| format!("uid {uid}"))
    }

    /// Whether `user` may connect to `t`.
    pub async fn decide(&self, user: Option<&str>, t: &Target) -> Decision {
        let settings = self.settings().await;
        let Some(user) = user else {
            // The connection's owner couldn't be found: refuse, whatever the mode.
            return Decision {
                go: false,
                word: "denied",
                reason: "couldn't tell which Unix user this connection comes from".into(),
                role: None,
                private_ok: false,
                networks: vec![],
                restricted: true,
                mode: Mode::Enforce,
            };
        };
        let Some(role) = settings.role_of_user(user) else {
            return Decision {
                go: true,
                word: "allowed",
                reason: "not in a restricted role".into(),
                role: None,
                private_ok: true,
                networks: vec![],
                restricted: false,
                mode: Mode::Log,
            };
        };
        let (allowed, reason, private_ok) = self.rule(role, t).await;
        verdict(role, allowed, reason, private_ok)
    }

    /// The rule for a role and target: (allowed, why, may reach private addresses).
    async fn rule(&self, role: &Role, t: &Target) -> (bool, String, bool) {
        if let Some((p, src)) = role.listed(t) {
            return (true, format!("{} ({p})", src.words()), p.is_exact());
        }
        let key = t.policy_name();
        let mut state = self.state.lock().await;
        let rs = state.roles.entry(role.name.clone()).or_default();
        if let Some(a) = rs.approved.get_mut(&key) {
            a.used_at = Some(Timestamp::now());
            return (true, "approved".into(), true);
        }
        if let Some(p) = rs.pending.get(&key) {
            let id = p.id.as_deref().map(|i| format!(" (request {i})")).unwrap_or_default();
            return (false, format!("waiting for a person to approve {key}{id}"), false);
        }
        (false, format!("{} isn't on role {}'s list", t.display_host(), role.name), false)
    }

    /// Whether a role's user may resolve `name`.
    pub async fn decide_name(&self, user: Option<&str>, name: &str) -> Decision {
        let settings = self.settings().await;
        let Some(user) = user else {
            return Decision {
                go: false,
                word: "denied",
                reason: "couldn't tell which Unix user this question comes from".into(),
                role: None,
                private_ok: false,
                networks: vec![],
                restricted: true,
                mode: Mode::Enforce,
            };
        };
        let Some(role) = settings.role_of_user(user) else {
            return Decision {
                go: true,
                word: "allowed",
                reason: "not in a restricted role".into(),
                role: None,
                private_ok: true,
                networks: vec![],
                restricted: false,
                mode: Mode::Log,
            };
        };
        let mut allowed = role.name_listed(name);
        if !allowed {
            let state = self.state.lock().await;
            allowed = state.roles.get(&role.name).is_some_and(|rs| {
                rs.approved.keys().any(|k| {
                    crate::hosts::parse_authority(k.trim_start_matches("host:"), 443).is_some_and(|t| t.host == name)
                })
            });
        }
        let reason =
            if allowed { "on the list".to_string() } else { format!("{name} isn't on role {}'s list", role.name) };
        verdict(role, allowed, reason, false)
    }

    pub async fn record(&self, line: Line) {
        if let Err(e) = self.log.lock().await.write(&line) {
            tracing::warn!("can't write the audit log: {e}");
        }
        let mut recent = self.recent.lock().await;
        if recent.len() >= RECENT {
            recent.pop_front();
        }
        recent.push_back(line);
    }

    pub async fn save_state(&self) {
        let mut state = self.state.lock().await;
        state.updated_at = Some(Timestamp::now());
        state.problem = self.settings_problem.lock().await.clone();
        if let Err(e) = state.save(&self.state_path) {
            tracing::warn!("can't save {}: {e}", self.state_path.display());
        }
    }

    /// An agent asks for a host: already allowed, approved now, waiting for a person, or refused.
    pub async fn ask(&self, user: &str, t: &Target, why: &str) -> AskResult {
        let settings = self.settings().await;
        let Some(role) = settings.role_of_user(user) else {
            return AskResult::already(format!("{user} isn't in a restricted role: nothing to ask"));
        };
        let (allowed, reason, _) = self.rule(role, t).await;
        if allowed {
            return AskResult::already(format!("{} is already allowed for role {} ({reason})", t, role.name));
        }
        let Some(node) = role.node.clone() else {
            return AskResult {
                status: "unavailable",
                message: format!(
                    "role {} has no node in the egress settings, so it can't ask through Fleetlayer. Ask your person \
                     to add {} to the role's hosts",
                    role.name,
                    t.display_host()
                ),
                request: None,
            };
        };
        let target = t.policy_name();
        let already_asked =
            self.state.lock().await.roles.get(&role.name).is_some_and(|rs| rs.pending.contains_key(&target));
        if !already_asked {
            let mut asks = self.asks.lock().await;
            let mine = asks.entry(user.to_string()).or_default();
            while mine.front().is_some_and(|t| t.elapsed().as_secs() >= 3600) {
                mine.pop_front();
            }
            if mine.len() >= ASKS_PER_HOUR {
                return AskResult {
                    status: "refused",
                    message: format!(
                        "{user} has asked for {ASKS_PER_HOUR} new hosts in the last hour, the most allowed: tell your \
                         person what you need instead"
                    ),
                    request: None,
                };
            }
            mine.push_back(std::time::Instant::now());
        }
        let asker = self.asker.read().await.clone();
        let answer = asker.ask(&node, &target, why).await;
        let out = self.apply_answer(&role.name, &target, why, answer).await;
        self.record(Line {
            at: Timestamp::now(),
            user: user.to_string(),
            role: Some(role.name.clone()),
            via: "ask".into(),
            host: t.host.clone(),
            port: Some(t.port),
            decision: if out.status == "allowed" { "allowed".into() } else { "denied".into() },
            reason: format!("{}; why: {why}", out.message),
            up: 0,
            down: 0,
            ms: 0,
        })
        .await;
        out
    }

    async fn apply_answer(&self, role: &str, target: &str, why: &str, answer: Result<Answer, String>) -> AskResult {
        let now = Timestamp::now();
        let out = {
            let mut state = self.state.lock().await;
            let rs = state.roles.entry(role.to_string()).or_default();
            match answer {
                Ok(Answer::Allowed) => {
                    rs.pending.remove(target);
                    rs.approved.insert(
                        target.to_string(),
                        Approved { target: target.to_string(), checked_at: now, used_at: None },
                    );
                    AskResult {
                        status: "allowed",
                        message: format!("{target} is allowed for role {role} from now on"),
                        request: None,
                    }
                }
                Ok(Answer::Waiting(id)) => {
                    let asked_at = rs.pending.get(target).map(|p| p.asked_at).unwrap_or(now);
                    rs.pending.insert(
                        target.to_string(),
                        Pending {
                            target: target.to_string(),
                            id: id.clone(),
                            why: why.to_string(),
                            asked_at,
                            checked_at: now,
                        },
                    );
                    let approve = id
                        .as_deref()
                        .map(|i| format!(" (`fleetlayer-control approvals approve {i}`)"))
                        .unwrap_or_default();
                    AskResult {
                        status: "waiting",
                        message: format!(
                            "asked: {target} waits for your person's approval{approve}. Tell them what it's for; it \
                             works as soon as they approve it"
                        ),
                        request: id,
                    }
                }
                Ok(Answer::NotAsked) => {
                    rs.pending.remove(target);
                    rs.approved.remove(target);
                    AskResult {
                        status: "not_asked",
                        message: format!(
                            "{target} hasn't been asked for from role {role}'s node: `fleetlayer-egress ask` asks through \
                             the node of the session it runs in, which must be the role's node"
                        ),
                        request: None,
                    }
                }
                Ok(Answer::Refused(r)) => {
                    rs.pending.remove(target);
                    rs.approved.remove(target);
                    AskResult { status: "refused", message: format!("refused: {r}"), request: None }
                }
                Err(e) => AskResult {
                    status: "unavailable",
                    message: format!("couldn't ask: {e}. Try again later, or ask your person"),
                    request: None,
                },
            }
        };
        self.save_state().await;
        out
    }

    /// Check waiting requests again (every minute for the first hour, then every ten minutes, giving up after a day),
    /// and approved hosts in use once a day (which keeps their approval from lapsing, and drops a revoked one).
    pub async fn recheck(&self) {
        let now = Timestamp::now();
        let settings = self.settings().await;
        let mut due: Vec<(String, String, String, String)> = vec![]; // role, node, target, why
        {
            let mut state = self.state.lock().await;
            for (role_name, rs) in state.roles.iter_mut() {
                let Some(node) = settings.role(role_name).and_then(|r| r.node.clone()) else { continue };
                rs.pending.retain(|_, p| now.duration_since(p.asked_at).as_secs() < 86_400);
                for p in rs.pending.values() {
                    let age = now.duration_since(p.asked_at).as_secs();
                    let since = now.duration_since(p.checked_at).as_secs();
                    if (age < 3600 && since >= 60) || since >= 600 {
                        due.push((role_name.clone(), node.clone(), p.target.clone(), p.why.clone()));
                    }
                }
                for a in rs.approved.values() {
                    let used_since = a.used_at.is_some_and(|u| u > a.checked_at);
                    if used_since && now.duration_since(a.checked_at).as_secs() >= 86_400 {
                        due.push((role_name.clone(), node.clone(), a.target.clone(), "still in use".into()));
                    }
                }
            }
        }
        if due.is_empty() {
            return;
        }
        let asker = self.asker.read().await.clone();
        for (role, node, target, why) in due {
            let answer = asker.ask(&node, &target, &why).await;
            match answer {
                // The control plane out of reach: keep what's remembered, try again later.
                Err(_) => {
                    let mut state = self.state.lock().await;
                    if let Some(rs) = state.roles.get_mut(&role) {
                        if let Some(p) = rs.pending.get_mut(&target) {
                            p.checked_at = now;
                        }
                        if let Some(a) = rs.approved.get_mut(&target) {
                            a.checked_at = now;
                        }
                    }
                }
                a => {
                    let _ = self.apply_answer(&role, &target, &why, a).await;
                }
            }
        }
        self.save_state().await;
    }

    /// The decisions a user's connections got lately for a host (or all), newest first.
    pub async fn why(&self, user: &str, host: Option<&str>) -> Vec<Line> {
        let recent = self.recent.lock().await;
        recent
            .iter()
            .rev()
            .filter(|l| l.user == user && host.is_none_or(|h| l.host == h || l.host.ends_with(&format!(".{h}"))))
            .take(20)
            .cloned()
            .collect()
    }
}

fn verdict(role: &Role, allowed: bool, reason: String, private_ok: bool) -> Decision {
    let word = match (allowed, role.mode) {
        (true, _) => "allowed",
        (false, Mode::Log) => "would_deny",
        (false, Mode::Enforce) => "denied",
    };
    Decision {
        go: allowed || role.mode == Mode::Log,
        word,
        reason,
        role: Some(role.name.clone()),
        private_ok,
        networks: role.networks.clone(),
        restricted: true,
        mode: role.mode,
    }
}

impl Decision {
    /// Whether this connection may reach `ip`: anything public, and a private address (the LAN, the tailnet,
    /// loopback, cloud metadata) only in one of the role's networks. A listed or approved name never widens that: its
    /// owner can point it anywhere.
    pub fn address_ok(&self, ip: IpAddr) -> bool {
        !self.restricted || !is_private(ip) || self.networks.iter().any(|c| c.contains(ip))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AskResult {
    /// `allowed`, `already`, `waiting`, `refused` or `unavailable`.
    pub status: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<String>,
}

impl AskResult {
    fn already(message: String) -> Self {
        AskResult { status: "already", message, request: None }
    }
}

/// The node's access check, turned into an [`Answer`].
pub fn answer_from(d: &fleetlayer_protocol::link::AccessDecision) -> Answer {
    if d.allowed {
        Answer::Allowed
    } else if d.reason.contains("not asked yet") {
        Answer::NotAsked
    } else if d.reason.contains("waiting for the person to approve") {
        Answer::Waiting(approval_id(&d.reason))
    } else {
        Answer::Refused(d.reason.clone())
    }
}
