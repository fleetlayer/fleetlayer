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
        return Outcome { ok: true, error: None };
    }
    let why = if !text.is_empty() {
        text.to_string()
    } else if !stderr.trim().is_empty() {
        stderr.trim().to_string()
    } else {
        format!("claude exited {}", code.map_or("on a signal".into(), |c| c.to_string()))
    };
    Outcome { ok: false, error: Some(why.chars().take(300).collect()) }
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
        c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        c
    }

    /// `claude --version`'s number, or why it couldn't be run.
    pub async fn version(&self) -> Result<String, String> {
        let out = tokio::time::timeout(Duration::from_secs(30), self.command().arg("--version").output())
            .await
            .map_err(|_| format!("{} --version timed out", self.bin))?
            .map_err(|e| format!("running {}: {e}", self.bin))?;
        let text = String::from_utf8_lossy(&out.stdout);
        text.split_whitespace()
            .next()
            .map(str::to_string)
            .ok_or_else(|| format!("{} --version printed nothing", self.bin))
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
            return Outcome { ok: false, error: Some(format!("making {}: {e}", dir.display())) };
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
            Err(_) => Outcome { ok: false, error: Some(format!("claude -p took over {}s", TIMEOUT.as_secs())) },
            Ok(Err(e)) => Outcome { ok: false, error: Some(format!("running {}: {e}", self.bin)) },
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
        assert_eq!(outcome_of(Some(0), ok, ""), Outcome { ok: true, error: None });
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
        let r = reading(&m, &Outcome { ok: false, error: Some("401".into()) }, "2.1.332", jiff::Timestamp::UNIX_EPOCH);
        assert_eq!((r.kind.as_str(), r.key.as_str()), (KIND, "uuid-2"));
        assert_eq!(r.data, json!({"ok": false, "email": "a@example.com", "claudeVersion": "2.1.332", "error": "401"}));
    }

    /// Fails the way a missing binary does, without a real Claude Code.
    #[tokio::test]
    async fn a_missing_claude_is_a_failed_check() {
        let c = Claude { bin: "/nonexistent/claude".into() };
        assert!(c.version().await.is_err());
        let o = c.check("/tmp/none").await;
        assert!(!o.ok && o.error.unwrap().contains("running /nonexistent/claude"));
    }
}
