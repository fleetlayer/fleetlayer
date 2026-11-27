//! `claude.token_health`: whether each managed slot's token still works, by running Claude Code from it. Applying a
//! bundle only says the write worked; this says Claude Code accepts the token, and catches a Claude Code upgrade that
//! changes where it reads slots.
//!
//! Each check is one `claude -p` from the slot (a few seconds and one small request). A slot is checked when it's new
//! or its token changed, after each Claude Code upgrade, a day after its last check, and an hour after a failed one.
//! The reading is keyed by account UUID, so the control plane can alert on a failure and on a reading that stops
//! coming (the plugin or the node stopped).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};

use crate::claude::{OVERRIDING_ENV, sha256_hex};
use crate::slots::Managed;

pub const KIND: &str = "claude.token_health";
const DAY_MS: i64 = 24 * 3600 * 1000;
const RETRY_MS: i64 = 3600 * 1000;
const TIMEOUT: Duration = Duration::from_secs(120);

/// How one check went.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub ok: bool,
    pub error: Option<String>,
    /// The check couldn't run on this machine (Claude Code is missing or won't start): it says nothing about the
    /// token, so it's reported as the plugin's own problem and leaves the account alone.
    pub local: bool,
}

/// The last check of a slot.
#[derive(Debug, Clone)]
pub struct Last {
    pub at_ms: i64,
    pub version: String,
    /// A digest of the stored credentials, to see a replaced token.
    pub fingerprint: String,
    pub ok: bool,
}

/// Whether a slot is due a check.
pub fn due(last: Option<&Last>, now_ms: i64, version: &str, fingerprint: &str) -> bool {
    match last {
        None => true,
        Some(l) => {
            l.version != version
                || l.fingerprint != fingerprint
                || now_ms - l.at_ms >= DAY_MS
                || (!l.ok && now_ms - l.at_ms >= RETRY_MS)
        }
    }
}

pub fn fingerprint(stored: &str) -> String {
    sha256_hex(stored)[..16].to_string()
}

/// The reading for one check.
pub fn reading(
    m: &Managed,
    outcome: &Outcome,
    version: &str,
    at: jiff::Timestamp,
) -> fleetlayer_protocol::ext::Reading {
    let mut data = json!({
        "ok": outcome.ok,
        "email": m.email,
        "claudeVersion": version,
    });
    if let Some(e) = &outcome.error {
        data["error"] = json!(e);
    }
    fleetlayer_protocol::ext::Reading {
        kind: KIND.into(),
        key: if m.account_uuid.is_empty() { m.slot.clone() } else { m.account_uuid.clone() },
        at,
        node: None,
        data,
    }
}

/// Read `claude -p --output-format json`'s answer.
pub fn outcome_of(code: Option<i32>, stdout: &str, stderr: &str) -> Outcome {
    let answer: Option<Value> = stdout.lines().rev().find_map(|l| serde_json::from_str(l).ok());
    let is_error = answer.as_ref().and_then(|a| a["is_error"].as_bool());
    let text = answer.as_ref().and_then(|a| a["result"].as_str()).unwrap_or_default();
    if code == Some(0) && is_error == Some(false) {
        return Outcome { ok: true, error: None, local: false };
    }
    let why = if !text.is_empty() {
        text.to_string()
    } else if !stderr.trim().is_empty() {
        stderr.trim().to_string()
    } else {
        format!("claude exited {}", code.map_or("on a signal".into(), |c| c.to_string()))
    };
    Outcome { ok: false, error: Some(why.chars().take(300).collect()), local: false }
}

/// Runs Claude Code.
pub struct Claude {
    pub bin: String,
}

impl Claude {
    fn command(&self) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(&self.bin);
        for v in OVERRIDING_ENV {
            c.env_remove(v);
        }
        // A check never updates Claude Code: that moves ~/.local/bin/claude, the person's to manage.
        c.env("DISABLE_AUTOUPDATER", "1");
        c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        c
    }

    /// `claude --version`'s number, or why it couldn't be run: a problem on this machine, never a token's.
    pub async fn version(&self) -> Result<String, String> {
        let out = tokio::time::timeout(Duration::from_secs(30), self.command().arg("--version").output())
            .await
            .map_err(|_| format!("{} --version timed out", self.bin))?
            .map_err(|e| self.cant_run(&e))?;
        let text = String::from_utf8_lossy(&out.stdout);
        match text.split_whitespace().next() {
            Some(v) if out.status.success() => Ok(v.to_string()),
            _ => {
                let err = String::from_utf8_lossy(&out.stderr);
                let why = err.trim().lines().next().unwrap_or("it printed nothing");
                Err(format!("{} --version failed: {}", self.bin, why.chars().take(200).collect::<String>()))
            }
        }
    }

    /// One line on why the binary couldn't be started.
    fn cant_run(&self, e: &std::io::Error) -> String {
        if e.kind() == std::io::ErrorKind::NotFound {
            if self.bin.contains('/') {
                format!("claude not found at {}", self.bin)
            } else {
                format!("claude not found: no `{}` on the PATH or in ~/.local/bin", self.bin)
            }
        } else {
            format!("{} won't run: {e}", self.bin)
        }
    }

    /// One request from the slot, with nothing of the person's own setup loaded: no user settings (so no plugins or
    /// hooks, and the rotator can't switch slots under it), no MCP servers, no saved session.
    ///
    /// For a slot directory, Claude Code reads the credentials from `CLAUDE_SECURESTORAGE_CONFIG_DIR` and keeps its
    /// config in `CLAUDE_CONFIG_DIR`, which points at a scratch directory so nothing is left in the slot (tested with
    /// 2.1.332 on macOS and Linux: a broken token in the slot gives the 401, no token gives "Not logged in"). The default
    /// slot keeps Claude Code's own config directory, since that's where its sign-in is found.
    pub async fn check(&self, slot: &str) -> Outcome {
        self.check_with(Some(slot), None).await
    }

    /// The same request with a token given directly (`CLAUDE_CODE_OAUTH_TOKEN`), before it's stored anywhere: the hub
    /// checks each setup token Claude Code prints before keeping or sending it.
    pub async fn check_token(&self, token: &str) -> Outcome {
        self.check_with(None, Some(token)).await
    }

    async fn check_with(&self, slot: Option<&str>, token: Option<&str>) -> Outcome {
        let dir = scratch_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return Outcome { ok: false, error: Some(format!("making {}: {e}", dir.display())), local: true };
        }
        let mut c = self.command();
        c.current_dir(&dir).args([
            "-p",
            "Reply with just the word ok",
            "--model",
            "haiku",
            "--output-format",
            "json",
            "--strict-mcp-config",
            "--no-session-persistence",
            "--setting-sources",
            "project",
        ]);
        match (slot, token) {
            (_, Some(t)) => {
                // Nothing stored is read: the token alone, and a scratch config.
                c.env("CLAUDE_CODE_OAUTH_TOKEN", t)
                    .env("CLAUDE_CONFIG_DIR", &dir)
                    .env("CLAUDE_SECURESTORAGE_CONFIG_DIR", &dir);
            }
            (Some(""), None) => {
                c.env_remove("CLAUDE_SECURESTORAGE_CONFIG_DIR").env_remove("CLAUDE_CONFIG_DIR");
            }
            (Some(s), None) => {
                c.env("CLAUDE_SECURESTORAGE_CONFIG_DIR", s).env("CLAUDE_CONFIG_DIR", &dir);
            }
            (None, None) => {}
        }
        let out = tokio::time::timeout(TIMEOUT, c.output()).await;
        let _ = std::fs::remove_dir_all(&dir);
        match out {
            Err(_) => {
                Outcome { ok: false, error: Some(format!("claude -p took over {}s", TIMEOUT.as_secs())), local: false }
            }
            Ok(Err(e)) => Outcome { ok: false, error: Some(self.cant_run(&e)), local: true },
            Ok(Ok(o)) => {
                outcome_of(o.status.code(), &String::from_utf8_lossy(&o.stdout), &String::from_utf8_lossy(&o.stderr))
            }
        }
    }
}

fn scratch_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "fleetlayer-token-check-{}-{}",
        std::process::id(),
        jiff::Timestamp::now().as_nanosecond()
    ))
}

/// The checks already made, by slot.
pub type History = BTreeMap<String, Last>;

/// What one round of checks found.
#[derive(Debug, Default)]
pub struct Round {
    pub readings: Vec<fleetlayer_protocol::ext::Reading>,
    /// Each account checked, for the coordinator: (account UUID, ok, why).
    pub checked: Vec<(String, bool, Option<String>)>,
    /// A problem on this machine rather than with any account (Claude Code is missing or won't run): the plugin's
    /// status says it, and no account is marked as failing or checked off.
    pub problem: Option<String>,
}

/// Checks the managed slots that are due. `stored` gives a slot's stored credentials' fingerprint.
pub async fn round(
    claude: &Claude,
    managed: &[Managed],
    stored: impl Fn(&str) -> String,
    history: &mut History,
) -> Round {
    let mut r = Round::default();
    if managed.is_empty() {
        return r;
    }
    let version = match claude.version().await {
        Ok(v) => v,
        Err(e) => {
            r.problem = Some(e);
            return r;
        }
    };
    for m in managed {
        let fingerprint = stored(&m.slot);
        let now = jiff::Timestamp::now();
        if !due(history.get(&m.slot), now.as_millisecond(), &version, &fingerprint) {
            continue;
        }
        let outcome = claude.check(&m.slot).await;
        if outcome.local {
            // Not the token's fault, and not checked: it goes again next round.
            r.problem = outcome.error;
            break;
        }
        if outcome.ok {
            tracing::info!(account = %m.email, "token check passed");
        } else {
            tracing::warn!(account = %m.email, error = ?outcome.error, "token check failed");
        }
        r.checked.push((m.account_uuid.clone(), outcome.ok, outcome.error.clone()));
        r.readings.push(reading(m, &outcome, &version, now));
        history.insert(
            m.slot.clone(),
            Last { at_ms: now.as_millisecond(), version: version.clone(), fingerprint, ok: outcome.ok },
        );
    }
    history.retain(|slot, _| managed.iter().any(|m| &m.slot == slot));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last(at_ms: i64, ok: bool) -> Last {
        Last { at_ms, version: "2.1.332".into(), fingerprint: "f".into(), ok }
    }

    #[test]
    fn when_a_slot_is_due() {
        let h = 3600 * 1000;
        assert!(due(None, 0, "2.1.332", "f"));
        assert!(!due(Some(&last(0, true)), 23 * h, "2.1.332", "f"));
        assert!(due(Some(&last(0, true)), 24 * h, "2.1.332", "f"), "daily");
        assert!(due(Some(&last(0, true)), h, "2.1.333", "f"), "after an upgrade");
        assert!(due(Some(&last(0, true)), h, "2.1.332", "g"), "a replaced token");
        assert!(!due(Some(&last(0, false)), h - 1, "2.1.332", "f"));
        assert!(due(Some(&last(0, false)), h, "2.1.332", "f"), "an hour after a failure");
    }

    #[test]
    fn reads_claude_answers() {
        let ok = r#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#;
        assert_eq!(outcome_of(Some(0), ok, ""), Outcome { ok: true, error: None, local: false });
        // A rejected token, as Claude Code 2.1.332 reports it.
        let bad = r#"{"type":"result","subtype":"success","is_error":true,"result":"Failed to authenticate. API Error: 401 OAuth access token is invalid."}"#;
        let o = outcome_of(Some(1), bad, "");
        assert!(!o.ok && o.error.unwrap().contains("401"));
        assert_eq!(outcome_of(Some(127), "", "not found\n").error.as_deref(), Some("not found"));
        assert_eq!(outcome_of(None, "", "").error.as_deref(), Some("claude exited on a signal"));
    }

    #[test]
    fn readings_are_keyed_by_account_and_carry_no_token() {
        let m = Managed {
            number: 2,
            slot: "/h/.claude/accounts/2".into(),
            account_uuid: "uuid-2".into(),
            email: "a@example.com".into(),
        };
        let r = reading(
            &m,
            &Outcome { ok: false, error: Some("401".into()), local: false },
            "2.1.332",
            jiff::Timestamp::UNIX_EPOCH,
        );
        assert_eq!((r.kind.as_str(), r.key.as_str()), (KIND, "uuid-2"));
        assert_eq!(r.data, json!({"ok": false, "email": "a@example.com", "claudeVersion": "2.1.332", "error": "401"}));
    }

    fn managed(n: u32) -> Managed {
        Managed {
            number: n,
            slot: format!("/h/.claude/accounts/{n}"),
            account_uuid: format!("uuid-{n}"),
            email: format!("{n}@example.com"),
        }
    }

    /// A missing Claude Code (a deleted binary, a symlink into a folder a reboot wiped) is one problem on this
    /// machine: no account is marked as failing, nothing is published that would page about tokens, and nothing is
    /// recorded as checked, so each account is checked once Claude Code is back.
    #[tokio::test]
    async fn a_missing_claude_is_a_local_problem_not_failed_tokens() {
        let c = Claude { bin: "/nonexistent/claude".into() };
        assert_eq!(c.version().await, Err("claude not found at /nonexistent/claude".into()));
        let o = c.check("/tmp/none").await;
        assert!(!o.ok && o.local);
        let mut history = History::new();
        let r = round(&c, &[managed(1), managed(2)], |_| "f".into(), &mut history).await;
        assert_eq!(r.problem.as_deref(), Some("claude not found at /nonexistent/claude"));
        assert!(r.readings.is_empty() && r.checked.is_empty() && history.is_empty());

        let bare = Claude { bin: "fleetlayer-no-such-claude".into() };
        assert!(bare.version().await.unwrap_err().starts_with("claude not found: no `fleetlayer-no-such-claude`"));
    }

    /// A binary that's there but broken (not executable, or `--version` fails) is local too; a token Claude Code
    /// rejects is still the account's.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_broken_claude_is_local_and_a_rejected_token_is_not() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("fleetlayer-health-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, body: &str, mode: u32| {
            let p = dir.join(name);
            std::fs::write(&p, body).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
            p.display().to_string()
        };
        let not_exec = Claude { bin: write("noexec", "#!/bin/sh\n", 0o644) };
        let mut history = History::new();
        let r = round(&not_exec, &[managed(1)], |_| "f".into(), &mut history).await;
        assert!(r.problem.unwrap().contains("won't run"));
        assert!(r.checked.is_empty() && r.readings.is_empty());

        let failing = Claude { bin: write("failing", "#!/bin/sh\necho 'cannot load' >&2\nexit 1\n", 0o755) };
        assert!(failing.version().await.unwrap_err().ends_with("--version failed: cannot load"));

        let rejecting = Claude {
            bin: write(
                "rejecting",
                "#!/bin/sh\n[ \"$1\" = --version ] && { echo '2.1.332 (Claude Code)'; exit 0; }\n\
                 echo '{\"type\":\"result\",\"is_error\":true,\"result\":\"API Error: 401\"}'\nexit 1\n",
                0o755,
            ),
        };
        let r = round(&rejecting, &[managed(1)], |_| "f".into(), &mut history).await;
        assert_eq!(r.problem, None);
        assert_eq!(r.checked, vec![("uuid-1".to_string(), false, Some("API Error: 401".to_string()))]);
        assert_eq!(r.readings.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
