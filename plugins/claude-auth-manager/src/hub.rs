//! The hub: one person's Claude sign-ins, on an always-on machine they control, and the web page where they do every
//! sign-in from their own browser. It never runs on the person's laptop: agents on servers depend on it.
//!
//! For each account it keeps, in its own folder (`dir`, 0700) that no session ever uses:
//! - a normal sign-in (`claude auth login --claudeai` into `<dir>/home/.claude/accounts/<slot>`), for reading usage;
//!   a refresh revokes the old access token, so nothing else may read these;
//! - a setup token (`claude setup-token`: a year long, inference only), which goes to every node, sealed.
//!
//! Signing in headless works (tested with Claude Code 2.1.332): `auth login` prints the sign-in URL and reads the code
//! the person pastes from a pipe; `setup-token` draws a terminal screen, so it runs in a pseudo-terminal, and prints
//! the token on it. The page shows the URL; the person signs in in their browser and pastes the code back.
//!
//! Sign-in URLs, codes and tokens are as sensitive as each other: never logged, never published.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use fleetlayer_protocol::ext;

const SESSION_HOURS: i64 = 12;
const LOCKOUT_FAILURES: usize = 5;
const LOCKOUT: Duration = Duration::from_secs(15 * 60);
/// Sign-ins expire about 30 days after they're made (refreshing doesn't extend them); warn from day 25.
const SIGNIN_LIFETIME_DAYS: i64 = 30;
const SIGNIN_WARN_DAYS: i64 = 25;
/// Setup tokens last a year; warn a month ahead.
const TOKEN_LIFETIME_DAYS: i64 = 365;
const TOKEN_WARN_DAYS_LEFT: i64 = 30;
const FLOW_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubAccount {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub account_uuid: Option<String>,
    #[serde(default)]
    pub signed_in_at: Option<Timestamp>,
    /// When the normal sign-in's refresh token expires, when the credential says.
    #[serde(default)]
    pub sign_in_expires_at: Option<Timestamp>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub token_at: Option<Timestamp>,
    /// The organization's rate-limit tier from the profile (`default_claude_max_20x`), for showing the plan.
    #[serde(default)]
    pub tier: Option<String>,
    /// The account's organization from the profile at sign-in: a setup token must belong to the same one.
    #[serde(default)]
    pub org_id: Option<String>,
    /// The organization the setup token answered for, when the hub could read it.
    #[serde(default)]
    pub token_org_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub accounts: BTreeMap<u32, HubAccount>,
}

pub struct HubConfig {
    pub dir: PathBuf,
    pub claude: PathBuf,
    pub listen: SocketAddr,
    pub login_file: PathBuf,
    pub trusted_proxies: Vec<IpAddr>,
    /// Shown under the login box: where this deployment keeps its login key (never the key or its file's path).
    pub login_hint: Option<String>,
    pub heartbeat: Option<Heartbeat>,
}

/// A heartbeat the hub pushes after each round (Gatus external-endpoint style: `?success=true|false&error=…`, with a
/// bearer token), so a hub that's up but no longer polling or publishing alerts.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Heartbeat {
    pub url: String,
    pub token_file: PathBuf,
}

enum Input {
    Pipe(tokio::process::ChildStdin),
    Pty(Box<dyn Write + Send>),
}

/// Stops a running flow: the flag for loops that check it, the notify for tasks that wait on the program.
#[derive(Default)]
struct Cancel {
    flag: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

impl Cancel {
    fn cancel(&self) {
        self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
        self.notify.notify_one();
    }

    fn is_cancelled(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// What the last poll learned about an account, for the page: its usage and why it needs signing in again.
#[derive(Clone, Default)]
struct Polled {
    usage: Option<crate::coordinator::model::Reading>,
    sign_in_needed: Option<String>,
}

struct Flow {
    kind: &'static str,
    slot: u32,
    started: Instant,
    cancel: Arc<Cancel>,
    /// Claude Code's latest message when the latest code went in: only a newer one is about that code.
    message_at_paste: Option<String>,
    /// The sign-in URL once the program printed it.
    url: Arc<std::sync::Mutex<Option<String>>>,
    /// What it printed, the newest 64 KB (in memory only), for its latest message.
    output: Arc<std::sync::Mutex<String>>,
    input: Option<Input>,
    pasted: bool,
    /// `running`, `done`, or `failed: …`.
    state: Arc<std::sync::Mutex<String>>,
}

impl Flow {
    fn is_running(&self) -> bool {
        *self.state.lock().unwrap() == "running"
    }

    /// Stops it if it's still running: the program is killed and the flow says `cancelled`.
    fn stop(&self) {
        let mut state = self.state.lock().unwrap();
        if *state == "running" {
            *state = "cancelled".into();
            self.cancel.cancel();
        }
    }

    /// The flow for the page. `stage` is where it is: `starting` (no link yet), `link` (waiting for the code),
    /// `checking` (a code went in), `done`, `failed` or `cancelled`. `message` is what Claude Code said about the
    /// latest code: a failure's reason, or, while a sign-in still runs, why it's waiting for the code again (a code
    /// cut short).
    fn json(&self, id: &str) -> Value {
        let state = self.state.lock().unwrap().clone();
        let url = self.url.lock().unwrap().clone();
        let running = state == "running";
        // A failure's reason; else only after a code went in, and only a newer message than the one on screen then
        // (before, the latest line is the welcome text). A setup token's screen redraws as it works, so only its
        // failure says anything.
        let message = if let Some(why) = state.strip_prefix("failed: ") {
            Some(why.to_string())
        } else if !self.pasted || (running && self.kind != "sign_in") {
            None
        } else {
            latest_message(&self.output.lock().unwrap()).filter(|m| Some(m) != self.message_at_paste.as_ref())
        };
        let stage = match state.as_str() {
            "running" if url.is_none() => "starting",
            // A sign-in that said something about the code is waiting for it again.
            "running" if !self.pasted || message.is_some() => "link",
            "running" => "checking",
            "done" => "done",
            "cancelled" => "cancelled",
            _ => "failed",
        };
        json!({"id": id, "kind": self.kind, "slot": self.slot, "url": url, "state": state, "stage": stage,
            "message": message, "seconds": self.started.elapsed().as_secs()})
    }
}

pub struct Hub {
    cfg: HubConfig,
    store: Mutex<Store>,
    flows: Mutex<HashMap<String, Flow>>,
    sessions: Mutex<HashMap<String, Instant>>,
    failures: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
    polled: Mutex<HashMap<u32, Polled>>,
    login: String,
}

fn new_id() -> String {
    let mut b = [0u8; 24];
    getrandom_fill(&mut b);
    hex::encode(b)
}

fn getrandom_fill(buf: &mut [u8]) {
    // /dev/urandom on Linux and macOS.
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(buf);
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(tmp, path)
}

/// Remove terminal escape sequences, for reading what a terminal program printed.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                let mut param = String::new();
                for d in chars.by_ref() {
                    if d.is_ascii_alphabetic() || d == '~' {
                        // A terminal UI moves the cursor right instead of printing spaces.
                        if d == 'C' {
                            let n = param.parse::<usize>().unwrap_or(1).min(200);
                            out.extend(std::iter::repeat_n(' ', n));
                        }
                        break;
                    }
                    param.push(d);
                }
            }
            Some(']') => {
                chars.next();
                while let Some(d) = chars.next() {
                    if d == '\u{7}' {
                        break;
                    }
                    if d == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {
                chars.next();
            }
        }
    }
    out
}

/// The sign-in URL in what a program printed.
pub fn find_url(text: &str) -> Option<String> {
    // In the raw text: a terminal link (OSC 8) carries the URL inside an escape sequence.
    let i = text.find("https://claude.ai/oauth/authorize")?;
    let url: String = text[i..].chars().take_while(|c| !c.is_whitespace() && !c.is_control()).collect();
    Some(url)
}

const OUTPUT_LIMIT: usize = 64 * 1024;

fn append(output: &std::sync::Mutex<String>, text: &str) -> String {
    let mut o = output.lock().unwrap();
    o.push_str(text);
    if o.len() > OUTPUT_LIMIT {
        let mut cut = o.len() - OUTPUT_LIMIT;
        while !o.is_char_boundary(cut) {
            cut += 1;
        }
        o.drain(..cut);
    }
    o.clone()
}

/// Claude Code's latest message in what it printed (a rejected code, an error), without the URL, the prompt or a
/// terminal box's borders.
pub fn latest_message(text: &str) -> Option<String> {
    let clean = strip_ansi(text);
    clean
        .lines()
        .map(|l| {
            let l = l.trim_matches(|c: char| c.is_whitespace() || "│╭╮╰╯─".contains(c));
            l.rsplit_once("Paste code here if prompted >").map_or(l, |(_, rest)| rest).trim().to_string()
        })
        .rfind(|l| {
            // The input box echoes the pasted code masked, with its last characters shown: never a message.
            let masked = l.chars().filter(|c| *c == '*').count() * 2 > l.chars().count();
            !l.is_empty()
                && !masked
                && !l.contains("https://")
                && !l.starts_with("Paste code here")
                && !l.contains("sk-ant-")
        })
}

/// A setup token in the screen `claude setup-token` drew (read through a terminal emulator 200 columns wide, so the
/// token sits whole on one line). It ends at the first character that can't be in a token: never joined to the line
/// under it, which once glued "Store this token securely" onto every token. None until the whole token is on screen.
pub fn find_token(text: &str) -> Option<String> {
    let clean = strip_ansi(text);
    let i = clean.find(TOKEN_PREFIX)?;
    let rest = &clean[i..];
    let tok: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    // Followed by something that isn't part of it (the end of the line), and long enough to be whole.
    let ended = rest.len() > tok.len();
    (ended && plausible_token(&tok)).then_some(tok)
}

const TOKEN_PREFIX: &str = "sk-ant-oat01-";

/// The shape of a setup token: the prefix and 80 to 200 URL-safe characters (108 in all as of Claude Code 2.1.332).
pub fn plausible_token(t: &str) -> bool {
    t.strip_prefix(TOKEN_PREFIX).is_some_and(|rest| {
        (80..=200).contains(&rest.len()) && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    })
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

/// Who a sign-in is, from the profile.
struct Who {
    email: String,
    uuid: Option<String>,
    expires: Option<Timestamp>,
    tier: Option<String>,
    org: Option<String>,
}

const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";

/// The organization a setup token belongs to: the `anthropic-organization-id` header of one tiny request made with it
/// alone (on an error answer too: the header comes with any authenticated answer). None when it couldn't be read.
async fn token_org(token: &str) -> Option<String> {
    let res = reqwest::Client::new()
        .post(MESSAGES_URL)
        .bearer_auth(token)
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("anthropic-version", "2023-06-01")
        .json(&json!({"model": "claude-haiku-4-5", "max_tokens": 1, "messages": [{"role": "user", "content": "hi"}]}))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .ok()?;
    org_header(res.headers())
}

fn org_header(h: &HeaderMap) -> Option<String> {
    h.get("anthropic-organization-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(String::from)
}

/// Why a setup token can't be kept for this account: it answered for another organization than the account's sign-in.
/// None when they match or either isn't known.
pub fn wrong_account(expected: Option<&str>, token: Option<&str>, email: Option<&str>) -> Option<String> {
    let (expected, token) = (expected?, token?);
    (expected != token).then(|| {
        let who = email.map_or("this account".to_string(), |e| e.to_string());
        format!(
            "That token is for a different Claude account. Sign in to claude.ai as {who} in your browser, then make the \
            token again."
        )
    })
}

fn days_since(t: Timestamp) -> i64 {
    Timestamp::now().duration_since(t).as_secs() / 86_400
}

impl Hub {
    pub fn open(cfg: HubConfig) -> anyhow::Result<Arc<Hub>> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(cfg.dir.join("home/.claude/accounts"))?;
        std::fs::set_permissions(&cfg.dir, std::fs::Permissions::from_mode(0o700))?;
        let login = std::fs::read_to_string(&cfg.login_file)
            .map_err(|e| anyhow::anyhow!("reading the login file {}: {e}", cfg.login_file.display()))?
            .trim()
            .to_string();
        anyhow::ensure!(login.len() >= 32, "the hub's login value must be at least 32 characters");
        let store: Store = match std::fs::read(cfg.dir.join("hub.json")) {
            Ok(b) => serde_json::from_slice(&b)?,
            Err(_) => Store::default(),
        };
        if !cfg.dir.join("tokens.json").exists() {
            write_private(&cfg.dir.join("tokens.json"), b"[]")?;
        }
        Ok(Arc::new(Hub {
            cfg,
            store: Mutex::new(store),
            flows: Mutex::default(),
            sessions: Mutex::default(),
            failures: Mutex::default(),
            polled: Mutex::default(),
            login,
        }))
    }

    pub fn tokens_file(&self) -> PathBuf {
        self.cfg.dir.join("tokens.json")
    }

    fn slot_path(&self, slot: u32) -> PathBuf {
        self.cfg.dir.join("home/.claude/accounts").join(slot.to_string())
    }

    /// The environment for a `claude` run against a slot: nothing of the hub's own Claude settings, the hub's home.
    fn env(&self, slot: Option<&Path>) -> Vec<(String, String)> {
        let mut env = crate::coordinator::deps::clean_env();
        env.retain(|(k, _)| k != "HOME");
        env.push(("HOME".into(), self.cfg.dir.join("home").display().to_string()));
        if let Some(s) = slot {
            env.extend(crate::claude::slot_env(&s.display().to_string()));
        }
        env
    }

    async fn save(&self, store: &Store) -> anyhow::Result<()> {
        write_private(&self.cfg.dir.join("hub.json"), &serde_json::to_vec_pretty(store)?)?;
        // The tokens bundle: every account with a setup token and a known account UUID, but never a token found to be
        // another account's (machines would use the wrong account under this one's number).
        let tokens: Vec<Value> = store
            .accounts
            .iter()
            .filter(|(_, a)| wrong_account(a.org_id.as_deref(), a.token_org_id.as_deref(), None).is_none())
            .filter_map(|(slot, a)| {
                let (token, at, uuid, email) = (a.token.as_ref()?, a.token_at?, a.account_uuid.as_ref()?, a.email.as_ref()?);
                let expires = at + SignedDuration::from_hours(24 * TOKEN_LIFETIME_DAYS);
                Some(json!({"slot": slot, "accountUuid": uuid, "email": email, "expiresAt": expires.to_string(), "token": token}))
            })
            .collect();
        write_private(&self.tokens_file(), serde_json::to_string_pretty(&tokens)?.as_bytes())?;
        Ok(())
    }

    /// Who a slot is signed in as, with its account UUID (from the profile endpoint) and the sign-in's expiry.
    async fn identify(&self, slot: u32) -> Option<Who> {
        use crate::coordinator::core::{Deps, Stored};
        let logger = crate::coordinator::deps::Logger::new(self.cfg.dir.join("hub.log"));
        let deps = crate::coordinator::deps::RealDeps::new(
            self.cfg.dir.join("home"),
            self.cfg.claude.clone(),
            self.cfg.dir.join("coordinator-state.json"),
            logger,
        );
        let path = self.slot_path(slot).display().to_string();
        let Stored::Present(Some(cred)) = deps.read_stored(&path).await else { return None };
        let who =
            crate::coordinator::model::identity_from_profile(deps.fetch_profile(&cred.access_token).await.as_ref())?;
        let expires = cred.refresh_token_expires_at.and_then(|ms| Timestamp::from_millisecond(ms).ok());
        Some(Who { email: who.email?, uuid: who.account_uuid, expires, tier: who.tier, org: who.org_id })
    }

    // ------------------------------------------------------------ flows

    /// Before a new flow for an account: stop the one already running for it (two programs signing the same account
    /// in would race), and forget finished flows nobody has looked at for a while.
    async fn begin(&self, slot: u32) {
        let mut flows = self.flows.lock().await;
        for f in flows.values().filter(|f| f.slot == slot) {
            f.stop();
        }
        flows.retain(|_, f| f.is_running() || f.started.elapsed() < FLOW_TIMEOUT + Duration::from_secs(60));
    }

    /// Stops a flow (its program is killed). False when there's no such flow; a finished flow stays as it ended.
    pub async fn cancel_flow(&self, id: &str) -> bool {
        let flows = self.flows.lock().await;
        let Some(f) = flows.get(id) else { return false };
        f.stop();
        true
    }

    async fn start_sign_in(self: &Arc<Self>, slot: u32) -> anyhow::Result<String> {
        self.begin(slot).await;
        let path = self.slot_path(slot);
        std::fs::create_dir_all(&path)?;
        let mut child = tokio::process::Command::new(&self.cfg.claude)
            .args(["auth", "login", "--claudeai"])
            .env_clear()
            .envs(self.env(Some(&path)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow::anyhow!("no stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| anyhow::anyhow!("no stderr"))?;
        let id = new_id();
        let url = Arc::new(std::sync::Mutex::new(None));
        let output = Arc::new(std::sync::Mutex::new(String::new()));
        let state = Arc::new(std::sync::Mutex::new("running".to_string()));
        let cancel = Arc::new(Cancel::default());
        for stream in [Box::new(stdout) as Box<dyn tokio::io::AsyncRead + Send + Unpin>, Box::new(stderr)] {
            let (url, output) = (url.clone(), output.clone());
            tokio::spawn(async move {
                // Not line by line: the prompt and a rejected code's message share a line that ends only later.
                let mut r = BufReader::new(stream);
                let mut buf = vec![0u8; 4096];
                while let Ok(n) = tokio::io::AsyncReadExt::read(&mut r, &mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let o = append(&output, &String::from_utf8_lossy(&buf[..n]));
                    let mut u = url.lock().unwrap();
                    if u.is_none() {
                        *u = find_url(&o);
                    }
                }
            });
        }
        {
            let (hub, state, output, cancel) = (self.clone(), state.clone(), output.clone(), cancel.clone());
            tokio::spawn(async move {
                let result = tokio::select! {
                    r = tokio::time::timeout(FLOW_TIMEOUT, child.wait()) => Some(r),
                    _ = cancel.notify.notified() => None,
                };
                // Cancelled: stop the program; the flow already says so.
                let Some(result) = result else {
                    let _ = child.kill().await;
                    return;
                };
                if cancel.is_cancelled() {
                    return;
                }
                let ok = matches!(result, Ok(Ok(s)) if s.success());
                if !ok {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let why = latest_message(&output.lock().unwrap())
                        .unwrap_or_else(|| "the sign-in didn't finish (wrong or expired code, or it timed out)".into());
                    *state.lock().unwrap() = format!("failed: {why}");
                    return;
                }
                match hub.identify(slot).await {
                    Some(Who { email, uuid, expires, tier, org }) => {
                        let mut store = hub.store.lock().await;
                        let a = store.accounts.entry(slot).or_default();
                        a.email = Some(email.clone());
                        if uuid.is_some() {
                            a.account_uuid = uuid;
                        }
                        a.signed_in_at = Some(Timestamp::now());
                        a.sign_in_expires_at = expires;
                        if tier.is_some() {
                            a.tier = tier;
                        }
                        if org.is_some() {
                            a.org_id = org;
                        }
                        let snapshot = store.clone();
                        drop(store);
                        let _ = hub.save(&snapshot).await;
                        // Whatever the last poll found wrong with the old sign-in, this one replaces it.
                        if let Some(p) = hub.polled.lock().await.get_mut(&slot) {
                            p.sign_in_needed = None;
                        }
                        tracing::info!(slot, email = %email, "signed in");
                        *state.lock().unwrap() = "done".into();
                    }
                    None => *state.lock().unwrap() = "failed: signed in, but the account couldn't be read".into(),
                }
            });
        }
        self.flows.lock().await.insert(
            id.clone(),
            Flow {
                kind: "sign_in",
                slot,
                started: Instant::now(),
                cancel,
                message_at_paste: None,
                url,
                output,
                input: Some(Input::Pipe(stdin)),
                pasted: false,
                state,
            },
        );
        Ok(id)
    }

    async fn start_setup_token(self: &Arc<Self>, slot: u32) -> anyhow::Result<String> {
        self.begin(slot).await;
        use portable_pty::{CommandBuilder, PtySize, native_pty_system};
        let pty = native_pty_system();
        let pair = pty.openpty(PtySize { rows: 50, cols: 200, pixel_width: 0, pixel_height: 0 })?;
        let id = new_id();
        // A throwaway config directory: setup-token signs in only to print the token. Its own per flow: a stopped
        // flow removes its folder after a new one for the same account has started.
        let scratch = self.cfg.dir.join(format!("setup-{slot}-{}", &id[..12]));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch)?;
        let mut cmd = CommandBuilder::new(&self.cfg.claude);
        cmd.arg("setup-token");
        cmd.env_clear();
        for (k, v) in self.env(None) {
            cmd.env(k, v);
        }
        cmd.env("CLAUDE_CONFIG_DIR", &scratch);
        cmd.env("TERM", "xterm");
        let mut child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let url = Arc::new(std::sync::Mutex::new(None));
        let output = Arc::new(std::sync::Mutex::new(String::new()));
        let state = Arc::new(std::sync::Mutex::new("running".to_string()));
        let cancel = Arc::new(Cancel::default());
        {
            let (url, output) = (url.clone(), output.clone());
            std::thread::spawn(move || {
                // The screen as a person would see it (a terminal UI positions text with escape codes), for its
                // messages and the token; the raw bytes for the URL, which a terminal link carries in an escape code.
                let size = |n: u16| std::num::NonZeroU16::new(n).unwrap_or(std::num::NonZeroU16::MIN);
                let mut screen = vt100::Parser::new(size(50), size(200), 0);
                let mut raw = String::new();
                let mut buf = [0u8; 4096];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    screen.process(&buf[..n]);
                    *output.lock().unwrap() = screen.screen().contents();
                    let mut u = url.lock().unwrap();
                    if u.is_none() {
                        raw.push_str(&String::from_utf8_lossy(&buf[..n]));
                        *u = find_url(&raw);
                        if raw.len() > OUTPUT_LIMIT {
                            raw.clear();
                        }
                    }
                }
            });
        }
        {
            let (hub, state, output, cancel) = (self.clone(), state.clone(), output.clone(), cancel.clone());
            let master = pair.master;
            tokio::spawn(async move {
                let started = Instant::now();
                loop {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    if cancel.is_cancelled() {
                        break;
                    }
                    let token = find_token(&output.lock().unwrap());
                    if let Some(token) = token {
                        // Kept and sent only once it works: one small request with the token alone.
                        *state.lock().unwrap() = "running".into();
                        let checked = crate::health::Claude { bin: hub.cfg.claude.display().to_string() }
                            .check_token(&token)
                            .await;
                        if !checked.ok {
                            let why = checked.error.unwrap_or_default();
                            tracing::warn!(slot, "the setup token Claude Code printed doesn't work: {why}");
                            *state.lock().unwrap() =
                                format!("failed: the token Claude Code printed doesn't work ({why}). Start again.");
                            break;
                        }
                        if cancel.is_cancelled() {
                            break;
                        }
                        // Whose token it is: the one the person's browser was signed in to claude.ai as, which may not
                        // be this account. Refused when its organization isn't the account's.
                        let token_org = token_org(&token).await;
                        let (expected, email) = {
                            let store = hub.store.lock().await;
                            let a = store.accounts.get(&slot);
                            (a.and_then(|a| a.org_id.clone()), a.and_then(|a| a.email.clone()))
                        };
                        if let Some(why) = wrong_account(expected.as_deref(), token_org.as_deref(), email.as_deref()) {
                            tracing::warn!(slot, "the setup token is for a different account");
                            *state.lock().unwrap() = format!("failed: {why}");
                            break;
                        }
                        if token_org.is_none() {
                            tracing::warn!(slot, "couldn't read which account the setup token is for");
                        }
                        if cancel.is_cancelled() {
                            break;
                        }
                        let mut store = hub.store.lock().await;
                        let a = store.accounts.entry(slot).or_default();
                        a.token = Some(token);
                        a.token_at = Some(Timestamp::now());
                        a.token_org_id = token_org;
                        let snapshot = store.clone();
                        drop(store);
                        let _ = hub.save(&snapshot).await;
                        tracing::info!(slot, "setup token made");
                        *state.lock().unwrap() = "done".into();
                        break;
                    }
                    let retry = latest_message(&output.lock().unwrap())
                        .is_some_and(|m| m.replace(' ', "").to_lowercase().contains("pressentertoretry"));
                    if retry {
                        *state.lock().unwrap() =
                            "failed: that code wasn't accepted (incomplete, expired or already used). \
                            Start again for a new link."
                                .into();
                        break;
                    }
                    if let Ok(Some(_)) = child.try_wait() {
                        let why = latest_message(&output.lock().unwrap())
                            .unwrap_or_else(|| "setup-token ended without a token (wrong or expired code?)".into());
                        *state.lock().unwrap() = format!("failed: {why}");
                        break;
                    }
                    if started.elapsed() > FLOW_TIMEOUT {
                        let _ = child.kill();
                        *state.lock().unwrap() = "failed: timed out".into();
                        break;
                    }
                }
                let _ = child.kill();
                drop(master);
                let _ = std::fs::remove_dir_all(&scratch);
            });
        }
        self.flows.lock().await.insert(
            id.clone(),
            Flow {
                kind: "setup_token",
                slot,
                started: Instant::now(),
                cancel,
                message_at_paste: None,
                url,
                output,
                input: Some(Input::Pty(writer)),
                pasted: false,
                state,
            },
        );
        Ok(id)
    }

    async fn paste(&self, id: &str, code: &str) -> anyhow::Result<()> {
        let code = code.trim();
        anyhow::ensure!(!code.is_empty() && code.len() < 4096 && !code.contains(['\n', '\r']), "that isn't a code");
        let mut flows = self.flows.lock().await;
        let f = flows.get_mut(id).ok_or_else(|| anyhow::anyhow!("no such sign-in"))?;
        match f.input.take() {
            Some(Input::Pipe(mut stdin)) => {
                stdin.write_all(format!("{code}\n").as_bytes()).await?;
                stdin.flush().await?;
                f.input = Some(Input::Pipe(stdin));
            }
            Some(Input::Pty(mut w)) => {
                // A terminal UI takes a burst of text as a paste and swallows an Enter inside it: send Enter on its
                // own, once the paste has landed.
                w.write_all(code.as_bytes())?;
                w.flush()?;
                tokio::time::sleep(Duration::from_millis(500)).await;
                w.write_all(b"\r")?;
                w.flush()?;
                f.input = Some(Input::Pty(w));
            }
            None => anyhow::bail!("this sign-in isn't waiting for a code"),
        }
        f.message_at_paste = latest_message(&f.output.lock().unwrap());
        f.pasted = true;
        Ok(())
    }

    async fn flow_json(&self, id: &str) -> Option<Value> {
        let flows = self.flows.lock().await;
        let f = flows.get(id)?;
        Some(f.json(id))
    }

    /// What the page shows: accounts (never their tokens), their dates and warnings, what the last poll found (usage,
    /// a sign-in it found no longer works), and the flows still running, so a reloaded page can pick them up.
    async fn state_json(&self) -> Value {
        let store = self.store.lock().await;
        let polled = self.polled.lock().await.clone();
        let accounts: Vec<Value> = store
            .accounts
            .iter()
            .map(|(slot, a)| {
                let signin_days = a.signed_in_at.map(days_since);
                let signin_left = a
                    .sign_in_expires_at
                    .map(|e| e.duration_since(Timestamp::now()).as_secs() / 86_400)
                    .or(signin_days.map(|d| SIGNIN_LIFETIME_DAYS - d));
                let token_left = a.token_at.map(|t| TOKEN_LIFETIME_DAYS - days_since(t));
                let signin_expires = a.sign_in_expires_at.or(a
                    .signed_in_at
                    .map(|t| t + SignedDuration::from_hours(24 * SIGNIN_LIFETIME_DAYS)));
                let token_expires = a.token_at.map(|t| t + SignedDuration::from_hours(24 * TOKEN_LIFETIME_DAYS));
                let p = polled.get(slot).cloned().unwrap_or_default();
                json!({"slot": slot, "email": a.email, "accountUuid": a.account_uuid, "tier": a.tier,
                    "signInExpiresAt": signin_expires, "tokenExpiresAt": token_expires,
                    "signInNeeded": p.sign_in_needed, "usage": p.usage,
                    "tokenWrongAccount": a.token.is_some() && wrong_account(a.org_id.as_deref(), a.token_org_id.as_deref(), None).is_some(),
                    // Not known to match: either organization unknown (never a reason to drop the token).
                    "tokenUnchecked": a.token.is_some() && (a.token_org_id.is_none() || a.org_id.is_none()),
                    "signedInAt": a.signed_in_at, "signInDaysLeft": signin_left,
                    "signInWarn": signin_days.is_some_and(|d| d >= SIGNIN_WARN_DAYS) || signin_left.is_some_and(|l| l <= SIGNIN_LIFETIME_DAYS - SIGNIN_WARN_DAYS),
                    "hasToken": a.token.is_some(), "tokenAt": a.token_at, "tokenDaysLeft": token_left,
                    "tokenWarn": token_left.is_some_and(|l| l <= TOKEN_WARN_DAYS_LEFT)})
            })
            .collect();
        drop(store);
        let flows: Vec<Value> = self
            .flows
            .lock()
            .await
            .iter()
            .filter(|(_, f)| f.is_running())
            .map(|(id, f)| json!({"id": id, "kind": f.kind, "slot": f.slot}))
            .collect();
        json!({"accounts": accounts, "flows": flows})
    }

    /// The readings the hub publishes about its accounts: sign-in and setup-token ages, with warnings.
    #[cfg(test)]
    pub async fn readings(&self) -> Vec<Value> {
        self.readings_with(&HashMap::new()).await
    }

    /// The same, with why the last poll found an account needs signing in again (by account number).
    /// `claude.signin_age` also carries the account's number, the number it has on every machine, and whether it's
    /// signed in on the hub at all, so programs that list accounts (`fleetlayer-claude-accounts`) need nothing else.
    async fn readings_with(&self, sign_in_needed: &HashMap<u32, String>) -> Vec<Value> {
        let st = self.state_json().await;
        let now = Timestamp::now().to_string();
        let mut out = vec![];
        for a in st["accounts"].as_array().into_iter().flatten() {
            let Some(uuid) = a["accountUuid"].as_str() else { continue };
            let number = a["slot"].as_u64().map(|n| n as u32);
            let why = number.and_then(|n| sign_in_needed.get(&n));
            out.push(json!({"kind": "claude.signin_age", "key": uuid, "at": now, "data": {
                "email": a["email"], "daysLeft": a["signInDaysLeft"], "warn": a["signInWarn"],
                "number": number, "signedIn": !a["signedInAt"].is_null(), "signInNeeded": why, "tier": a["tier"]}}));
            if a["hasToken"] == true {
                out.push(json!({"kind": "claude.setup_token_age", "key": uuid, "at": now, "data": {
                    "email": a["email"], "daysLeft": a["tokenDaysLeft"], "warn": a["tokenWarn"]}}));
            }
        }
        out
    }

    /// Removes an account: signs its normal sign-in out (Claude Code revokes it), deletes its folder, and drops it
    /// from the tokens bundle, so every machine clears that slot on the next publish. The setup token itself can only
    /// be revoked on claude.ai. False when there was no such account.
    pub async fn delete(&self, slot: u32) -> anyhow::Result<bool> {
        use crate::coordinator::core::Deps;
        let mut store = self.store.lock().await;
        if store.accounts.remove(&slot).is_none() {
            return Ok(false);
        }
        let snapshot = store.clone();
        drop(store);
        {
            let mut flows = self.flows.lock().await;
            for f in flows.values().filter(|f| f.slot == slot) {
                f.stop();
            }
            flows.retain(|_, f| f.slot != slot);
        }
        self.polled.lock().await.remove(&slot);
        let path = self.slot_path(slot);
        if path.exists() {
            self.deps().sign_out(&path.display().to_string()).await;
            std::fs::remove_dir_all(&path)?;
        }
        self.save(&snapshot).await?;
        tracing::info!(slot, "account removed");
        Ok(true)
    }

    // ------------------------------------------------------------ polling

    fn deps(&self) -> crate::coordinator::deps::RealDeps {
        crate::coordinator::deps::RealDeps::new(
            self.cfg.dir.join("home"),
            self.cfg.claude.clone(),
            self.cfg.dir.join("coordinator-state.json"),
            crate::coordinator::deps::Logger::new(self.cfg.dir.join("hub.log")),
        )
    }

    /// One round: each signed-in account's usage (refreshing an expired access token through Claude Code first), as
    /// `claude.usage` readings with source `poll`, plus the sign-in and setup-token ages. Answers the readings and
    /// what went wrong, if anything did.
    pub async fn poll(&self) -> (Vec<ext::Reading>, Vec<String>) {
        use crate::coordinator::core::{Deps, Stored, UsageAnswer};
        let deps = self.deps();
        let accounts: Vec<(u32, HubAccount)> =
            self.store.lock().await.accounts.iter().map(|(k, v)| (*k, v.clone())).collect();
        let mut readings = vec![];
        let mut problems = vec![];
        let mut sign_in_needed = HashMap::new();
        let mut usage = HashMap::new();
        for (slot, a) in accounts {
            let (Some(uuid), Some(_)) = (a.account_uuid.clone(), a.signed_in_at) else { continue };
            let path = self.slot_path(slot).display().to_string();
            let now = Timestamp::now().as_millisecond();
            let mut cred = match deps.read_stored(&path).await {
                Stored::Present(Some(c)) => c,
                _ => {
                    problems.push(format!("account {slot}: no sign-in stored"));
                    sign_in_needed.insert(slot, "no sign-in stored".to_string());
                    continue;
                }
            };
            if cred.expires_at <= now + 60_000 {
                if !deps.refresh_token(&path).await {
                    problems.push(format!("account {slot}: the sign-in couldn't be refreshed; sign it in again"));
                    sign_in_needed.insert(slot, "the sign-in couldn't be refreshed".to_string());
                    continue;
                }
                match deps.read_stored(&path).await {
                    Stored::Present(Some(c)) => cred = c,
                    _ => continue,
                }
            }
            match deps.fetch_usage(&cred.access_token).await {
                UsageAnswer::Ok(body) => {
                    if let Some(r) = crate::coordinator::model::reading_from_usage(&body, now) {
                        usage.insert(slot, r.clone());
                        let mut data = serde_json::to_value(&r).unwrap_or_default();
                        data["source"] = json!("poll");
                        readings.push(ext::Reading {
                            kind: crate::coordinator::USAGE_KIND.into(),
                            key: uuid,
                            at: Timestamp::now(),
                            node: None,
                            data,
                        });
                    }
                }
                UsageAnswer::Rejected => {
                    problems.push(format!("account {slot}: the usage endpoint refused the sign-in; sign it in again"));
                    sign_in_needed.insert(slot, "the usage endpoint refused the sign-in".to_string());
                }
                UsageAnswer::Failed { status, .. } => {
                    problems.push(format!("account {slot}: the usage endpoint answered {status}"))
                }
            }
        }
        // Accounts signed in before the hub kept their organization: from the stored sign-in's profile, once.
        let unknown: Vec<u32> = self
            .store
            .lock()
            .await
            .accounts
            .iter()
            .filter(|(_, a)| a.org_id.is_none() && a.signed_in_at.is_some())
            .map(|(slot, _)| *slot)
            .collect();
        for slot in unknown {
            let Some(who) = self.identify(slot).await else { continue };
            let mut store = self.store.lock().await;
            let Some(a) = store.accounts.get_mut(&slot) else { continue };
            if a.org_id.is_some() || who.org.is_none() {
                continue;
            }
            a.org_id = who.org;
            if a.tier.is_none() {
                a.tier = who.tier;
            }
            let snapshot = store.clone();
            drop(store);
            let _ = self.save(&snapshot).await;
        }
        // Tokens made before the hub checked whose they are (or whose check couldn't be read): once each.
        let unchecked: Vec<(u32, String)> = self
            .store
            .lock()
            .await
            .accounts
            .iter()
            .filter(|(_, a)| a.token_org_id.is_none())
            .filter_map(|(slot, a)| Some((*slot, a.token.clone()?)))
            .collect();
        for (slot, a) in self.store.lock().await.accounts.iter() {
            if a.token.is_some() && wrong_account(a.org_id.as_deref(), a.token_org_id.as_deref(), None).is_some() {
                problems.push(format!("account {slot}: its setup token is for a different Claude account"));
            }
        }
        for (slot, token) in unchecked {
            let Some(org) = token_org(&token).await else { continue };
            let mut store = self.store.lock().await;
            let Some(a) = store.accounts.get_mut(&slot) else { continue };
            if a.token.as_deref() != Some(token.as_str()) {
                continue;
            }
            a.token_org_id = Some(org);
            let snapshot = store.clone();
            drop(store);
            let _ = self.save(&snapshot).await;
        }
        {
            let mut polled = self.polled.lock().await;
            polled.clear();
            for (slot, r) in usage {
                polled.entry(slot).or_default().usage = Some(r);
            }
            for (slot, why) in &sign_in_needed {
                polled.entry(*slot).or_default().sign_in_needed = Some(why.clone());
            }
        }
        for v in self.readings_with(&sign_in_needed).await {
            if let Ok(r) = serde_json::from_value::<ext::Reading>(v) {
                readings.push(r);
            }
        }
        (readings, problems)
    }

    pub async fn beat(&self, problems: &[String]) {
        let Some(hb) = &self.cfg.heartbeat else { return };
        let Ok(token) = std::fs::read_to_string(&hb.token_file) else {
            tracing::warn!("reading the heartbeat token {}", hb.token_file.display());
            return;
        };
        let mut url =
            format!("{}{}success={}", hb.url, if hb.url.contains('?') { '&' } else { '?' }, problems.is_empty());
        if !problems.is_empty() {
            url.push_str("&error=");
            url.push_str(&form_urlencoded::byte_serialize(problems.join("; ").as_bytes()).collect::<String>());
        }
        let res =
            reqwest::Client::new().post(&url).bearer_auth(token.trim()).timeout(Duration::from_secs(20)).send().await;
        if let Err(e) = res.and_then(|r| r.error_for_status()) {
            tracing::warn!("the heartbeat: {e}");
        }
    }

    // ------------------------------------------------------------ login

    async fn locked_out(&self, ip: IpAddr) -> bool {
        let mut f = self.failures.lock().await;
        let q = f.entry(ip).or_default();
        while q.front().is_some_and(|t| t.elapsed() > LOCKOUT) {
            q.pop_front();
        }
        q.len() >= LOCKOUT_FAILURES
    }

    async fn failed(&self, ip: IpAddr) {
        self.failures.lock().await.entry(ip).or_default().push_back(Instant::now());
    }

    fn check_login(&self, given: &str) -> bool {
        // Compare hashes, so the comparison's time doesn't depend on where the values differ.
        let a = Sha256::digest(given.trim().as_bytes());
        let b = Sha256::digest(self.login.as_bytes());
        a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }

    async fn authed(&self, headers: &HeaderMap) -> bool {
        let Some(cookie) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) else { return false };
        let Some(sid) = cookie.split(';').find_map(|c| c.trim().strip_prefix("hub_session=")) else { return false };
        let mut sessions = self.sessions.lock().await;
        sessions.retain(|_, t| t.elapsed() < Duration::from_secs(SESSION_HOURS as u64 * 3600));
        sessions.contains_key(sid)
    }
}

// ------------------------------------------------------------ web

type Ctx = State<Arc<Hub>>;

pub fn router(hub: Arc<Hub>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .route("/", get(page))
        .route("/api/state", get(api_state))
        .route("/api/accounts", post(api_add))
        .route("/api/accounts/{slot}", axum::routing::delete(api_delete))
        .route("/api/accounts/{slot}/sign-in", post(api_sign_in))
        .route("/api/accounts/{slot}/setup-token", post(api_setup_token))
        .route("/api/flows/{id}", get(api_flow).delete(api_cancel))
        .route("/api/flows/{id}/code", post(api_code))
        .route("/assets/{name}", get(asset))
        .layer(axum::middleware::map_response(security_headers))
        .with_state(hub)
}

/// The page's own files, built into the binary: no CDN, nothing fetched at runtime.
const ASSETS: &[(&str, &str, &str)] = &[
    ("hub.css", "text/css; charset=utf-8", include_str!("../web/hub.css")),
    ("app.js", "text/javascript; charset=utf-8", include_str!("../web/app.js")),
    ("icon.svg", "image/svg+xml", include_str!("../web/icon.svg")),
];

async fn asset(UrlPath(name): UrlPath<String>) -> Response {
    match ASSETS.iter().find(|(n, _, _)| *n == name) {
        Some((_, kind, body)) => {
            ([(header::CONTENT_TYPE, *kind), (header::CACHE_CONTROL, "no-cache")], *body).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// On every answer: scripts and styles only from the hub itself (none inline), no framing, no referrer to the
/// sign-in page it links to, and nothing cached but its own files.
async fn security_headers(mut res: Response) -> Response {
    let h = res.headers_mut();
    let set = |h: &mut HeaderMap, k: &'static str, v: &'static str| {
        h.insert(header::HeaderName::from_static(k), header::HeaderValue::from_static(v));
    };
    set(
        h,
        "content-security-policy",
        "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self'; \
        form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
    );
    set(h, "x-frame-options", "DENY");
    set(h, "referrer-policy", "no-referrer");
    set(h, "x-content-type-options", "nosniff");
    if !h.contains_key(header::CACHE_CONTROL) {
        set(h, "cache-control", "no-store");
    }
    res
}

pub async fn serve(hub: Arc<Hub>) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(hub.cfg.listen).await?;
    tracing::info!(listen = %hub.cfg.listen, "hub web UI listening");
    axum::serve(listener, router(hub).into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

async fn healthz(State(hub): Ctx) -> impl IntoResponse {
    match std::fs::metadata(&hub.cfg.dir) {
        Ok(_) => (StatusCode::OK, "ok"),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "can't read its state"),
    }
}

const LOGIN_HTML: &str = include_str!("../web/login.html");

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn login_html(hub: &Hub, msg: &str) -> Html<String> {
    let hint =
        hub.cfg.login_hint.as_deref().map_or(String::new(), |h| format!(r#"<p class="login-hint">{}</p>"#, esc(h)));
    let msg = if msg.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="field-error" id="login-error" role="alert">{}</p>"#, esc(msg))
    };
    let invalid = if msg.is_empty() { "" } else { r#" aria-invalid="true" aria-describedby="login-error""# };
    Html(LOGIN_HTML.replace("{hint}", &hint).replace("{msg}", &msg).replace("{invalid}", invalid))
}

async fn login_page(State(hub): Ctx) -> Html<String> {
    login_html(&hub, "")
}

#[derive(Deserialize)]
struct LoginForm {
    login: String,
}

async fn login(
    State(hub): Ctx,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(f): Form<LoginForm>,
) -> Response {
    let ip = client_ip(peer.ip(), &headers, &hub.cfg.trusted_proxies);
    if hub.locked_out(ip).await {
        return (StatusCode::TOO_MANY_REQUESTS, login_html(&hub, "Too many tries: wait 15 minutes.")).into_response();
    }
    if !hub.check_login(&f.login) {
        hub.failed(ip).await;
        tracing::warn!(%ip, "a failed hub login");
        return (StatusCode::UNAUTHORIZED, login_html(&hub, "That isn't the hub's login key.")).into_response();
    }
    let sid = new_id();
    hub.sessions.lock().await.insert(sid.clone(), Instant::now());
    let cookie =
        format!("hub_session={sid}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age={}", SESSION_HOURS * 3600);
    ([(header::SET_COOKIE, cookie)], Redirect::to("/")).into_response()
}

async fn logout(State(hub): Ctx, headers: HeaderMap) -> Response {
    if let Some(sid) = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| c.split(';').find_map(|c| c.trim().strip_prefix("hub_session=").map(str::to_string)))
    {
        hub.sessions.lock().await.remove(&sid);
    }
    (
        [(header::SET_COOKIE, "hub_session=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0")],
        Redirect::to("/login"),
    )
        .into_response()
}

async fn page(State(hub): Ctx, headers: HeaderMap) -> Response {
    if !hub.authed(&headers).await {
        return Redirect::to("/login").into_response();
    }
    Html(PAGE_HTML).into_response()
}

macro_rules! guard {
    ($hub:expr, $headers:expr) => {
        if !$hub.authed(&$headers).await {
            return (StatusCode::UNAUTHORIZED, Json(json!({"error": "sign in to the hub first"}))).into_response();
        }
    };
}

async fn api_state(State(hub): Ctx, headers: HeaderMap) -> Response {
    guard!(hub, headers);
    Json(hub.state_json().await).into_response()
}

async fn api_add(State(hub): Ctx, headers: HeaderMap) -> Response {
    guard!(hub, headers);
    let mut store = hub.store.lock().await;
    // The lowest free number: the hub's numbers are the account numbers on every machine (1 is each machine's
    // default sign-in).
    let slot = (1..).find(|n| !store.accounts.contains_key(n)).unwrap_or(1);
    store.accounts.insert(slot, HubAccount::default());
    let snapshot = store.clone();
    drop(store);
    let _ = hub.save(&snapshot).await;
    Json(json!({"slot": slot})).into_response()
}

async fn api_sign_in(State(hub): Ctx, headers: HeaderMap, UrlPath(slot): UrlPath<u32>) -> Response {
    guard!(hub, headers);
    if !hub.store.lock().await.accounts.contains_key(&slot) {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "no such account"}))).into_response();
    }
    match hub.start_sign_in(slot).await {
        Ok(id) => Json(json!({"id": id})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

async fn api_setup_token(State(hub): Ctx, headers: HeaderMap, UrlPath(slot): UrlPath<u32>) -> Response {
    guard!(hub, headers);
    if !hub.store.lock().await.accounts.contains_key(&slot) {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "no such account"}))).into_response();
    }
    match hub.start_setup_token(slot).await {
        Ok(id) => Json(json!({"id": id})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

async fn api_delete(State(hub): Ctx, headers: HeaderMap, UrlPath(slot): UrlPath<u32>) -> Response {
    guard!(hub, headers);
    match hub.delete(slot).await {
        Ok(true) => Json(json!({"ok": true})).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, Json(json!({"error": "no such account"}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

async fn api_flow(State(hub): Ctx, headers: HeaderMap, UrlPath(id): UrlPath<String>) -> Response {
    guard!(hub, headers);
    match hub.flow_json(&id).await {
        Some(v) => Json(v).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "no such sign-in"}))).into_response(),
    }
}

async fn api_cancel(State(hub): Ctx, headers: HeaderMap, UrlPath(id): UrlPath<String>) -> Response {
    guard!(hub, headers);
    if !hub.cancel_flow(&id).await {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "no such sign-in"}))).into_response();
    }
    match hub.flow_json(&id).await {
        Some(v) => Json(v).into_response(),
        None => Json(json!({"ok": true})).into_response(),
    }
}

#[derive(Deserialize)]
struct CodeBody {
    code: String,
}

async fn api_code(
    State(hub): Ctx,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<CodeBody>,
) -> Response {
    guard!(hub, headers);
    match hub.paste(&id, &b.code).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

const PAGE_HTML: &str = include_str!("../web/app.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_tokens_are_found_in_terminal_output() {
        let out = "Opening browser…\nIf the browser didn't open, visit:\n\u{1b}]8;;https://claude.ai/oauth/authorize?abc123\u{7}x\nPaste code here if prompted > ";
        assert_eq!(find_url(out).as_deref(), Some("https://claude.ai/oauth/authorize?abc123"));
        // The screen setup-token draws: the token, then a blank line and "Store this token securely".
        let body = "A".repeat(60) + "-_" + &"b".repeat(33);
        let token = format!("sk-ant-oat01-{body}");
        let screen = format!(
            "Your OAuth token (valid for 1 year):\n\n{token}\n\nStore this token securely. You won't be able to see it again.\n"
        );
        assert_eq!(find_token(&screen).as_deref(), Some(token.as_str()), "the token alone, not the line under it");
        assert_eq!(find_token(&format!("{token}\nStore")).as_deref(), Some(token.as_str()));
        assert!(find_token(&token).is_none(), "not until something follows it: it may still be arriving");
        assert!(find_token("sk-ant-oat01-short\n").is_none());
        assert!(find_token("no token here").is_none());
        let rejected = "visit:\nhttps://claude.ai/oauth/authorize?x\nPaste code here if prompted > That doesn't look like the full code.\n";
        assert_eq!(latest_message(rejected).as_deref(), Some("That doesn't look like the full code."));
        let boxed = "\u{1b}[1m│ Couldn't log in: That code wasn't accepted. │\n╰────╯\n";
        assert_eq!(latest_message(boxed).as_deref(), Some("Couldn't log in: That code wasn't accepted."));
        assert_eq!(
            latest_message("Paste code here if prompted >\n│ ****************abc123 │\n"),
            None,
            "the masked code"
        );
        assert_eq!(strip_ansi("Press\u{1b}[1CEnter\u{1b}[2Cto"), "Press Enter  to");
    }

    #[test]
    fn the_client_address_comes_from_a_trusted_proxy_only() {
        let proxy: IpAddr = "10.5.7.11".parse().unwrap();
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "203.0.113.9, 10.5.7.11".parse().unwrap());
        assert_eq!(client_ip(proxy, &h, &[proxy]), "203.0.113.9".parse::<IpAddr>().unwrap());
        let other: IpAddr = "10.5.7.99".parse().unwrap();
        assert_eq!(client_ip(other, &h, &[proxy]), other, "an untrusted connection's header is ignored");
        assert_eq!(client_ip(proxy, &HeaderMap::new(), &[proxy]), proxy);
    }

    #[tokio::test]
    async fn the_login_locks_an_address_out_after_five_failures() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("login"), "x".repeat(40)).unwrap();
        let hub = Hub::open(HubConfig {
            dir: d.path().join("hub"),
            claude: "claude".into(),
            listen: "127.0.0.1:0".parse().unwrap(),
            login_file: d.path().join("login"),
            trusted_proxies: vec![],
            heartbeat: None,
            login_hint: None,
        })
        .unwrap();
        assert!(hub.check_login(&"x".repeat(40)));
        let page = login_html(&hub, "").0;
        assert!(!page.contains("login-hint"), "no hint unless configured");
        assert!(!page.contains(&"x".repeat(40)), "never the key");
        assert!(!hub.check_login("nope"));
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        for _ in 0..5 {
            assert!(!hub.locked_out(ip).await);
            hub.failed(ip).await;
        }
        assert!(hub.locked_out(ip).await);
        assert!(!hub.locked_out("192.0.2.2".parse().unwrap()).await);
    }

    #[tokio::test]
    async fn the_tokens_file_lists_accounts_with_a_token_and_never_the_page() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("login"), "y".repeat(40)).unwrap();
        let hub = Hub::open(HubConfig {
            dir: d.path().join("hub"),
            claude: "claude".into(),
            listen: "127.0.0.1:0".parse().unwrap(),
            login_file: d.path().join("login"),
            trusted_proxies: vec![],
            heartbeat: None,
            login_hint: None,
        })
        .unwrap();
        let mut s = Store::default();
        s.accounts.insert(
            2,
            HubAccount {
                email: Some("a@example.com".into()),
                account_uuid: Some("u2".into()),
                signed_in_at: Some(Timestamp::now()),
                token: Some("sk-ant-oat01-secret".into()),
                token_at: Some(Timestamp::now()),
                ..Default::default()
            },
        );
        s.accounts.insert(3, HubAccount { email: Some("b@example.com".into()), ..Default::default() });
        *hub.store.lock().await = s.clone();
        hub.save(&s).await.unwrap();
        let tokens: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(hub.tokens_file()).unwrap()).unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0]["slot"], 2);
        let page = hub.state_json().await.to_string();
        assert!(!page.contains("sk-ant"), "the page never carries a token");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(hub.tokens_file()).unwrap().permissions().mode() & 0o777, 0o600);
        let readings = hub.readings().await;
        assert_eq!(readings.len(), 2, "sign-in age and token age for the account with a UUID");
        let signin = readings.iter().find(|r| r["kind"] == "claude.signin_age").unwrap();
        assert!(signin["data"]["number"].as_u64().is_some(), "the account's number, for account lists");
        // A token whose account's organization isn't known stays in the bundle, shown as unchecked.
        {
            let mut st = hub.store.lock().await;
            st.accounts.get_mut(&2).unwrap().token_org_id = Some("org-b".into());
            let snap = st.clone();
            drop(st);
            hub.save(&snap).await.unwrap();
        }
        let tokens: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(hub.tokens_file()).unwrap()).unwrap();
        assert_eq!(tokens.len(), 1, "one side unknown: never dropped");
        assert_eq!(hub.state_json().await["accounts"][0]["tokenUnchecked"], true);
        // A token found to be another account's leaves the bundle.
        {
            let mut st = hub.store.lock().await;
            let a = st.accounts.get_mut(&2).unwrap();
            a.org_id = Some("org-a".into());
            a.token_org_id = Some("org-b".into());
            let snap = st.clone();
            drop(st);
            hub.save(&snap).await.unwrap();
        }
        let tokens: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(hub.tokens_file()).unwrap()).unwrap();
        assert!(tokens.is_empty(), "a token for the wrong account is never sent");
        assert_eq!(hub.state_json().await["accounts"][0]["tokenUnchecked"], false);
        assert_eq!(hub.state_json().await["accounts"][0]["tokenWrongAccount"], true);
        // Deleting account 2 drops it from the tokens file.
        assert!(hub.delete(2).await.unwrap());
        assert!(!hub.delete(2).await.unwrap());
        let tokens: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(hub.tokens_file()).unwrap()).unwrap();
        assert!(tokens.is_empty());
    }

    fn test_hub(d: &Path, claude: PathBuf) -> Arc<Hub> {
        std::fs::write(d.join("login"), "z".repeat(40)).unwrap();
        Hub::open(HubConfig {
            dir: d.join("hub"),
            claude,
            listen: "127.0.0.1:0".parse().unwrap(),
            login_file: d.join("login"),
            trusted_proxies: vec![],
            heartbeat: None,
            login_hint: None,
        })
        .unwrap()
    }

    /// A stand-in for `claude auth login`: prints a sign-in URL, says a code without a `#` is cut short and waits
    /// for another, refuses any other code; writes its process ID so a test can see it stopped.
    fn fake_claude(d: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = d.join("claude");
        let script = format!(
            "#!/bin/sh\necho $$ > {pid}\necho 'Visit: https://claude.ai/oauth/authorize?fake=1'\n\
            printf 'Paste code here if prompted > '\nwhile read code; do\n  case \"$code\" in\n    \
            *'#'*) echo \"Couldn't log in: that code wasn't accepted.\"; exit 1;;\n    \
            *) printf \"That doesn't look like the full code.\\nPaste code here if prompted > \";;\n  esac\ndone\n",
            pid = d.join("pid").display()
        );
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    async fn flow_until(hub: &Hub, id: &str, stage: &str) -> Value {
        for _ in 0..100 {
            let f = hub.flow_json(id).await.unwrap();
            if f["stage"] == stage {
                return f;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the flow never reached {stage}: {}", hub.flow_json(id).await.unwrap());
    }

    fn alive(pid_file: &Path) -> bool {
        let pid = std::fs::read_to_string(pid_file).unwrap().trim().to_string();
        std::process::Command::new("kill").args(["-0", &pid]).stderr(Stdio::null()).status().unwrap().success()
    }

    #[tokio::test]
    async fn a_sign_in_shows_its_stages_and_claude_codes_messages_and_cancel_stops_it() {
        let d = tempfile::tempdir().unwrap();
        let hub = test_hub(d.path(), fake_claude(d.path()));
        hub.store.lock().await.accounts.insert(1, HubAccount::default());

        let id = hub.start_sign_in(1).await.unwrap();
        let f = flow_until(&hub, &id, "link").await;
        assert_eq!(f["url"], "https://claude.ai/oauth/authorize?fake=1");
        assert!(f["message"].is_null(), "nothing said about a code before one went in");
        let st = hub.state_json().await;
        assert_eq!(st["flows"][0]["id"], id.as_str(), "a running flow is listed, for a reloaded page");

        // A code cut short: Claude Code says so and waits for the code again.
        hub.paste(&id, "abc").await.unwrap();
        let f = flow_until(&hub, &id, "link").await;
        let f = if f["message"].is_null() {
            tokio::time::sleep(Duration::from_millis(300)).await;
            hub.flow_json(&id).await.unwrap()
        } else {
            f
        };
        assert_eq!(f["message"], "That doesn't look like the full code.");
        // A refused code ends it, with Claude Code's reason.
        hub.paste(&id, "abc#def").await.unwrap();
        let f = flow_until(&hub, &id, "failed").await;
        assert_eq!(f["message"], "Couldn't log in: that code wasn't accepted.");
        assert!(hub.state_json().await["flows"].as_array().unwrap().is_empty());

        // Cancel kills the program.
        let id = hub.start_sign_in(1).await.unwrap();
        flow_until(&hub, &id, "link").await;
        assert!(alive(&d.path().join("pid")));
        assert!(hub.cancel_flow(&id).await);
        assert_eq!(hub.flow_json(&id).await.unwrap()["stage"], "cancelled");
        let mut stopped = false;
        for _ in 0..40 {
            if !alive(&d.path().join("pid")) {
                stopped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(stopped, "the cancelled sign-in's program is stopped");
        assert!(!hub.cancel_flow("nope").await);

        // A new flow for the same account stops the one already running.
        let first = hub.start_sign_in(1).await.unwrap();
        flow_until(&hub, &first, "link").await;
        let second = hub.start_sign_in(1).await.unwrap();
        assert_eq!(hub.flow_json(&first).await.unwrap()["stage"], "cancelled");
        flow_until(&hub, &second, "link").await;
        // Deleting the account stops its flow too.
        assert!(hub.delete(1).await.unwrap());
        assert!(hub.flow_json(&second).await.is_none());
    }

    #[tokio::test]
    async fn every_answer_carries_the_security_headers_and_the_api_needs_the_login() {
        let d = tempfile::tempdir().unwrap();
        let hub = test_hub(d.path(), "claude".into());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router(hub).into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
        });
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        let base = format!("http://{addr}");
        for path in ["/login", "/", "/api/state", "/assets/hub.css", "/healthz"] {
            let r = client.get(format!("{base}{path}")).send().await.unwrap();
            let h = r.headers();
            let csp = h["content-security-policy"].to_str().unwrap();
            assert!(csp.contains("script-src 'self'") && !csp.contains("unsafe-inline"), "{path}: {csp}");
            assert!(csp.contains("frame-ancestors 'none'"), "{path}");
            assert_eq!(h["x-frame-options"], "DENY", "{path}");
            assert_eq!(h["referrer-policy"], "no-referrer", "{path}");
        }
        let r = client.get(format!("{base}/api/state")).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let r = client.delete(format!("{base}/api/flows/x")).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "cancel needs the login too");
        let r = client.get(format!("{base}/")).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        let r = client.get(format!("{base}/assets/app.js")).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert!(r.headers()["content-type"].to_str().unwrap().starts_with("text/javascript"));
        assert_eq!(client.get(format!("{base}/assets/nope")).send().await.unwrap().status(), StatusCode::NOT_FOUND);
        let page = client.get(format!("{base}/login")).send().await.unwrap().text().await.unwrap();
        assert!(!page.contains("<script>") && !page.contains("onclick"), "no inline script");
        for (_, _, body) in ASSETS {
            assert!(!body.contains("localStorage"), "nothing kept in the browser");
        }
    }

    #[test]
    fn a_setup_token_for_another_account_is_refused() {
        assert!(wrong_account(Some("org-a"), Some("org-a"), Some("a@example.com")).is_none());
        let why = wrong_account(Some("org-a"), Some("org-b"), Some("a@example.com")).unwrap();
        assert!(why.contains("different Claude account") && why.contains("a@example.com"), "{why}");
        assert!(wrong_account(None, Some("org-b"), None).is_none(), "no sign-in yet: nothing to compare with");
        assert!(wrong_account(Some("org-a"), None, None).is_none(), "the token's organization unread");
        let mut h = HeaderMap::new();
        h.insert("anthropic-organization-id", " org-b ".parse().unwrap());
        assert_eq!(org_header(&h).as_deref(), Some("org-b"));
        assert_eq!(org_header(&HeaderMap::new()), None);
    }
}
