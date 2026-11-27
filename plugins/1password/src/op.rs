//! 1Password through its own command line, `op`, signed in with a service-account token. The token goes to `op` in
//! its environment (`OP_SERVICE_ACCOUNT_TOKEN`), never in its arguments, so it doesn't show in a process list.
//! Values come back on `op`'s standard output and stay in this process's memory.
//!
//! Change detection is cheap: one `op item list` per vault gives every item's `version`, and only an item whose
//! version moved is read again (`op read`).

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;

/// A reference to one field: `op://<vault>/<item>/[<section>/]<field>`, with `?attribute=…` allowed after it. Vault
/// and item are names or IDs, as `op read` takes them.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Reference {
    pub full: String,
    pub vault: String,
    pub item: String,
}

impl Reference {
    pub fn parse(s: &str) -> Result<Reference, String> {
        let bad = || format!("{s:?} isn't a 1Password reference (op://vault/item/field)");
        let rest = s.strip_prefix("op://").ok_or_else(bad)?;
        let path = rest.split('?').next().unwrap_or_default();
        let parts: Vec<&str> = path.split('/').collect();
        if !(3..=4).contains(&parts.len()) || parts.iter().any(|p| p.trim().is_empty()) || s.contains(char::is_control)
        {
            return Err(bad());
        }
        Ok(Reference { full: s.to_string(), vault: parts[0].to_string(), item: parts[1].to_string() })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Vault {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Item {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Why a call failed: the thing isn't there (an item, a vault or a field was removed or renamed), or anything else
/// (no network, a rate limit, a refused token), which says nothing about whether it's there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpError {
    NotFound(String),
    Failed(String),
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpError::NotFound(e) | OpError::Failed(e) => f.write_str(e),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Op {
    pub bin: String,
}

impl Op {
    async fn run(&self, token: &str, args: &[&str]) -> Result<Vec<u8>, OpError> {
        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.args(args)
            .env("OP_SERVICE_ACCOUNT_TOKEN", token)
            // A Connect server's settings would take over from the service account.
            .env_remove("OP_CONNECT_HOST")
            .env_remove("OP_CONNECT_TOKEN")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let out = match tokio::time::timeout(Duration::from_secs(60), cmd.output()).await {
            Err(_) => return Err(OpError::Failed(format!("{} {} took over a minute", self.bin, args[0]))),
            Ok(Err(e)) => {
                return Err(OpError::Failed(format!(
                    "running {} (is 1Password's op command installed?): {e}",
                    self.bin
                )));
            }
            Ok(Ok(o)) => o,
        };
        if out.status.success() {
            return Ok(out.stdout);
        }
        let err = String::from_utf8_lossy(&out.stderr);
        // `[ERROR] 2026/11/28 10:00:00 "x" isn't an item in the "y" vault...`: keep the message, drop the stamp.
        let msg = err
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("no message")
            .trim()
            .trim_start_matches("[ERROR]")
            .trim();
        let msg = match msg.split_once(' ').and_then(|(d, rest)| {
            (d.len() == 10 && d.as_bytes().get(4) == Some(&b'/')).then(|| rest.split_once(' ').map(|x| x.1))
        }) {
            Some(Some(rest)) => rest.to_string(),
            _ => msg.to_string(),
        };
        let lower = msg.to_ascii_lowercase();
        let not_found =
            ["isn't an item", "isn't a vault", "isn't a field", "could not find", "no item found", "not found"]
                .iter()
                .any(|p| lower.contains(p));
        let msg = format!("op {}: {msg}", args[0]);
        Err(if not_found { OpError::NotFound(msg) } else { OpError::Failed(msg) })
    }

    pub async fn vaults(&self, token: &str) -> Result<Vec<Vault>, OpError> {
        let out = self.run(token, &["vault", "list", "--format", "json"]).await?;
        serde_json::from_slice(&out).map_err(|e| OpError::Failed(format!("op vault list: {e}")))
    }

    pub async fn items(&self, token: &str, vault_id: &str) -> Result<Vec<Item>, OpError> {
        let out = self.run(token, &["item", "list", "--vault", vault_id, "--format", "json"]).await?;
        serde_json::from_slice(&out).map_err(|e| OpError::Failed(format!("op item list: {e}")))
    }

    pub async fn read(&self, token: &str, reference: &str) -> Result<String, OpError> {
        let out = self.run(token, &["read", "--no-newline", reference]).await?;
        String::from_utf8(out).map_err(|_| OpError::Failed(format!("{reference} isn't text")))
    }
}

/// Read a token file: one token, whitespace around it ignored.
pub fn read_token(path: &PathBuf) -> Result<String, String> {
    let t = std::fs::read_to_string(path).map_err(|e| format!("reading the token {}: {e}", path.display()))?;
    let t = t.trim().to_string();
    if t.is_empty() {
        return Err(format!("the token file {} is empty", path.display()));
    }
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_parse() {
        let r = Reference::parse("op://github/deploy bot/token").unwrap();
        assert_eq!((r.vault.as_str(), r.item.as_str()), ("github", "deploy bot"));
        assert!(Reference::parse("op://v/i/section/field").is_ok());
        assert!(Reference::parse("op://v/i/f?attribute=otp").is_ok());
        for bad in ["op://v/i", "op://v//f", "http://v/i/f", "op://a/b/c/d/e", "op://v/i/f\n"] {
            assert!(Reference::parse(bad).is_err(), "{bad}");
        }
    }
}
