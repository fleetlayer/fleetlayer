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

struct Flow {
    kind: &'static str,
    slot: u32,
    started: Instant,
    /// The sign-in URL once the program printed it.
    url: Arc<std::sync::Mutex<Option<String>>>,
    /// What it printed, the newest 64 KB (in memory only), for its latest message.
    output: Arc<std::sync::Mutex<String>>,
    input: Option<Input>,
    pasted: bool,
    /// `running`, `done`, or `failed: …`.
    state: Arc<std::sync::Mutex<String>>,
}

pub struct Hub {
    cfg: HubConfig,
    store: Mutex<Store>,
    flows: Mutex<HashMap<String, Flow>>,
    sessions: Mutex<HashMap<String, Instant>>,
    failures: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
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
        // The tokens bundle: every account with a setup token and a known account UUID.
        let tokens: Vec<Value> = store
            .accounts
            .iter()
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
    async fn identify(&self, slot: u32) -> Option<(String, Option<String>, Option<Timestamp>)> {
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
        Some((who.email?, who.account_uuid, expires))
    }

    // ------------------------------------------------------------ flows

    async fn start_sign_in(self: &Arc<Self>, slot: u32) -> anyhow::Result<String> {
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
            let (hub, state, output) = (self.clone(), state.clone(), output.clone());
            tokio::spawn(async move {
                let result = tokio::time::timeout(FLOW_TIMEOUT, child.wait()).await;
                let ok = matches!(result, Ok(Ok(s)) if s.success());
                if !ok {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let why = latest_message(&output.lock().unwrap())
                        .unwrap_or_else(|| "the sign-in didn't finish (wrong or expired code, or it timed out)".into());
                    *state.lock().unwrap() = format!("failed: {why}");
                    return;
                }
                match hub.identify(slot).await {
                    Some((email, uuid, expires)) => {
                        let mut store = hub.store.lock().await;
                        let a = store.accounts.entry(slot).or_default();
                        a.email = Some(email.clone());
                        if uuid.is_some() {
                            a.account_uuid = uuid;
                        }
                        a.signed_in_at = Some(Timestamp::now());
                        a.sign_in_expires_at = expires;
                        let snapshot = store.clone();
                        drop(store);
                        let _ = hub.save(&snapshot).await;
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
        use portable_pty::{CommandBuilder, PtySize, native_pty_system};
        let pty = native_pty_system();
        let pair = pty.openpty(PtySize { rows: 50, cols: 200, pixel_width: 0, pixel_height: 0 })?;
        // A throwaway config directory: setup-token signs in only to print the token.
        let scratch = self.cfg.dir.join(format!("setup-{slot}"));
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
        let id = new_id();
        let url = Arc::new(std::sync::Mutex::new(None));
        let output = Arc::new(std::sync::Mutex::new(String::new()));
        let state = Arc::new(std::sync::Mutex::new("running".to_string()));
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
            let (hub, state, output) = (self.clone(), state.clone(), output.clone());
            let master = pair.master;
            tokio::spawn(async move {
                let started = Instant::now();
                loop {
                    tokio::time::sleep(Duration::from_millis(500)).await;
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
                        let mut store = hub.store.lock().await;
                        let a = store.accounts.entry(slot).or_default();
                        a.token = Some(token);
                        a.token_at = Some(Timestamp::now());
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
        f.pasted = true;
        Ok(())
    }

    async fn flow_json(&self, id: &str) -> Option<Value> {
        let flows = self.flows.lock().await;
        let f = flows.get(id)?;
        let state = f.state.lock().unwrap().clone();
        // Only after a code went in: before that, the latest line is the welcome text.
        let message = if f.pasted { latest_message(&f.output.lock().unwrap()) } else { None };
        Some(
            json!({"id": id, "kind": f.kind, "slot": f.slot, "url": *f.url.lock().unwrap(), "state": state, "message": message,
            "seconds": f.started.elapsed().as_secs()}),
        )
    }

    /// What the page shows: accounts (never their tokens), their dates and warnings.
    async fn state_json(&self) -> Value {
        let store = self.store.lock().await;
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
                json!({"slot": slot, "email": a.email, "accountUuid": a.account_uuid,
                    "signedInAt": a.signed_in_at, "signInDaysLeft": signin_left,
                    "signInWarn": signin_days.is_some_and(|d| d >= SIGNIN_WARN_DAYS) || signin_left.is_some_and(|l| l <= SIGNIN_LIFETIME_DAYS - SIGNIN_WARN_DAYS),
                    "hasToken": a.token.is_some(), "tokenAt": a.token_at, "tokenDaysLeft": token_left,
                    "tokenWarn": token_left.is_some_and(|l| l <= TOKEN_WARN_DAYS_LEFT)})
            })
            .collect();
        json!({"accounts": accounts})
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
                "number": number, "signedIn": !a["signedInAt"].is_null(), "signInNeeded": why}}));
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
        self.flows.lock().await.retain(|_, f| f.slot != slot);
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
        .route("/api/flows/{id}", get(api_flow))
        .route("/api/flows/{id}/code", post(api_code))
        .with_state(hub)
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

const LOGIN_HTML: &str = r#"<!doctype html><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1">
<title>Claude hub</title><style>body{font:16px system-ui;max-width:30rem;margin:4rem auto;padding:0 1rem;line-height:1.45}
input,button{font:inherit;padding:.5rem;width:100%;box-sizing:border-box;margin:.25rem 0}.hint{color:#666;font-size:.9em}
.msg{color:#b91c1c}@media (prefers-color-scheme:dark){body{background:#111;color:#ddd}.hint{color:#999}.msg{color:#f87171}}</style>
<h1>Claude hub</h1>
<p>This hub holds your fleet's Claude sign-ins and sends each account's setup token to your machines, sealed.</p>
<p>To open it, paste the hub's own login key: the long random value set up with the hub. Signing in to your Claude
accounts comes next, on the following page.</p>
<form method=post action=/login><label for=login>Hub login key</label>
<input type=password id=login name=login autocomplete=current-password autofocus><button>Open the hub</button></form>
{hint}<p class=msg>{msg}</p>"#;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn login_html(hub: &Hub, msg: &str) -> Html<String> {
    let hint = hub
        .cfg
        .login_hint
        .as_deref()
        .map_or(String::new(), |h| format!("<p class=hint>Where the key is: {}</p>", esc(h)));
    Html(LOGIN_HTML.replace("{hint}", &hint).replace("{msg}", msg))
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
    ([(header::SET_COOKIE, "hub_session=; Max-Age=0; Path=/")], Redirect::to("/login")).into_response()
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

const PAGE_HTML: &str = r#"<!doctype html><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1">
<title>Claude hub</title>
<style>
body{font:15px system-ui;max-width:52rem;margin:2rem auto;padding:0 1rem;color:#222}
table{width:100%;border-collapse:collapse}td,th{text-align:left;padding:.5rem;border-bottom:1px solid #ddd}
button{font:inherit;padding:.35rem .7rem;cursor:pointer}.warn{color:#b45309;font-weight:600}
#flow{border:1px solid #ccc;padding:1rem;margin:1rem 0;display:none}input{font:inherit;padding:.4rem;width:70%}
@media (prefers-color-scheme:dark){body{background:#111;color:#ddd}td,th{border-color:#333}#flow{border-color:#444}}
</style>
<h1>Claude hub</h1>
<p>Each account needs two things, so it has two buttons:</p>
<ul><li><b>Sign in</b>: the account's normal sign-in. The hub uses it only to read the account's usage. Redo it about
once a month; the hub warns a few days before it runs out.</li>
<li><b>Make setup token</b>: the token your machines get, sealed, for their agents to use. It lasts a year.</li></ul>
<p>Both open Claude's sign-in page: sign in as that account, then paste the code it shows back here.</p>
<div id=flow><p id=fstate></p><p id=furl></p>
<p><input id=code placeholder="Paste the code from the sign-in page"> <button onclick=paste()>Send</button></p></div>
<table><thead><tr><th>#</th><th>Account</th><th>Sign-in</th><th>Setup token</th><th></th></tr></thead><tbody id=rows></tbody></table>
<p><button onclick=add()>Add an account</button>
<form method=post action=/logout style=display:inline><button>Log out</button></form></p>
<script>
let flow=null;
async function j(u,o){const r=await fetch(u,o);return r.json()}
function esc(s){return String(s??'').replace(/[&<>"]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]))}
let accts=[];
async function load(){const s=await j('/api/state');accts=s.accounts;document.getElementById('rows').innerHTML=s.accounts.map(a=>`<tr>
<td>${a.slot}</td><td>${esc(a.email)||'<i>not signed in</i>'}</td>
<td class="${a.signInWarn?'warn':''}">${a.signedInAt?(a.signInDaysLeft+' days left'):'—'}</td>
<td class="${a.tokenWarn?'warn':''}">${a.hasToken?(a.tokenDaysLeft+' days left'):'—'}</td>
<td><button onclick="start(${a.slot},'sign-in')">Sign in</button> <button onclick="start(${a.slot},'setup-token')">Make setup token</button>
<button onclick="del(${a.slot})">Delete</button></td></tr>`).join('')}
async function del(slot){const email=(accts.find(a=>a.slot===slot)||{}).email;if(!confirm(`Delete account ${slot}${email?' ('+email+')':''}? Its sign-in here is signed out, and every machine clears account ${slot} on the next publish. To revoke its setup token as well, do it on claude.ai.`))return;
const r=await j('/api/accounts/'+slot,{method:'DELETE'});if(r.error)alert(r.error);load()}
async function add(){await j('/api/accounts',{method:'POST'});load()}
async function start(slot,kind){const r=await j(`/api/accounts/${slot}/${kind}`,{method:'POST'});if(r.error){alert(r.error);return}
flow=r.id;document.getElementById('flow').style.display='block';poll()}
async function poll(){if(!flow)return;const f=await j('/api/flows/'+flow);
document.getElementById('fstate').textContent=(f.kind==='sign_in'?'Signing in':'Making a setup token')+' for account '+f.slot+': '+f.state+(f.message&&f.state==='running'?' ('+f.message+')':'');
document.getElementById('furl').innerHTML=f.url?`<a href="${esc(f.url)}" target=_blank rel=noopener>Open the sign-in page</a>, sign in as that account, then paste the code it shows below.`:'Waiting for the sign-in link…';
if(f.state==='running')setTimeout(poll,1500);else{flow=null;load()}}
async function paste(){const c=document.getElementById('code');const r=await j('/api/flows/'+flow+'/code',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({code:c.value})});c.value='';if(r.error)alert(r.error)}
load();
</script>"#;

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
        assert!(!page.contains("Where the key is"), "no hint unless configured");
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
        // Deleting account 2 drops it from the tokens file.
        assert!(hub.delete(2).await.unwrap());
        assert!(!hub.delete(2).await.unwrap());
        let tokens: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(hub.tokens_file()).unwrap()).unwrap();
        assert!(tokens.is_empty());
    }
}
