//! The usage monitor: the one process that holds what every session shares. It
//! reads each login's sign-in, polls usage once per login (not once per session),
//! runs browser sign-ins one at a time, keeps the plugin's settings and counts the
//! sessions on each login. Where a session goes is the rotation plugin's to
//! decide, from this state. A port of claude-account-rotator's
//! `coordinator/core.ts`, with the same behaviour and the same API
//! (`docs/coordinator-api.md`): everything outside it is passed in, so tests drive
//! it in memory on a clock they move.
//!
//! Changed for Fleetlayer (MIT notice in NOTICE): borrow mode, where the slots hold setup tokens from the auth hub, so
//! nothing is polled, refreshed or signed in, and accounts and readings come from the hub (`borrowing`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use futures::future::{BoxFuture, FutureExt, Shared};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::ReadingSource;
use super::model::*;

/// How often the monitor re-reads every sign-in and prunes dead sessions.
pub const TICK_MS: Ms = 60_000;
/// A login's usage is polled once its last reading is this old: sessions' responses keep busy logins fresher.
pub const POLL_MS: Ms = 5 * 60_000;
/// A check asked for fresh usage reuses a reading at most this old.
pub const FRESH_MS: Ms = 60_000;
/// A session with no open long poll and no request for this long is gone, even where its process can't be checked.
pub const STALE_SESSION_MS: Ms = 10 * 60_000;
/// After a failed usage call, how long before the next: doubling with each failure, up to the most.
pub const BACKOFF_MS: Ms = 2 * 60_000;
pub const MAX_BACKOFF_MS: Ms = 15 * 60_000;
/// After Claude Code failed to refresh a login's expired token, try again this long after.
pub const REFRESH_RETRY_MS: Ms = 10 * 60_000;
/// The longest a long poll waits.
pub const MAX_WAIT_MS: Ms = 55_000;
const NOTICE_KEEP: usize = 20;
/// Why a login whose token a session saw rejected needs a sign-in (or, in borrow mode, a new token).
const REJECTED: &str = "sign-in rejected";

#[derive(Clone, Debug, PartialEq)]
pub enum Stored {
    Present(Option<Credential>),
    Absent,
    Unreadable(i32),
}

#[derive(Clone, Debug, PartialEq)]
pub enum UsageAnswer {
    Ok(Value),
    Rejected,
    Failed { status: u16, retry_after_ms: Option<Ms> },
}

/// A browser sign-in under way: what it ended with (exit code, stderr), and how to stop it.
pub struct SignInRun {
    pub done: BoxFuture<'static, (Option<i32>, String)>,
    pub cancel: Arc<dyn Fn() + Send + Sync>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Usage {
    Poll,
    Fresh,
}

/// Everything outside the monitor: the clock, the logins' stored sign-ins, Anthropic's endpoints, `claude`, disk.
#[async_trait]
pub trait Deps: Send + Sync + 'static {
    fn now(&self) -> Ms;
    async fn sleep(&self, ms: Ms);
    async fn read_stored(&self, slot: &str) -> Stored;
    async fn fetch_usage(&self, access_token: &str) -> UsageAnswer;
    /// The profile endpoint's body for a token, or None when the call failed.
    async fn fetch_profile(&self, access_token: &str) -> Option<Value>;
    /// Who a slot is signed in as, from `claude auth status --json`.
    async fn auth_status(&self, slot: &str) -> Option<Identity>;
    /// Has Claude Code refresh a slot's expired access token (`claude -p /usage` as that login); whether it worked.
    async fn refresh_token(&self, slot: &str) -> bool;
    fn sign_in(&self, slot: &str, email: Option<&str>) -> SignInRun;
    async fn sign_out(&self, slot: &str);
    /// Makes a new login's directory and answers its path.
    async fn make_slot(&self, id: &str) -> Result<String, String>;
    fn is_alive(&self, pid: i64) -> bool;
    fn save(&self, persisted: &Persisted);
    fn log(&self, text: &str);
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub rotate_to: String,
    pub footer: FooterStyle,
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chrome {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default)]
    pub account_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub miss: Option<String>,
}

/// What the monitor keeps on disk (`state.json`, the TypeScript coordinator's format).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Persisted {
    pub accounts: Vec<Account>,
    pub readings: BTreeMap<String, Reading>,
    pub health: BTreeMap<String, Health>,
    pub holders: BTreeMap<String, Holder>,
    pub settings: Settings,
    pub chrome: Chrome,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    pub version: String,
    pub pid: u32,
    pub started_at: Ms,
    pub is_managed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: String,
    pub account_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub is_interactive: bool,
    pub is_blocked: bool,
    pub is_pending: bool,
    pub since: Ms,
    pub last_seen: Ms,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notice {
    pub id: u64,
    pub at: Ms,
    pub text: String,
    pub notify: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountState {
    #[serde(flatten)]
    pub account: Account,
    pub health: Health,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reading: Option<Reading>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<Holder>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Ms>,
    pub weight: u32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_retiring: bool,
    pub session_count: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignInState {
    pub account_id: String,
    pub since: Ms,
}

/// Everything the monitor shares: what GET /state answers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedState {
    pub version: u64,
    pub coordinator: Info,
    pub now: Ms,
    pub accounts: Vec<AccountState>,
    pub combined: Combined,
    pub settings: Settings,
    pub chrome: Chrome,
    pub sessions: Vec<SessionInfo>,
    pub notices: Vec<Notice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sign_in: Option<SignInState>,
}

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub body: Option<Value>,
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: Value,
    pub shutdown: bool,
}

/// A request refused: its status and the sentence the client shows.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpError {
    pub status: u16,
    pub message: String,
}

fn err(status: u16, message: impl Into<String>) -> HttpError {
    HttpError { status, message: message.into() }
}

struct Signing {
    account_id: String,
    since: Ms,
    cancel: Arc<dyn Fn() + Send + Sync>,
}

#[derive(Default)]
struct St {
    accounts: Vec<Account>,
    readings: BTreeMap<String, Reading>,
    health: BTreeMap<String, Health>,
    holders: BTreeMap<String, Holder>,
    expires_at: BTreeMap<String, Ms>,
    refresh_tried_at: HashMap<String, Ms>,
    /// In the order they were asked for.
    retiring: Vec<String>,
    settings: Option<Settings>,
    chrome: Chrome,
    /// In the order they registered.
    sessions: Vec<SessionInfo>,
    open_polls: HashMap<String, u32>,
    /// Each session's last long poll: the `since` it sent, when that was behind the state. A session that sends the
    /// same stale `since` twice isn't taking the version from the answers (an older hook), and would spin.
    stale_polls: HashMap<String, (u64, bool)>,
    notices: Vec<Notice>,
    next_notice_id: u64,
    claims: HashMap<u64, String>,
    signing: Option<Signing>,
    sign_queue: Vec<String>,
    version: u64,
    usage_at: HashMap<String, Ms>,
    backoff: HashMap<String, (Ms, Ms)>,
    unreadable: HashSet<String>,
    /// Where each login's newest reading came from: a poll, or a session.
    sources: HashMap<String, ReadingSource>,
    is_ticking: bool,
    is_stopped: bool,
    /// Borrow mode: the accounts the hub shares now (by ID). An account that left keeps its sessions until they move
    /// off, then goes.
    shared_ids: Option<BTreeSet<String>>,
}

impl St {
    fn find(&self, id: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.id == id)
    }
    fn session_count(&self, account_id: &str) -> usize {
        self.sessions.iter().filter(|s| s.account_id == account_id).count()
    }
    fn health_kind(&self, id: &str) -> Option<&'static str> {
        self.health.get(id).map(Health::kind)
    }
    fn settings(&self) -> Settings {
        self.settings.clone().unwrap_or(Settings { rotate_to: DEFAULT_ROTATE_TO.into(), footer: DEFAULT_FOOTER })
    }
}

struct Inner<D: Deps> {
    deps: D,
    info: Info,
    /// Borrow mode: the slots hold setup tokens the hub sent (`claude-auth-manager`). They can't read usage or profiles
    /// (403), so nothing is polled, refreshed or signed in here: accounts and readings come from the hub, and from what
    /// sessions report.
    is_borrowed: bool,
    st: Mutex<St>,
    /// Bumps on every change, and once more as the monitor stops: what long polls wait on.
    wake: watch::Sender<u64>,
    inflight: Mutex<HashMap<String, Shared<BoxFuture<'static, ()>>>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

/// The monitor. Cheap to clone: every clone is the same one.
pub struct Coordinator<D: Deps> {
    inner: Arc<Inner<D>>,
}

impl<D: Deps> Clone for Coordinator<D> {
    fn clone(&self) -> Self {
        Coordinator { inner: self.inner.clone() }
    }
}

fn obj(v: &Option<Value>) -> serde_json::Map<String, Value> {
    v.as_ref().and_then(Value::as_object).cloned().unwrap_or_default()
}

/// JavaScript's `Number(s)` for the strings a query holds: blank is 0, anything else not a number is NaN.
fn js_number(s: &str) -> f64 {
    let t = s.trim();
    if t.is_empty() { 0.0 } else { t.parse::<f64>().unwrap_or(f64::NAN) }
}

/// `decodeURIComponent`: None for a malformed escape or bytes that aren't UTF-8.
fn decode_component(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

impl<D: Deps> Coordinator<D> {
    /// A monitor that reads its own sign-ins and polls them: the hub's, once it's built (the node runs `borrowing`).
    #[cfg_attr(not(test), expect(dead_code, reason = "poll mode is for the hub, not built yet"))]
    pub fn new(deps: D, info: Info, persisted: Option<&Value>) -> Self {
        Self::with_mode(deps, info, persisted, false)
    }

    /// A monitor in borrow mode (see `Inner::is_borrowed`).
    pub fn borrowing(deps: D, info: Info, persisted: Option<&Value>) -> Self {
        Self::with_mode(deps, info, persisted, true)
    }

    pub fn is_borrowed(&self) -> bool {
        self.inner.is_borrowed
    }

    fn with_mode(deps: D, info: Info, persisted: Option<&Value>, is_borrowed: bool) -> Self {
        // The version starts at the clock, in ms, so it's higher than any version an earlier run reached: sessions keep
        // the highest version they've seen across a restart, and polled with it forever when a fresh run counted from 1
        // (25 Nov 2026: all ~50 sessions on a Mac stuck at 2567 while the node was at 80, every poll answered at once).
        let start = u64::try_from(deps.now()).unwrap_or(0).max(1);
        let mut st = St { version: start, next_notice_id: 1, ..Default::default() };
        if let Some(p) = persisted {
            load(&mut st, p);
        }
        if !st.accounts.iter().any(|a| a.slot.is_empty()) {
            st.accounts.retain(|a| a.id != "1");
            st.accounts.insert(0, Account { id: "1".into(), slot: String::new(), ..Default::default() });
        }
        for h in st.health.values_mut() {
            // A sign-in a monitor before this one ran never finished.
            if matches!(h, Health::LoggingIn { .. }) {
                *h = Health::login_needed("sign-in not finished");
            }
        }
        let (wake, _) = watch::channel(0);
        Coordinator {
            inner: Arc::new(Inner {
                deps,
                info,
                is_borrowed,
                st: Mutex::new(st),
                wake,
                inflight: Mutex::new(HashMap::new()),
                tasks: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn deps(&self) -> &D {
        &self.inner.deps
    }

    fn st(&self) -> MutexGuard<'_, St> {
        self.inner.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn spawn(&self, fut: impl std::future::Future<Output = ()> + Send + 'static) {
        let handle = tokio::spawn(fut);
        let mut tasks = self.inner.tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.retain(|t| !t.is_finished());
        tasks.push(handle);
    }

    /// Starts the periodic checks and reads every login once.
    pub async fn start(&self) {
        let me = self.clone();
        self.spawn(async move {
            loop {
                me.deps().sleep(TICK_MS).await;
                me.tick().await;
            }
        });
        {
            let mut st = self.st();
            self.changed(&mut st);
        }
        if !self.is_borrowed() {
            self.boot().await;
        }
    }

    pub fn stop(&self) {
        let cancel = {
            let mut st = self.st();
            st.is_stopped = true;
            st.signing.as_ref().map(|s| s.cancel.clone())
        };
        for t in self.inner.tasks.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
            t.abort();
        }
        if let Some(cancel) = cancel {
            cancel();
        }
        self.inner.wake.send_modify(|v| *v += 1);
    }

    async fn boot(&self) {
        self.check_all(Usage::Poll).await;
        // Logins whose token has run out are named from what `claude auth login` left in their folder.
        let unnamed: Vec<Account> =
            self.st().accounts.iter().filter(|a| a.email.is_none() || a.org_id.is_none()).cloned().collect();
        for account in unnamed {
            if let Some(who) = self.deps().auth_status(&account.slot).await {
                let mut st = self.st();
                self.update_account(&mut st, &account.id, &who);
            }
        }
    }

    // ------------------------------------------------------------ state

    fn changed(&self, st: &mut St) {
        st.version += 1;
        self.persist(st);
        self.inner.wake.send_modify(|v| *v += 1);
    }

    fn persist(&self, st: &St) {
        let health = st
            .health
            .iter()
            .filter(|(_, h)| !matches!(h, Health::LoggingIn { .. }))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        self.deps().save(&Persisted {
            accounts: st.accounts.clone(),
            readings: st.readings.clone(),
            health,
            holders: st.holders.clone(),
            settings: st.settings(),
            chrome: st.chrome.clone(),
        });
    }

    pub fn state(&self) -> SharedState {
        let now = self.deps().now();
        let st = self.st();
        let accounts = st
            .accounts
            .iter()
            .map(|a| AccountState {
                account: a.clone(),
                health: st.health.get(&a.id).cloned().unwrap_or_else(|| Health::login_needed("not read yet")),
                reading: st.readings.get(&a.id).cloned(),
                holder: st.holders.get(&a.id).cloned(),
                expires_at: st.expires_at.get(&a.id).copied(),
                weight: weight_of(a.tier.as_deref()),
                is_retiring: st.retiring.contains(&a.id),
                session_count: st.session_count(&a.id),
            })
            .collect();
        SharedState {
            version: st.version,
            coordinator: self.inner.info.clone(),
            now,
            accounts,
            combined: combined_usage(&st.accounts, &st.readings, &st.health, now),
            settings: st.settings(),
            chrome: st.chrome.clone(),
            sessions: st.sessions.clone(),
            notices: st.notices.clone(),
            sign_in: st.signing.as_ref().map(|s| SignInState { account_id: s.account_id.clone(), since: s.since }),
        }
    }

    /// A receiver that sees every change (and the stop).
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.inner.wake.subscribe()
    }

    /// Resolves once the state moves past `since`, or after `ms`. A session that polls again with the same stale
    /// `since` it sent last time didn't take the version from the answer it got, so it waits as if up to date:
    /// otherwise every poll returns at once and it spins (25 Nov 2026: ~50 sessions on a Mac kept a node at 80% CPU).
    pub async fn wait_for_change(&self, since: f64, ms: f64, session: Option<&str>) {
        let mut rx = self.inner.wake.subscribe();
        {
            let mut st = self.st();
            if st.is_stopped {
                return;
            }
            if since != st.version as f64 {
                let Some(session) = session else { return };
                let stale = if since < 0.0 { u64::MAX } else { since as u64 };
                let version = st.version;
                let logged = match st.stale_polls.get(session) {
                    Some(&(last, logged)) if last == stale => logged,
                    _ => {
                        st.stale_polls.insert(session.to_string(), (stale, false));
                        return;
                    }
                };
                if !logged {
                    st.stale_polls.insert(session.to_string(), (stale, true));
                    self.deps().log(&format!(
                        "session {session} keeps polling since version {stale} (now {version}): waiting as if up to date"
                    ));
                }
            } else if let Some(session) = session {
                st.stale_polls.remove(session);
            }
        }
        let wait = if ms.is_finite() { (ms.max(0.0) as Ms).min(MAX_WAIT_MS) } else { 0 };
        tokio::select! {
            _ = rx.changed() => {}
            _ = self.deps().sleep(wait) => {}
        }
    }

    fn need(st: &St, id: &str) -> Result<Account, HttpError> {
        st.find(id).cloned().ok_or_else(|| err(404, format!("No Claude account {id}.")))
    }

    fn update_account(&self, st: &mut St, id: &str, change: &Identity) {
        let Some(before) = st.find(id).cloned() else {
            return;
        };
        let after = before.with(change);
        if after == before {
            return;
        }
        for a in st.accounts.iter_mut().filter(|a| a.id == id) {
            *a = after.clone();
        }
        self.changed(st);
    }

    /// Stores a login's health; a login that could serve and can't now is announced.
    fn set_health(&self, st: &mut St, id: &str, health: Health) {
        let before = st.health.get(id).cloned();
        if before.as_ref() == Some(&health) {
            return;
        }
        st.health.insert(id.to_string(), health.clone());
        if let (Health::LoginNeeded { why }, Some(Health::Ok { .. })) = (&health, &before) {
            let name = label(st.find(id));
            let text = if self.is_borrowed() {
                format!("Claude account {name} stopped working here ({why}). Its setup token comes from the auth hub.")
            } else {
                format!("Claude account {name} needs a sign-in ({why}). Click ⚠{id} under the prompt.")
            };
            self.add_notice(st, text, true, Some(id));
        }
        self.changed(st);
    }

    /// A newer reading of a login; one that shows the same as the last moves its time on without waking anyone.
    /// A response's reading has no model weeks (only the usage endpoint reports them), so it keeps the last poll's.
    fn put_reading(&self, st: &mut St, id: &str, given: Reading, source: ReadingSource) {
        let before = st.readings.get(id).cloned();
        if before.as_ref().is_some_and(|b| b.at > given.at) {
            return;
        }
        st.sources.insert(id.to_string(), source);
        let mut reading = given;
        if reading.models.is_none()
            && let Some(models) = before.as_ref().and_then(|b| b.models.clone())
        {
            reading.models = Some(models);
        }
        let same = same_reading(before.as_ref(), &reading);
        st.readings.insert(id.to_string(), reading);
        if same {
            self.persist(st);
        } else {
            self.changed(st);
        }
    }

    fn add_notice(&self, st: &mut St, text: String, notify: bool, account_id: Option<&str>) {
        let notice = Notice {
            id: st.next_notice_id,
            at: self.deps().now(),
            text: text.clone(),
            notify,
            account_id: account_id.map(str::to_string),
        };
        st.next_notice_id += 1;
        st.notices.push(notice);
        let drop = st.notices.len().saturating_sub(NOTICE_KEEP);
        st.notices.drain(..drop);
        let kept: HashSet<u64> = st.notices.iter().map(|n| n.id).collect();
        st.claims.retain(|id, _| kept.contains(id));
        self.deps().log(&format!("notice: {text}"));
        self.changed(st);
    }

    // ------------------------------------------------------------ checks

    async fn tick(&self) {
        {
            let mut st = self.st();
            if st.is_ticking {
                return;
            }
            st.is_ticking = true;
        }
        self.prune();
        if self.is_borrowed() {
            self.drop_unshared();
        } else {
            self.finish_retirements().await;
            self.check_all(Usage::Poll).await;
        }
        self.st().is_ticking = false;
    }

    pub async fn check_all(&self, usage: Usage) {
        let ids: Vec<String> = self.st().accounts.iter().map(|a| a.id.clone()).collect();
        for id in ids {
            self.check(&id, usage).await;
        }
    }

    /// Re-reads one login, one check at a time per login: a fresh check waiting on a running one goes again after it.
    pub async fn check(&self, id: &str, usage: Usage) {
        if self.is_borrowed() {
            // Nothing to read here: the hub's readings and the sessions' own keep the state current.
            return;
        }
        let running = self.inner.inflight.lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned();
        if let Some(running) = running {
            running.await;
            if usage == Usage::Poll || !self.wants_usage(&self.st(), id, Usage::Fresh, self.deps().now()) {
                return;
            }
        }
        let me = self.clone();
        let key = id.to_string();
        // Run as its own task, so a client that hangs up doesn't leave the check half done.
        let handle = tokio::spawn(async move {
            me.run_check(&key, usage).await;
            me.inner.inflight.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
        });
        let run = async move {
            let _ = handle.await;
        }
        .boxed()
        .shared();
        self.inner.inflight.lock().unwrap_or_else(|e| e.into_inner()).insert(id.to_string(), run.clone());
        run.await;
    }

    /// Whether to have Claude Code refresh a login's expired token: only while no session is on it, and a failed
    /// try waits REFRESH_RETRY_MS.
    fn wants_refresh(&self, st: &St, id: &str, credential: &Credential, usage: Usage, now: Ms) -> bool {
        credential.expires_at <= now + 60_000
            && st.session_count(id) == 0
            && self.wants_usage(st, id, usage, now)
            && st.refresh_tried_at.get(id).is_none_or(|&at| now - at >= REFRESH_RETRY_MS)
    }

    fn wants_usage(&self, st: &St, id: &str, usage: Usage, now: Ms) -> bool {
        let most = if usage == Usage::Fresh { FRESH_MS } else { POLL_MS };
        let read_at = st.readings.get(id).map_or(0, |r| r.at);
        let asked_at = st.usage_at.get(id).copied().unwrap_or(0);
        if now < st.backoff.get(id).map_or(0, |b| b.0) {
            return false;
        }
        now - read_at >= most && now - asked_at >= most
    }

    /// Re-reads one login's stored sign-in: who it belongs to and, while its token is live, its usage when the last
    /// reading is old enough. One that is another login's on the list leaves this login needing its own.
    async fn run_check(&self, id: &str, usage: Usage) {
        let account = {
            let st = self.st();
            match st.find(id) {
                Some(a) if st.health_kind(id) != Some("logging-in") => a.clone(),
                _ => return,
            }
        };
        let now = self.deps().now();
        let stored = self.deps().read_stored(&account.slot).await;
        let mut credential = {
            let mut st = self.st();
            if st.find(id).is_none() {
                return;
            }
            if let Stored::Unreadable(code) = stored {
                if st.unreadable.insert(id.to_string()) {
                    self.add_notice(&mut st, format!("Could not read Claude account {id}'s sign-in from the keychain (exit {code}); is it locked?"), false, Some(id));
                }
                return;
            }
            st.unreadable.remove(id);
            match stored {
                Stored::Present(c) => c,
                _ => None,
            }
        };
        let refresh = {
            let mut st = self.st();
            let wants = credential.as_ref().is_some_and(|c| {
                matches!(health_of(Some(c), now), Health::Ok { .. }) && self.wants_refresh(&st, id, c, usage, now)
            });
            if wants {
                st.refresh_tried_at.insert(id.to_string(), now);
            }
            wants
        };
        if refresh {
            if !self.deps().refresh_token(&account.slot).await {
                self.deps().log(&format!("account {id}: Claude Code could not refresh its expired sign-in"));
            }
            let again = self.deps().read_stored(&account.slot).await;
            if self.st().find(id).is_none() {
                return;
            }
            if let Stored::Present(Some(c)) = again {
                credential = Some(c);
            }
        }
        {
            let mut st = self.st();
            self.set_expiry(&mut st, id, credential.as_ref().map(|c| c.expires_at));
        }
        let mut health = health_of(credential.as_ref(), now);
        if let (Health::Ok { .. }, Some(credential)) = (&health, &credential) {
            let is_live = credential.expires_at > now + 60_000;
            let holder = self.holder_of(&account, credential, is_live).await;
            let twin = holder.and_then(|h| {
                let mut st = self.st();
                self.follow_sign_in(&mut st, id, &h.who)
            });
            if let Some(twin) = twin {
                health = Health::login_needed(format!("signed in as account {} now", twin.id));
            } else if is_live && {
                let mut st = self.st();
                let wants = self.wants_usage(&st, id, usage, now);
                if wants {
                    st.usage_at.insert(id.to_string(), now);
                }
                wants
            } {
                let answer = self.deps().fetch_usage(&credential.access_token).await;
                let mut st = self.st();
                match &answer {
                    UsageAnswer::Rejected => {
                        health = Health::login_needed("sign-in rejected");
                        st.backoff.remove(id);
                    }
                    UsageAnswer::Failed { status, retry_after_ms } => {
                        let doubled = (st.backoff.get(id).map_or(BACKOFF_MS / 2, |b| b.1) * 2).min(MAX_BACKOFF_MS);
                        let delay = retry_after_ms.unwrap_or(0).max(doubled);
                        st.backoff.insert(id.to_string(), (now + delay, delay));
                        self.deps().log(&format!(
                            "usage for account {id}: HTTP {status}; next try in {} s",
                            (delay as f64 / 1000.0).round() as i64
                        ));
                    }
                    UsageAnswer::Ok(body) => {
                        st.backoff.remove(id);
                        if let Some(reading) = reading_from_usage(body, self.deps().now())
                            && st.find(id).is_some()
                        {
                            self.put_reading(&mut st, id, reading, ReadingSource::Poll);
                        }
                    }
                }
            }
        }
        let mut st = self.st();
        if st.find(id).is_none() || st.health_kind(id) == Some("logging-in") {
            return;
        }
        self.set_health(&mut st, id, health);
    }

    fn set_expiry(&self, st: &mut St, id: &str, expires_at: Option<Ms>) {
        if st.expires_at.get(id).copied() == expires_at {
            return;
        }
        match expires_at {
            None => st.expires_at.remove(id),
            Some(at) => st.expires_at.insert(id.to_string(), at),
        };
        self.changed(st);
    }

    /// Who a login's sign-in belongs to: as answered for its token before, else asked while the token is live.
    async fn holder_of(&self, account: &Account, credential: &Credential, is_live: bool) -> Option<Holder> {
        let token = fingerprint(credential.refresh_token.as_deref().unwrap_or(&credential.access_token));
        // One answered before the profile's account UUID was kept is asked again while the token is live.
        if let Some(known) = self
            .st()
            .holders
            .get(&account.id)
            .filter(|h| h.token == token && (h.who.account_uuid.is_some() || !is_live))
        {
            return Some(known.clone());
        }
        if !is_live {
            return None;
        }
        let profile = self.deps().fetch_profile(&credential.access_token).await;
        let who = identity_from_profile(profile.as_ref())?;
        let mut st = self.st();
        if who.email.is_none() || st.find(&account.id).is_none() {
            return None;
        }
        let holder = Holder { token, who: Identity { plan: credential.plan.clone().or(who.plan.clone()), ..who } };
        st.holders.insert(account.id.clone(), holder.clone());
        self.changed(&mut st);
        Some(holder)
    }

    /// A login's sign-in belongs to `who`. Another login on the list: this one holds that one's sign-in, answered so
    /// it asks for its own again. Else the login is who signed in, told when that is another account than before.
    fn follow_sign_in(&self, st: &mut St, id: &str, who: &Identity) -> Option<Account> {
        let own = st.find(id)?.clone();
        let is_same = own.email.is_none() || is_login_of(&own.identity(), who);
        if !is_same && let Some(twin) = st.accounts.iter().find(|a| a.id != own.id && is_login_of(&a.identity(), who)) {
            return Some(twin.clone());
        }
        let followed = own.with(who);
        self.update_account(st, id, who);
        if !is_same {
            let name = match &followed.org_name {
                Some(org) if !org.is_empty() => {
                    format!("{} · {org}", followed.email.clone().unwrap_or_default())
                }
                _ => followed.email.clone().unwrap_or_default(),
            };
            let text = if own.slot.is_empty() {
                format!("Claude Code's login, account {}, is {name} now: new sessions start on it.", own.id)
            } else {
                format!(
                    "Claude account {} is {name} now: a /login in a session on account {} signs in there, not Claude Code's login.",
                    own.id, own.id
                )
            };
            self.add_notice(st, text, false, Some(id));
        }
        None
    }

    /// A turn hit the usage limit on a login: read it now, and where the usage doesn't show it yet, count the window
    /// that limits it full; a session on a model with its own week (`scope`) counts that week.
    pub async fn spent(&self, id: &str, scope: Option<&str>) -> Result<(), HttpError> {
        Self::need(&self.st(), id)?;
        self.check(id, Usage::Fresh).await;
        let now = self.deps().now();
        let mut st = self.st();
        let r = st.readings.get(id).cloned();
        if matches!(st.health.get(id), Some(Health::LoginNeeded { .. })) || is_spent(r.as_ref(), now, scope) {
            return Ok(());
        }
        self.put_reading(&mut st, id, spent_reading(r.as_ref(), now, scope), ReadingSource::Session);
        Ok(())
    }

    /// A turn's sign-in was rejected on a login.
    pub fn rejected(&self, id: &str) -> Result<(), HttpError> {
        let mut st = self.st();
        Self::need(&st, id)?;
        self.set_health(&mut st, id, Health::login_needed(REJECTED));
        Ok(())
    }

    /// A response's own usage, as a session on the login saw it.
    pub fn report(&self, id: &str, reading: Reading) -> Result<(), HttpError> {
        let mut st = self.st();
        Self::need(&st, id)?;
        if st.health_kind(id) == Some("logging-in") {
            return Ok(());
        }
        if self.is_borrowed() && st.health_kind(id) != Some("ok") {
            // A response went through on the account's token, so it works again (the hub sent a new one).
            self.set_health(&mut st, id, Health::Ok { refresh_expires_at: None });
        }
        self.put_reading(&mut st, id, reading, ReadingSource::Session);
        Ok(())
    }

    // ------------------------------------------------------------ sessions

    pub fn register(&self, id: &str, pid: Option<i64>, slot: &str, cwd: Option<&str>, is_interactive: bool) -> String {
        let now = self.deps().now();
        let mut st = self.st();
        let mut adopted = false;
        let account_id = match st.accounts.iter().find(|a| a.slot == slot) {
            Some(a) => a.id.clone(),
            None => {
                adopted = true;
                // Started on a slot nobody registered (set by hand, or by another tool): adopt it.
                let account = Account { id: free_id(&st.accounts), slot: slot.to_string(), ..Default::default() };
                let new_id = account.id.clone();
                st.accounts.push(account);
                let me = self.clone();
                let key = new_id.clone();
                self.spawn(async move { me.check(&key, Usage::Fresh).await });
                new_id
            }
        };
        let before = st.sessions.iter().position(|s| s.id == id);
        let info = SessionInfo {
            id: id.to_string(),
            account_id: account_id.clone(),
            pid,
            cwd: cwd.map(str::to_string),
            is_interactive,
            is_blocked: before.is_some_and(|i| st.sessions[i].is_blocked),
            is_pending: false,
            since: before.map_or(now, |i| st.sessions[i].since),
            last_seen: now,
        };
        // A session registers again and again (each poll it restarts, each hook): only a real change wakes the long
        // polls, or ~50 sessions on a Mac re-read the whole state on every registration (25 Nov 2026: half a core).
        let differs = adopted
            || before.is_none_or(|i| {
                SessionInfo { last_seen: 0, ..st.sessions[i].clone() } != SessionInfo { last_seen: 0, ..info.clone() }
            });
        match before {
            Some(i) => st.sessions[i] = info,
            None => st.sessions.push(info),
        }
        if differs {
            self.changed(&mut st);
        }
        account_id
    }

    /// `why` is the session's reason for a move to another account, or for its rotation stopping, which the log keeps.
    pub fn patch_session(
        &self,
        id: &str,
        account_id: Option<&str>,
        is_blocked: Option<bool>,
        is_pending: Option<bool>,
        why: Option<&str>,
    ) -> Result<(), HttpError> {
        let now = self.deps().now();
        let mut st = self.st();
        let Some(i) = st.sessions.iter().position(|s| s.id == id) else {
            return Err(err(404, format!("No session {id}: register it again.")));
        };
        if let Some(a) = account_id {
            Self::need(&st, a)?;
        }
        let before = st.sessions[i].clone();
        let is_move = account_id.is_some_and(|a| a != before.account_id);
        if is_move || why.is_some() {
            let place = match (before.pid, &before.cwd) {
                (Some(pid), Some(cwd)) => format!("pid {pid} in {cwd}"),
                (Some(pid), None) => format!("pid {pid}"),
                _ => id.to_string(),
            };
            let what = match account_id {
                Some(a) if is_move => format!("account {} -> {a}", before.account_id),
                _ => format!("on account {}", before.account_id),
            };
            let reason = why.map(|w| format!(" ({})", w.chars().take(300).collect::<String>())).unwrap_or_default();
            self.deps().log(&format!("session {place}: {what}{reason}"));
        }
        let mut next = before.clone();
        if let Some(a) = account_id {
            next.account_id = a.to_string();
        }
        if let Some(b) = is_blocked {
            next.is_blocked = b;
        }
        if let Some(p) = is_pending {
            next.is_pending = p;
        }
        next.last_seen = now;
        let differs = SessionInfo { last_seen: 0, ..before } != SessionInfo { last_seen: 0, ..next.clone() };
        st.sessions[i] = next;
        if differs {
            self.changed(&mut st);
            drop(st);
            let me = self.clone();
            self.spawn(async move { me.finish_retirements().await });
        }
        Ok(())
    }

    pub fn drop_session(&self, id: &str) {
        let mut st = self.st();
        let Some(i) = st.sessions.iter().position(|s| s.id == id) else {
            return;
        };
        st.sessions.remove(i);
        self.changed(&mut st);
        drop(st);
        let me = self.clone();
        self.spawn(async move { me.finish_retirements().await });
    }

    /// A request from a session: it lives.
    pub fn seen(&self, id: &str) {
        let now = self.deps().now();
        let mut st = self.st();
        if let Some(s) = st.sessions.iter_mut().find(|s| s.id == id) {
            s.last_seen = now;
        }
    }

    pub fn poll_opened(&self, id: &str) {
        *self.st().open_polls.entry(id.to_string()).or_insert(0) += 1;
        self.seen(id);
    }

    pub fn poll_closed(&self, id: &str) {
        {
            let mut st = self.st();
            let open = st.open_polls.get(id).copied().unwrap_or(1).saturating_sub(1);
            if open == 0 {
                st.open_polls.remove(id);
            } else {
                st.open_polls.insert(id.to_string(), open);
            }
        }
        self.seen(id);
    }

    /// Drops sessions whose process has ended; one whose process can't be checked, once it has gone quiet.
    pub fn prune(&self) {
        let now = self.deps().now();
        let mut st = self.st();
        let before = st.sessions.len();
        let dead: Vec<String> = st
            .sessions
            .iter()
            .filter(|s| {
                let is_quiet =
                    st.open_polls.get(&s.id).copied().unwrap_or(0) == 0 && now - s.last_seen > STALE_SESSION_MS;
                match s.pid {
                    None => is_quiet,
                    Some(pid) => !self.deps().is_alive(pid),
                }
            })
            .map(|s| s.id.clone())
            .collect();
        st.sessions.retain(|s| !dead.contains(&s.id));
        for id in &dead {
            st.open_polls.remove(id);
        }
        if st.sessions.len() != before {
            self.changed(&mut st);
        }
    }

    // ------------------------------------------------------------ borrow mode

    fn refuse_when_borrowed(&self) -> Result<(), HttpError> {
        if self.is_borrowed() {
            return Err(err(
                409,
                "This machine's Claude accounts come from the auth hub: add, remove or sign in to them there.",
            ));
        }
        Ok(())
    }

    /// Borrow mode: the accounts the hub shares now, each with its slot, account UUID and email. Accounts that left
    /// go once no session is on them (`drop_unshared`); Claude Code's own sign-in (account 1) always stays. A shared
    /// account counts as working unless a session saw its token rejected or its token check failed, which a passing
    /// check or a session's reading clears.
    pub fn set_shared_accounts(&self, shared: Vec<Account>) {
        let mut st = self.st();
        let mut changed = false;
        for a in &shared {
            match st.accounts.iter_mut().find(|b| b.id == a.id) {
                Some(b) if b == a => {}
                Some(b) => {
                    *b = a.clone();
                    changed = true;
                }
                None => {
                    st.accounts.push(a.clone());
                    changed = true;
                }
            }
            if !st.health.contains_key(&a.id) {
                st.health.insert(a.id.clone(), Health::Ok { refresh_expires_at: None });
                changed = true;
            }
        }
        st.accounts.sort_by_key(|a| a.id.parse::<u32>().unwrap_or(u32::MAX));
        st.shared_ids = Some(shared.iter().map(|a| a.id.clone()).collect());
        if changed {
            self.changed(&mut st);
        }
        drop(st);
        self.drop_unshared();
    }

    /// Borrow mode: drops accounts the hub no longer shares once no session is on them, and marks those that still
    /// have sessions as not working, so the rotator moves them off.
    fn drop_unshared(&self) {
        let mut st = self.st();
        let Some(shared) = st.shared_ids.clone() else {
            return;
        };
        let gone: Vec<String> = st
            .accounts
            .iter()
            .filter(|a| !a.slot.is_empty() && !shared.contains(&a.id))
            .map(|a| a.id.clone())
            .collect();
        let mut changed = false;
        for id in gone {
            if st.session_count(&id) == 0 {
                st.accounts.retain(|a| a.id != id);
                st.readings.remove(&id);
                st.health.remove(&id);
                st.sources.remove(&id);
                changed = true;
            } else if st.health_kind(&id) == Some("ok") {
                st.health.insert(id.clone(), Health::login_needed("no longer shared by the auth hub"));
                changed = true;
            }
        }
        if changed {
            self.changed(&mut st);
        }
    }

    fn id_of_uuid(st: &St, account_uuid: &str) -> Option<String> {
        st.accounts.iter().find(|a| a.account_uuid.as_deref() == Some(account_uuid)).map(|a| a.id.clone())
    }

    /// Borrow mode: a reading from elsewhere (the hub's poll, or a session on another machine) for the account with
    /// this UUID, kept as `put_reading` keeps any: the newest whole reading wins, and one without model weeks keeps
    /// the last ones. An older reading with model weeks the current one lacks gives it them. False when no account
    /// here has that UUID.
    pub fn borrow_reading(&self, account_uuid: &str, given: Reading, source: ReadingSource) -> bool {
        let mut st = self.st();
        let Some(id) = Self::id_of_uuid(&st, account_uuid) else {
            return false;
        };
        if let Some(b) = st.readings.get(&id).cloned().filter(|b| b.at > given.at) {
            if b.models.is_none() && given.models.is_some() {
                st.readings.insert(id, Reading { models: given.models, ..b });
                self.changed(&mut st);
            }
            return true;
        }
        self.put_reading(&mut st, &id, given, source);
        true
    }

    /// Borrow mode: what the token check found for the account with this UUID.
    pub fn token_checked(&self, account_uuid: &str, ok: bool, why: Option<&str>) {
        let mut st = self.st();
        let Some(id) = Self::id_of_uuid(&st, account_uuid) else {
            return;
        };
        let health = if ok {
            Health::Ok { refresh_expires_at: None }
        } else {
            Health::login_needed(format!("token check failed: {}", why.unwrap_or("no reason given")))
        };
        self.set_health(&mut st, &id, health);
    }

    /// The readings sessions on this machine reported, newer than `after` (by account ID): what borrow mode publishes.
    pub fn session_readings(&self, after: &HashMap<String, Ms>) -> Vec<(Account, Reading)> {
        let st = self.st();
        st.accounts
            .iter()
            .filter(|a| st.sources.get(&a.id) == Some(&ReadingSource::Session))
            .filter_map(|a| {
                let r = st.readings.get(&a.id)?;
                after.get(&a.id).is_none_or(|t| r.at > *t).then(|| (a.clone(), r.clone()))
            })
            .collect()
    }

    // ------------------------------------------------------------ accounts

    /// Adds a login in a new slot under ~/.claude/accounts and signs it in.
    pub async fn add(&self) -> Result<String, HttpError> {
        self.refuse_when_borrowed()?;
        let id = free_id(&self.st().accounts);
        let slot = self.deps().make_slot(&id).await.map_err(|e| err(500, e))?;
        {
            let mut st = self.st();
            st.accounts.push(Account { id: id.clone(), slot, ..Default::default() });
            st.health.insert(id.clone(), Health::login_needed("not signed in yet"));
            self.changed(&mut st);
        }
        self.queue_sign_in(vec![id.clone()]);
        Ok(id)
    }

    /// Removes a login: it is signed out and dropped once no session is on it. Moving its sessions off is the
    /// rotation plugin's work, which refuses a removal no other login can take over from.
    pub async fn remove(&self, id: &str) -> Result<(), HttpError> {
        self.refuse_when_borrowed()?;
        {
            let mut st = self.st();
            let account = Self::need(&st, id)?;
            if account.slot.is_empty() {
                return Err(err(
                    400,
                    "Claude Code's own login can't be removed; sign it in to another account with /login instead.",
                ));
            }
            if !st.retiring.iter().any(|r| r == id) {
                st.retiring.push(id.to_string());
            }
            self.changed(&mut st);
        }
        self.finish_retirements().await;
        Ok(())
    }

    async fn finish_retirements(&self) {
        let ids: Vec<String> = self.st().retiring.clone();
        for id in ids {
            let (account, cancel) = {
                let mut st = self.st();
                if st.session_count(&id) > 0 || !st.retiring.contains(&id) {
                    continue;
                }
                st.retiring.retain(|r| r != &id);
                let Some(account) = st.find(&id).cloned() else {
                    continue;
                };
                let cancel = st.signing.as_ref().filter(|s| s.account_id == id).map(|s| s.cancel.clone());
                st.sign_queue.retain(|x| x != &id);
                (account, cancel)
            };
            if let Some(cancel) = cancel {
                cancel();
            }
            self.deps().sign_out(&account.slot).await;
            let mut st = self.st();
            st.accounts.retain(|a| a.id != id);
            st.readings.remove(&id);
            st.health.remove(&id);
            st.holders.remove(&id);
            st.expires_at.remove(&id);
            st.sources.remove(&id);
            if st.chrome.account_id.as_deref() == Some(&id) {
                st.chrome.account_id = None;
            }
            st.chrome.account_ids.retain(|x| x != &id);
            if st.chrome.miss.as_deref() == Some(&id) {
                st.chrome.miss = None;
            }
            self.add_notice(&mut st, format!("Removed Claude account {}.", label(Some(&account))), false, None);
        }
    }

    // ------------------------------------------------------------ sign-in

    /// Signs one login in, or every login that needs it, one after another.
    pub fn sign_in(&self, id: Option<&str>) -> Result<(), HttpError> {
        self.refuse_when_borrowed()?;
        let ids = {
            let st = self.st();
            match id {
                Some(id) => vec![Self::need(&st, id)?.id],
                None => st
                    .accounts
                    .iter()
                    .filter(|a| matches!(st.health.get(&a.id), Some(Health::LoginNeeded { .. })))
                    .map(|a| a.id.clone())
                    .collect(),
            }
        };
        self.queue_sign_in(ids);
        Ok(())
    }

    pub fn cancel_sign_in(&self) {
        let cancel = {
            let mut st = self.st();
            st.sign_queue.clear();
            st.signing.as_ref().map(|s| s.cancel.clone())
        };
        if let Some(cancel) = cancel {
            cancel();
        }
    }

    fn queue_sign_in(&self, ids: Vec<String>) {
        {
            let mut st = self.st();
            for id in ids {
                let is_signing = st.signing.as_ref().is_some_and(|s| s.account_id == id);
                if !is_signing && !st.sign_queue.contains(&id) {
                    st.sign_queue.push(id);
                }
            }
        }
        self.pump();
    }

    fn pump(&self) {
        let (id, run) = {
            let mut st = self.st();
            if st.signing.is_some() || st.is_stopped {
                return;
            }
            let account = loop {
                if st.sign_queue.is_empty() {
                    return;
                }
                let id = st.sign_queue.remove(0);
                if let Some(a) = st.find(&id) {
                    break a.clone();
                }
            };
            let since = self.deps().now();
            // The child opens the browser itself; its paste-a-code link goes unread.
            let run = self.deps().sign_in(&account.slot, account.email.as_deref());
            st.signing = Some(Signing { account_id: account.id.clone(), since, cancel: run.cancel.clone() });
            st.health.insert(account.id.clone(), Health::LoggingIn { since });
            self.changed(&mut st);
            (account.id, run.done)
        };
        let me = self.clone();
        self.spawn(async move {
            let (code, errors) = run.await;
            let is_listed = {
                let mut st = me.st();
                st.signing = None;
                let is_listed = st.find(&id).is_some();
                if is_listed {
                    st.health.insert(id.clone(), Health::login_needed("sign-in not finished"));
                    me.changed(&mut st);
                    if code != Some(0) {
                        let errors = errors.trim();
                        let why = if errors.is_empty() {
                            ".".to_string()
                        } else {
                            format!(": {}", errors.chars().take(160).collect::<String>())
                        };
                        me.add_notice(
                            &mut st,
                            format!("Sign-in for Claude account {id} did not finish{why}"),
                            false,
                            Some(&id),
                        );
                    }
                }
                is_listed
            };
            if is_listed && code == Some(0) {
                me.adopt_sign_in(&id).await;
            }
            me.pump();
        });
    }

    /// After a browser sign-in: who signed in, and is it a login already on the list?
    async fn adopt_sign_in(&self, id: &str) {
        let Some(account) = self.st().find(id).cloned() else {
            return;
        };
        let who = self.deps().auth_status(&account.slot).await;
        {
            let mut st = self.st();
            if let Some(who) = &who {
                if let Some(twin) = st.accounts.iter().find(|a| a.id != id && same_login(&a.identity(), who)).cloned() {
                    let name = label(Some(&twin));
                    self.set_health(&mut st, id, Health::login_needed(format!("that was account {name} again")));
                    self.add_notice(
                        &mut st,
                        format!("Claude account {id} signed in as account {name}. Sign out of claude.ai in your browser (or pick another organization), then sign in again."),
                        false,
                        Some(id),
                    );
                    return;
                }
                self.update_account(&mut st, id, who);
            }
            self.set_health(&mut st, id, Health::Ok { refresh_expires_at: None });
        }
        self.check(id, Usage::Fresh).await;
    }

    // ------------------------------------------------------------ settings, Chrome, notices

    pub fn patch_settings(&self, patch: &serde_json::Map<String, Value>) -> Result<(), HttpError> {
        let mut st = self.st();
        let mut settings = st.settings();
        if let Some(v) = patch.get("rotateTo") {
            match v.as_str() {
                Some(s) if ROTATE_TOS.contains(&s) => settings.rotate_to = s.to_string(),
                _ => {
                    return Err(err(400, format!("rotateTo is one of {}.", ROTATE_TOS.join(", "))));
                }
            }
        }
        if let Some(v) = patch.get("footer") {
            let Some(given) = v.as_object() else {
                return Err(err(400, "footer is an object."));
            };
            let mut merged = serde_json::to_value(&settings.footer).unwrap_or(json!({}));
            if let Some(m) = merged.as_object_mut() {
                for (k, v) in given {
                    m.insert(k.clone(), v.clone());
                }
            }
            settings.footer = footer_style_from(Some(&merged));
        }
        st.settings = Some(settings);
        self.changed(&mut st);
        Ok(())
    }

    pub fn patch_chrome(&self, patch: &serde_json::Map<String, Value>) -> Result<(), HttpError> {
        let mut st = self.st();
        let mut next = st.chrome.clone();
        match patch.get("accountId") {
            Some(Value::Null) => next.account_id = None,
            Some(Value::String(id)) => next.account_id = Some(Self::need(&st, id)?.id),
            _ => {}
        }
        if let Some(seen) = patch.get("seen").and_then(Value::as_object)
            && let (Some(id), Some(is_seen)) =
                (seen.get("accountId").and_then(Value::as_str), seen.get("isSeen").and_then(Value::as_bool))
        {
            if is_seen {
                let id = Self::need(&st, id)?.id;
                if !next.account_ids.contains(&id) {
                    next.account_ids.push(id);
                }
            } else {
                next.account_ids.retain(|x| x != id);
            }
        }
        match patch.get("miss") {
            Some(Value::Null) => next.miss = None,
            Some(Value::String(m)) => next.miss = Some(m.clone()),
            _ => {}
        }
        if next == st.chrome {
            return Ok(());
        }
        st.chrome = next;
        self.changed(&mut st);
        Ok(())
    }

    pub fn claim(&self, notice_id: u64, session_id: &str) -> bool {
        let mut st = self.st();
        let notify = st.notices.iter().find(|n| n.id == notice_id).is_some_and(|n| n.notify);
        if !notify || st.claims.contains_key(&notice_id) {
            return false;
        }
        st.claims.insert(notice_id, session_id.to_string());
        true
    }

    // ------------------------------------------------------------ the API

    /// Answers one request, as docs/coordinator-api.md says.
    pub async fn handle(&self, req: Request) -> Response {
        match self.route(req).await {
            Ok(r) => r,
            Err(e) => Response { status: e.status, body: json!({ "error": e.message }), shutdown: false },
        }
    }

    fn ok(&self, extra: Value) -> Result<Response, HttpError> {
        let mut body = extra.as_object().cloned().unwrap_or_default();
        body.insert("state".into(), serde_json::to_value(self.state()).unwrap_or(Value::Null));
        Ok(Response { status: 200, body: Value::Object(body), shutdown: false })
    }

    async fn route(&self, req: Request) -> Result<Response, HttpError> {
        let b = obj(&req.body);
        let parts: Vec<String> = req
            .path
            .split('/')
            .filter(|p| !p.is_empty())
            .map(decode_component)
            .collect::<Option<_>>()
            .ok_or_else(|| err(400, format!("{} is not a valid path.", req.path)))?;
        let session_id = b
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| req.query.get("session").cloned());
        if let Some(s) = &session_id {
            self.seen(s);
        }
        let has_id = matches!(parts.first().map(String::as_str), Some("accounts" | "sessions" | "notices"));
        let shape: Vec<&str> =
            parts.iter().enumerate().map(|(i, p)| if i == 1 && has_id { ":id" } else { p.as_str() }).collect();
        let route = format!("{} /{}", req.method, shape.join("/"));
        let id = parts.get(1).cloned().unwrap_or_default();
        let s = |k: &str| b.get(k).and_then(Value::as_str);
        match route.as_str() {
            "GET /hello" => {
                let mut body = serde_json::to_value(&self.inner.info).unwrap_or(json!({}));
                if let Some(o) = body.as_object_mut() {
                    o.insert("name".into(), json!(super::NAME));
                }
                Ok(Response { status: 200, body, shutdown: false })
            }
            "GET /state" => {
                if let Some(since) = req.query.get("since") {
                    let session = req.query.get("session").map(String::as_str);
                    // `since=undefined` from a session counts as a stale version, so repeating it waits too.
                    let since = match js_number(since) {
                        n if n.is_finite() => Some(n),
                        _ if session.is_some() => Some(-1.0),
                        _ => None,
                    };
                    if let Some(since) = since {
                        let wait = js_number(req.query.get("wait").map_or("0", String::as_str));
                        self.wait_for_change(since, if wait.is_nan() { 0.0 } else { wait }, session).await;
                    }
                }
                Ok(Response {
                    status: 200,
                    body: serde_json::to_value(self.state()).unwrap_or(Value::Null),
                    shutdown: false,
                })
            }
            "POST /sessions" => {
                let (Some(sid), Some(slot)) = (s("id").filter(|x| !x.is_empty()), s("slot")) else {
                    return Err(err(400, "A session needs an id and a slot."));
                };
                let pid = b.get("pid").and_then(Value::as_f64).map(|p| p as i64);
                let account_id =
                    self.register(sid, pid, slot, s("cwd"), b.get("isInteractive") == Some(&Value::Bool(true)));
                self.ok(json!({ "accountId": account_id }))
            }
            "PATCH /sessions/:id" => {
                self.patch_session(
                    &id,
                    s("accountId"),
                    b.get("isBlocked").and_then(Value::as_bool),
                    b.get("isPending").and_then(Value::as_bool),
                    s("why"),
                )?;
                self.ok(json!({}))
            }
            "DELETE /sessions/:id" => {
                self.drop_session(&id);
                Ok(Response { status: 200, body: json!({ "ok": true }), shutdown: false })
            }
            "POST /readings" => {
                let reading = b.get("reading").and_then(reading_from_client);
                let (Some(account_id), Some(reading)) = (s("accountId"), reading) else {
                    return Err(err(400, "A reading needs an accountId and a reading."));
                };
                self.report(account_id, reading)?;
                self.ok(json!({}))
            }
            "POST /accounts/:id/spent" => {
                self.spent(&id, s("model").filter(|m| !m.is_empty())).await?;
                self.ok(json!({}))
            }
            "POST /accounts/:id/rejected" => {
                self.rejected(&id)?;
                self.ok(json!({}))
            }
            "POST /accounts/:id/check" => {
                Self::need(&self.st(), &id)?;
                let usage = if b.get("fresh") == Some(&Value::Bool(true)) { Usage::Fresh } else { Usage::Poll };
                self.check(&id, usage).await;
                self.ok(json!({}))
            }
            "POST /check" => {
                self.check_all(Usage::Fresh).await;
                self.ok(json!({}))
            }
            "POST /accounts" => {
                let id = self.add().await?;
                self.ok(json!({ "accountId": id }))
            }
            "POST /accounts/:id/remove" => {
                self.remove(&id).await?;
                self.ok(json!({}))
            }
            "POST /signin" => {
                self.sign_in(s("accountId"))?;
                self.ok(json!({}))
            }
            "POST /signin/cancel" => {
                self.cancel_sign_in();
                self.ok(json!({}))
            }
            "PATCH /settings" => {
                self.patch_settings(&b)?;
                self.ok(json!({}))
            }
            "PATCH /chrome" => {
                self.patch_chrome(&b)?;
                self.ok(json!({}))
            }
            "POST /notices/:id/claim" => {
                let Some(session) = s("sessionId") else {
                    return Err(err(400, "A claim needs a sessionId."));
                };
                let n = js_number(&id);
                let claimed = n.is_finite() && n >= 0.0 && n.fract() == 0.0 && self.claim(n as u64, session);
                Ok(Response { status: 200, body: json!({ "claimed": claimed }), shutdown: false })
            }
            "POST /shutdown" => Ok(Response { status: 200, body: json!({ "ok": true }), shutdown: true }),
            _ => Err(err(404, format!("No {} {}.", req.method, req.path))),
        }
    }
}

/// Reads a state file leniently, as the TypeScript does: what can't be read is left out.
fn load(st: &mut St, p: &Value) {
    let accounts: Vec<Account> = p
        .get("accounts")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|a| a.get("id").is_some_and(Value::is_string) && a.get("slot").is_some_and(Value::is_string))
                .filter_map(|a| serde_json::from_value(a.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    let ids: BTreeSet<String> = accounts.iter().map(|a| a.id.clone()).collect();
    fn keep<T: for<'de> Deserialize<'de>>(v: Option<&Value>, ids: &BTreeSet<String>) -> BTreeMap<String, T> {
        v.and_then(Value::as_object)
            .map(|o| {
                o.iter()
                    .filter(|(k, _)| ids.contains(*k))
                    .filter_map(|(k, v)| serde_json::from_value(v.clone()).ok().map(|v| (k.clone(), v)))
                    .collect()
            })
            .unwrap_or_default()
    }
    st.readings = keep(p.get("readings"), &ids);
    st.health = keep(p.get("health"), &ids);
    st.holders = keep(p.get("holders"), &ids);
    let settings = p.get("settings");
    st.settings = Some(Settings {
        rotate_to: rotate_to_from(settings.and_then(|s| s.get("rotateTo"))),
        footer: footer_style_from(settings.and_then(|s| s.get("footer"))),
    });
    let chrome = p.get("chrome");
    st.chrome = Chrome {
        account_id: chrome
            .and_then(|c| c.get("accountId"))
            .and_then(Value::as_str)
            .filter(|id| ids.contains(*id))
            .map(str::to_string),
        account_ids: chrome
            .and_then(|c| c.get("accountIds"))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).filter(|id| ids.contains(*id)).map(str::to_string).collect())
            .unwrap_or_default(),
        miss: chrome.and_then(|c| c.get("miss")).and_then(Value::as_str).map(str::to_string),
    };
    st.accounts = accounts;
}
