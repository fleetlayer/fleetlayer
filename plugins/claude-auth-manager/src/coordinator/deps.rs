//! The real world beneath the usage monitor: the Keychain (macOS) or credentials files (Linux), Anthropic's usage
//! and profile endpoints, `claude auth`, and the state file. A port of the rotator coordinator's `nodeDeps`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::core::{Deps, Persisted, SignInRun, Stored, UsageAnswer};
use super::model::*;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
const LOG_LIMIT: u64 = 1024 * 1024;

/// A Retry-After header as milliseconds from now: seconds, or an HTTP date.
pub fn retry_after(header: Option<&str>, now: Ms) -> Option<Ms> {
    let header = header?.trim();
    if let Ok(seconds) = header.parse::<f64>() {
        return Some((seconds * 1000.0).max(0.0) as Ms);
    }
    let at = jiff::fmt::rfc2822::parse(header).ok().map(|z| z.timestamp().as_millisecond())?;
    Some((at - now).max(0))
}

/// The environment `claude` children get: the node's own, without what a Claude Code session sets for itself
/// (which login it reads, its API endpoint), so a node a session started behaves as one launchd started.
pub fn clean_env() -> Vec<(String, String)> {
    std::env::vars()
        .filter(|(name, _)| {
            name == "CLAUDE_ACCOUNT_ROTATOR_DIR"
                || !(name.starts_with("CLAUDE_") || name.starts_with("ANTHROPIC_") || name == "CLAUDECODE")
        })
        .collect()
}

/// Appends to the monitor's log, `coordinator.log`, moving it aside at 1 MB.
#[derive(Clone)]
pub struct Logger {
    file: PathBuf,
}

impl Logger {
    pub fn new(file: PathBuf) -> Self {
        Logger { file }
    }

    pub fn log(&self, text: &str) {
        use std::io::Write;
        let line = format!("{} {text}\n", iso(jiff::Timestamp::now().as_millisecond()));
        let write = || -> std::io::Result<()> {
            if std::fs::metadata(&self.file).is_ok_and(|m| m.len() > LOG_LIMIT) {
                std::fs::rename(&self.file, self.file.with_extension("log.1"))?;
            }
            let mut f = open_private(&self.file)?;
            f.write_all(line.as_bytes())
        };
        if write().is_err() {
            // A log that can't be written mustn't stop the monitor; stderr still has it.
            eprint!("{line}");
        }
        tracing::debug!(target: "usage", "{text}");
    }
}

fn open_private(file: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(file)
}

struct Output {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Runs a program with argv (never a shell) and a time limit; -1 when it couldn't run or ran too long.
async fn run(program: &Path, args: &[&str], env: &[(String, String)], timeout: Duration, cwd: Option<&Path>) -> Output {
    let mut cmd = Command::new(program);
    cmd.args(args).env_clear().envs(env.iter().map(|(k, v)| (k, v))).stdin(Stdio::null()).kill_on_drop(true);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => Output {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into(),
            stderr: String::from_utf8_lossy(&out.stderr).into(),
        },
        Ok(Err(e)) => Output { code: -1, stdout: String::new(), stderr: e.to_string() },
        Err(_) => Output { code: -1, stdout: String::new(), stderr: "timed out".into() },
    }
}

fn with_slot(env: &[(String, String)], slot: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = env.to_vec();
    for (k, v) in slot_env(slot) {
        out.retain(|(name, _)| name != &k);
        out.push((k, v));
    }
    out
}

/// What the real deps need to know about the machine.
pub struct RealDeps {
    pub home: PathBuf,
    pub claude: PathBuf,
    pub state_file: PathBuf,
    pub logger: Logger,
    env: Vec<(String, String)>,
    http: reqwest::Client,
    pending: Arc<Mutex<Option<Persisted>>>,
    is_saving: Arc<Mutex<bool>>,
}

impl RealDeps {
    pub fn new(home: PathBuf, claude: PathBuf, state_file: PathBuf, logger: Logger) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(20)).build().unwrap_or_default();
        RealDeps {
            home,
            claude,
            state_file,
            logger,
            env: clean_env(),
            http,
            pending: Arc::default(),
            is_saving: Arc::default(),
        }
    }

    async fn oauth_get(&self, url: &str, token: &str) -> Result<reqwest::Response, reqwest::Error> {
        self.http
            .get(url)
            .bearer_auth(token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("Content-Type", "application/json")
            .send()
            .await
    }

    /// Writes the state file now (as the process exits).
    pub fn flush(&self) {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(p) = pending {
            write_state(&self.state_file, &p, &self.logger);
        }
    }
}

fn write_state(file: &Path, p: &Persisted, logger: &Logger) {
    let tmp = file.with_extension("json.tmp");
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let text = serde_json::to_string_pretty(p).map_err(std::io::Error::other)?;
        let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(format!("{text}\n").as_bytes())?;
        std::fs::rename(&tmp, file)
    };
    if let Err(e) = write() {
        logger.log(&format!("could not save {}: {e}", file.display()));
    }
}

#[async_trait]
impl Deps for RealDeps {
    fn now(&self) -> Ms {
        jiff::Timestamp::now().as_millisecond()
    }

    async fn sleep(&self, ms: Ms) {
        tokio::time::sleep(Duration::from_millis(ms.max(0) as u64)).await;
    }

    async fn read_stored(&self, slot: &str) -> Stored {
        if cfg!(target_os = "macos") {
            let user = keychain_user(std::env::var("USER").ok().as_deref());
            let item = keychain_item(slot);
            let out = run(
                Path::new("/usr/bin/security"),
                &["find-generic-password", "-a", &user, "-w", "-s", &item],
                &self.env,
                Duration::from_secs(10),
                None,
            )
            .await;
            return match out.code {
                0 => Stored::Present(credential_from(out.stdout.trim())),
                44 => Stored::Absent,
                code => Stored::Unreadable(code),
            };
        }
        // A file that cannot be read holds no login, as Claude Code counts it.
        let Some(file) = credentials_file(slot, self.home.to_str()) else {
            return Stored::Absent;
        };
        match tokio::fs::read_to_string(&file).await {
            Ok(text) => Stored::Present(credential_from(&text)),
            Err(_) => Stored::Absent,
        }
    }

    async fn fetch_usage(&self, access_token: &str) -> UsageAnswer {
        match self.oauth_get(USAGE_URL, access_token).await {
            Ok(res) => {
                let status = res.status().as_u16();
                if status == 401 || status == 403 {
                    return UsageAnswer::Rejected;
                }
                if !res.status().is_success() {
                    let header = res.headers().get("retry-after").and_then(|v| v.to_str().ok()).map(str::to_string);
                    return UsageAnswer::Failed { status, retry_after_ms: retry_after(header.as_deref(), self.now()) };
                }
                match res.json::<Value>().await {
                    Ok(body) => UsageAnswer::Ok(body),
                    Err(e) => {
                        self.log(&format!("usage call failed: {e}"));
                        UsageAnswer::Failed { status: 0, retry_after_ms: None }
                    }
                }
            }
            Err(e) => {
                self.log(&format!("usage call failed: {e}"));
                UsageAnswer::Failed { status: 0, retry_after_ms: None }
            }
        }
    }

    async fn fetch_profile(&self, access_token: &str) -> Option<Value> {
        match self.oauth_get(PROFILE_URL, access_token).await {
            Ok(res) if res.status().is_success() => res.json().await.ok(),
            Ok(_) => None,
            Err(e) => {
                self.log(&format!("profile call failed: {e}"));
                None
            }
        }
    }

    async fn auth_status(&self, slot: &str) -> Option<Identity> {
        let out = run(
            &self.claude,
            &["auth", "status", "--json"],
            &with_slot(&self.env, slot),
            Duration::from_secs(20),
            None,
        )
        .await;
        if out.code == 0 { identity_from_status(&out.stdout) } else { None }
    }

    async fn refresh_token(&self, slot: &str) -> bool {
        // `/usage` runs in Claude Code itself, with no model request: reading usage refreshes an expired token first
        // and saves it where the login is kept. The rotation plugin in that run stays out (CLAUDE_ACCOUNT_ROTATOR_REFRESH).
        let mut env = with_slot(&self.env, slot);
        env.push(("CLAUDE_ACCOUNT_ROTATOR_REFRESH".into(), "1".into()));
        let tmp = std::env::temp_dir();
        let out = run(
            &self.claude,
            &["-p", "/usage", "--strict-mcp-config", "--no-session-persistence"],
            &env,
            Duration::from_secs(90),
            Some(&tmp),
        )
        .await;
        if out.code != 0 {
            let who = if slot.is_empty() { "the normal login" } else { slot };
            self.log(&format!(
                "refresh run for {who} exited {}: {}",
                out.code,
                out.stderr.trim().chars().take(200).collect::<String>()
            ));
        }
        out.code == 0 && out.stdout.contains("used")
    }

    fn sign_in(&self, slot: &str, email: Option<&str>) -> SignInRun {
        let mut cmd = Command::new(&self.claude);
        cmd.args(["auth", "login", "--claudeai"]);
        if let Some(email) = email {
            cmd.args(["--email", email]);
        }
        cmd.env_clear()
            .envs(with_slot(&self.env, slot))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let done = async move {
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => return (None, e.to_string()),
            };
            let mut stderr = child.stderr.take();
            let read = async move {
                let mut text = String::new();
                if let Some(s) = stderr.as_mut() {
                    let _ = s.read_to_string(&mut text).await;
                }
                text
            };
            let mut cancel_rx = cancel_rx;
            let wait = async {
                tokio::select! {
                    status = child.wait() => status.ok().and_then(|s| s.code()),
                    _ = async { let _ = cancel_rx.wait_for(|c| *c).await; } => {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        None
                    }
                }
            };
            let (code, errors) = tokio::join!(wait, read);
            (code, errors)
        }
        .boxed();
        SignInRun {
            done,
            cancel: Arc::new(move || {
                let _ = cancel_tx.send(true);
            }),
        }
    }

    async fn sign_out(&self, slot: &str) {
        let out =
            run(&self.claude, &["auth", "logout"], &with_slot(&self.env, slot), Duration::from_secs(20), None).await;
        if out.code != 0 {
            self.log(&format!("signing out {slot} failed: {}", out.stderr.trim()));
        }
    }

    async fn make_slot(&self, id: &str) -> Result<String, String> {
        use std::os::unix::fs::DirBuilderExt;
        let slot = self.home.join(".claude").join("accounts").join(id);
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&slot).map_err(|e| e.to_string())?;
        std::fs::write(slot.join("README"), format!("Claude account {id}, kept by the account-rotator mod.\n"))
            .map_err(|e| e.to_string())?;
        Ok(slot.to_string_lossy().into_owned())
    }

    fn is_alive(&self, pid: i64) -> bool {
        let Ok(pid) = i32::try_from(pid) else {
            return false;
        };
        if pid <= 0 {
            return false;
        }
        match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
            Ok(()) => true,
            // EPERM: it runs, as another user.
            Err(e) => e == nix::errno::Errno::EPERM,
        }
    }

    fn save(&self, persisted: &Persisted) {
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(persisted.clone());
        let mut is_saving = self.is_saving.lock().unwrap_or_else(|e| e.into_inner());
        if *is_saving {
            return;
        }
        *is_saving = true;
        let (pending, flag, file, logger) =
            (self.pending.clone(), self.is_saving.clone(), self.state_file.clone(), self.logger.clone());
        // Many changes in a burst are written once, half a second after the first.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            *flag.lock().unwrap_or_else(|e| e.into_inner()) = false;
            let p = pending.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(p) = p {
                tokio::task::spawn_blocking(move || write_state(&file, &p, &logger));
            }
        });
    }

    fn log(&self, text: &str) {
        self.logger.log(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_reads_seconds_and_dates() {
        let now = parse_iso("2026-11-09T10:00:00Z").unwrap_or_default();
        assert_eq!(retry_after(Some("120"), now), Some(120_000));
        assert_eq!(retry_after(Some("Mon, 09 Nov 2026 10:01:00 GMT"), now), Some(60_000));
        assert_eq!(retry_after(Some("soon"), now), None);
        assert_eq!(retry_after(None, now), None);
    }
}
