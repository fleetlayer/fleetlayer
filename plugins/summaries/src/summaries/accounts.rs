//! Pick the Claude account for a headless call from the rotator's coordinator (`GET /state` on its Unix socket).
//!
//! The rule is the coordinator's own "soonest reset": of the signed-in accounts that aren't spent (the coordinator
//! marks an account spent at 98% of its week), use the one whose week resets first, so room that's about to refill is
//! used before it does. Summaries run on Sonnet, so only the account's week counts, not Fable's own week.
//! Ported from claude-summarizer's `accounts.py`. The node serves the same API on the same socket once it takes the
//! usage monitor over from the coordinator.

/// Skip an account once its weekly window is this full (percent), as the account rotator leaves it.
pub const LEAVE_SEVEN_DAY: f64 = 98.0;

use std::collections::BTreeMap;
use std::path::Path;

use jiff::{SignedDuration, Timestamp};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, Clone, PartialEq)]
pub struct Account {
    pub id: String,
    /// The account's config directory; empty for Claude Code's own login.
    pub slot: String,
    pub email: String,
    pub week_pct: f64,
    pub resets_at: Option<Timestamp>,
}

impl Account {
    /// Claude Code's own login, used when no coordinator is installed.
    pub fn default_login() -> Self {
        Account {
            id: "default".into(),
            slot: String::new(),
            email: "Claude Code's own login".into(),
            week_pct: 0.0,
            resets_at: None,
        }
    }

    /// The same variables the coordinator uses to point a `claude` child at a login (its `slotEnv`): an added account
    /// keeps its config in its slot too, so a run never touches ~/.claude.json's record of Claude Code's own login.
    /// CLAUDE_ACCOUNT_ROTATOR_REFRESH keeps the rotator mod out of the run (it otherwise registers the run as a session
    /// and may move it to another account); `--setting-sources ""` keeps plugins, hooks and CLAUDE.md out as well.
    pub fn env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::from([
            ("CLAUDE_SECURESTORAGE_CONFIG_DIR".to_owned(), self.slot.clone()),
            ("CLAUDE_ACCOUNT_ROTATOR_REFRESH".to_owned(), "1".to_owned()),
        ]);
        if !self.slot.is_empty() {
            env.insert("CLAUDE_CONFIG_DIR".into(), self.slot.clone());
        }
        env
    }

    pub fn label(&self) -> String {
        if self.id == "default" {
            self.email.clone()
        } else {
            format!("account {} ({}, week {:.0}%)", self.id, self.email, self.week_pct)
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("coordinator socket: {0}")]
    Io(#[from] std::io::Error),
    #[error("coordinator /state answered {0}")]
    Status(String),
    #[error("coordinator /state: {0}")]
    Json(#[from] serde_json::Error),
}

/// `GET /state` over the coordinator's Unix socket.
pub async fn coordinator_state(socket: &Path) -> Result<Value, StateError> {
    let fut = async {
        let mut stream = tokio::net::UnixStream::connect(socket).await?;
        stream.write_all(b"GET /state HTTP/1.1\r\nHost: coordinator\r\nConnection: close\r\n\r\n").await?;
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await?;
        Ok::<_, StateError>(buf)
    };
    let buf = tokio::time::timeout(std::time::Duration::from_secs(10), fut)
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "no answer in 10 s"))??;
    let split =
        buf.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| StateError::Status("no HTTP headers".into()))?;
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let mut body = buf[split + 4..].to_vec();
    let status = head.lines().next().unwrap_or("");
    if status.split_whitespace().nth(1) != Some("200") {
        return Err(StateError::Status(status.to_owned()));
    }
    if head
        .lines()
        .any(|l| l.to_ascii_lowercase().starts_with("transfer-encoding:") && l.to_ascii_lowercase().contains("chunked"))
    {
        body = dechunk(&body).ok_or_else(|| StateError::Status("bad chunked body".into()))?;
    }
    Ok(serde_json::from_slice(&body)?)
}

fn dechunk(mut body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = body.windows(2).position(|w| w == b"\r\n")?;
        let size_text = std::str::from_utf8(&body[..line_end]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(body.get(..size)?);
        body = body.get(size + 2..)?;
    }
}

/// None when every account is spent.
pub fn choose(state: &Value, now: Timestamp) -> Option<Account> {
    let mut usable = Vec::new();
    for a in state.get("accounts").and_then(Value::as_array).into_iter().flatten() {
        let healthy = a.get("health").and_then(|h| h.get("kind")).and_then(Value::as_str) == Some("ok");
        let flag = |k: &str| a.get(k).and_then(Value::as_bool).unwrap_or(false);
        if !healthy || flag("isSpent") || flag("isRetiring") {
            continue;
        }
        let Some(week) = a.get("reading").and_then(|r| r.get("sevenDay")).filter(|w| !w.is_null()) else {
            continue;
        };
        let mut resets = week.get("resetsAt").and_then(Value::as_str).and_then(|s| s.parse::<Timestamp>().ok());
        let mut pct = week.get("pct").and_then(Value::as_f64).unwrap_or(0.0);
        if resets.is_some_and(|r| r <= now) {
            pct = 0.0;
            resets = None; // reset already; its next reset is a week out, which we don't know
        }
        // Spent by its reading, even when the coordinator hasn't flagged it yet (or never will: a full account ran
        // every summary on the Mac, 25 Nov 2026).
        if pct >= LEAVE_SEVEN_DAY {
            continue;
        }
        usable.push(Account {
            id: match a.get("id") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => continue,
            },
            slot: a.get("slot").and_then(Value::as_str).unwrap_or("").to_owned(),
            email: a.get("email").and_then(Value::as_str).unwrap_or("?").to_owned(),
            week_pct: pct,
            resets_at: resets,
        });
    }
    let week_out = now + SignedDuration::from_hours(7 * 24);
    usable.sort_by(|a, b| {
        (a.resets_at.unwrap_or(week_out), a.week_pct)
            .partial_cmp(&(b.resets_at.unwrap_or(week_out), b.week_pct))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    usable.into_iter().next()
}

/// What a call should use: the coordinator's choice, Claude Code's own login when no coordinator is installed, or
/// None when every account is spent (summaries wait for the next idle event or the sweep).
pub async fn pick(socket: &Path, now: Timestamp) -> Result<Option<Account>, StateError> {
    if !socket.exists() {
        return Ok(Some(Account::default_login()));
    }
    Ok(choose(&coordinator_state(socket).await?, now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn acct(id: &str, pct: f64, resets: &str) -> Value {
        json!({"id": id, "slot": format!("/slots/{id}"), "email": format!("{id}@x"), "health": {"kind": "ok"},
               "isSpent": false, "isRetiring": false, "reading": {"sevenDay": {"pct": pct, "resetsAt": resets}}})
    }

    #[test]
    fn soonest_reset_wins_and_spent_accounts_are_skipped() {
        let now: Timestamp = "2026-11-23T00:00:00Z".parse().unwrap();
        let mut spent = acct("1", 99.0, "2026-11-23T01:00:00Z");
        spent["isSpent"] = json!(true);
        let mut sick = acct("2", 1.0, "2026-11-23T02:00:00Z");
        sick["health"] = json!({"kind": "signed_out"});
        let mut retiring = acct("6", 1.0, "2026-11-23T02:00:00Z");
        retiring["isRetiring"] = json!(true);
        let state = json!({"accounts": [spent, sick, retiring, acct("3", 80.0, "2026-11-24T00:00:00Z"), acct("4", 10.0, "2026-11-26T00:00:00Z"), acct("5", 5.0, "2026-11-26T00:00:00Z")]});
        let a = choose(&state, now).unwrap();
        assert_eq!((a.id.as_str(), a.slot.as_str()), ("3", "/slots/3"));
        assert_eq!(a.label(), "account 3 (3@x, week 80%)");
        // Ties on reset time go to the emptier week.
        let state =
            json!({"accounts": [acct("4", 10.0, "2026-11-26T00:00:00Z"), acct("5", 5.0, "2026-11-26T00:00:00Z")]});
        assert_eq!(choose(&state, now).unwrap().id, "5");
    }

    #[test]
    fn a_week_that_already_reset_counts_as_empty_and_a_week_out() {
        let now: Timestamp = "2026-11-23T00:00:00Z".parse().unwrap();
        let state =
            json!({"accounts": [acct("1", 97.0, "2026-11-22T00:00:00Z"), acct("2", 50.0, "2026-11-29T00:00:00Z")]});
        let a = choose(&state, now).unwrap();
        assert_eq!(
            (a.id.as_str(), a.week_pct, a.resets_at),
            ("2", 50.0, Some("2026-11-29T00:00:00Z".parse().unwrap()))
        );
        let state =
            json!({"accounts": [acct("1", 97.0, "2026-11-22T00:00:00Z"), acct("2", 50.0, "2026-12-01T00:00:00Z")]});
        let a = choose(&state, now).unwrap();
        assert_eq!((a.id.as_str(), a.week_pct, a.resets_at), ("1", 0.0, None));
    }

    #[test]
    fn none_when_all_spent_and_env_matches_the_coordinator() {
        let now = Timestamp::now();
        let mut spent = acct("1", 99.0, "2030-01-01T00:00:00Z");
        spent["isSpent"] = json!(true);
        assert_eq!(choose(&json!({"accounts": [spent]}), now), None);
        let full = acct("9", 100.0, "2099-01-01T00:00:00Z");
        assert_eq!(choose(&json!({"accounts": [full]}), now), None);
        assert_eq!(choose(&json!({}), now), None);
        let env = Account::default_login().env();
        assert_eq!(env.get("CLAUDE_SECURESTORAGE_CONFIG_DIR").map(String::as_str), Some(""));
        assert!(!env.contains_key("CLAUDE_CONFIG_DIR"));
        let a = choose(&json!({"accounts": [acct("3", 1.0, "2030-01-01T00:00:00Z")]}), now).unwrap();
        assert_eq!(a.env().get("CLAUDE_CONFIG_DIR").map(String::as_str), Some("/slots/3"));
        assert_eq!(a.env().get("CLAUDE_ACCOUNT_ROTATOR_REFRESH").map(String::as_str), Some("1"));
    }

    #[tokio::test]
    async fn reads_state_from_a_unix_socket_with_a_chunked_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coordinator.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut req = [0u8; 1024];
            let _ = s.read(&mut req).await.unwrap();
            let body = r#"{"accounts": []}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n{:x}\r\n{}\r\n0\r\n\r\n",
                5,
                &body[..5],
                body.len() - 5,
                &body[5..]
            );
            s.write_all(resp.as_bytes()).await.unwrap();
        });
        let state = coordinator_state(&path).await.unwrap();
        assert_eq!(state, json!({"accounts": []}));
        assert_eq!(
            pick(&dir.path().join("missing.sock"), Timestamp::now()).await.unwrap(),
            Some(Account::default_login())
        );
    }
}
