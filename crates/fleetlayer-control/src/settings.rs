//! The control plane's settings file, `control.yaml` (`fleetlayer up` writes it to `~/.config/fleetlayer/`): every
//! setting in one file, so a move copies it with the state folder. Paths may start with `~/`.
//!
//! ```yaml
//! db: ~/.local/state/fleetlayer/control/control.db
//! listen: 127.0.0.1:7600
//! hooks_listen: 127.0.0.1:7601
//! admin_token_file: ~/.local/state/fleetlayer/control/admin-token
//! audit_key_file: ~/.local/state/fleetlayer/control/audit-key
//! audit_anchor_dir: ~/audit-anchors      # optional
//! policy_files: [~/fleet/policy.yaml]    # optional, as --policy-file
//! handoff_days: 30                       # optional: how long handoffs and their notes are kept
//! ```

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::policy_files;

/// The file `fleetlayer move control` leaves next to the database before it stops this copy: the new link URL and
/// the move's proof, one per line. Never copied with the folder.
pub const MOVED_TO: &str = "moved-to";
/// The move's proof, written by `fleetlayer move control` into this folder before the copy, so it travels with the
/// database: `{"nonce": "…", "confirmed": false}`. The new copy answers for it at `/v1/move-proof`, so a node can check
/// the address it was told about holds this control plane's state before it sends its token there; `confirmed` turns
/// true once the move passed its health check, and only then do nodes save the new address.
pub const PROOF: &str = "move-proof";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    db: Option<String>,
    listen: Option<SocketAddr>,
    hooks_listen: Option<SocketAddr>,
    admin_token_file: Option<String>,
    audit_key_file: Option<String>,
    audit_anchor_dir: Option<String>,
    #[serde(default)]
    policy_files: Vec<String>,
    handoff_days: Option<u32>,
}

#[derive(Debug, Default, Clone)]
pub struct Settings {
    pub db: Option<PathBuf>,
    pub listen: Option<SocketAddr>,
    pub hooks_listen: Option<SocketAddr>,
    pub admin_token_file: Option<PathBuf>,
    pub audit_key_file: Option<PathBuf>,
    pub audit_anchor_dir: Option<PathBuf>,
    pub policy_files: Vec<policy_files::Source>,
    /// How long handoffs and their notes are kept, in days (30 when absent).
    pub handoff_days: Option<u32>,
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(p),
    }
}

/// `~/.config/fleetlayer/control.yaml` (or under `$XDG_CONFIG_HOME`).
pub fn default_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("fleetlayer/control.yaml")
}

impl Settings {
    /// The named file (which must exist), else the default one when it exists and no --db was given, else nothing.
    pub fn load(named: Option<&Path>, db_given: bool) -> anyhow::Result<Settings> {
        let path = match named {
            Some(p) => p.to_path_buf(),
            None if !db_given && default_path().is_file() => default_path(),
            None => return Ok(Settings::default()),
        };
        let text = std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
    }

    pub fn parse(text: &str) -> anyhow::Result<Settings> {
        let f: File = if text.trim().is_empty() { File::default() } else { yaml_serde::from_str(text)? };
        let mut policy = vec![];
        for p in &f.policy_files {
            let mut s: policy_files::Source = p.parse().map_err(|e: String| anyhow::anyhow!(e))?;
            s.path = expand(&s.path.display().to_string());
            policy.push(s);
        }
        Ok(Settings {
            db: f.db.as_deref().map(expand),
            listen: f.listen,
            hooks_listen: f.hooks_listen,
            admin_token_file: f.admin_token_file.as_deref().map(expand),
            audit_key_file: f.audit_key_file.as_deref().map(expand),
            audit_anchor_dir: f.audit_anchor_dir.as_deref().map(expand),
            policy_files: policy,
            handoff_days: f.handoff_days,
        })
    }
}

/// The new address and proof in a `moved-to` file: a `ws://` or `wss://` URL and a nonce, or nothing.
pub fn read_moved_to(path: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines().map(str::trim);
    let url = lines.next()?.to_string();
    let nonce = lines.next().unwrap_or("").to_string();
    ((url.starts_with("ws://") || url.starts_with("wss://")) && nonce.len() >= 32).then_some((url, nonce))
}

/// Whether `nonce` is this folder's move proof, and if so whether the move is confirmed.
pub fn proof(path: &Path, nonce: &str) -> Option<bool> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let mine = v["nonce"].as_str()?;
    (mine.len() >= 32 && mine == nonce).then(|| v["confirmed"] == true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_settings_file_names_everything_serve_takes() {
        let s = Settings::parse(
            "db: /s/control.db\nlisten: 10.0.0.5:7600\nhooks_listen: 127.0.0.1:7601\nadmin_token_file: /s/admin-token\naudit_key_file: /s/audit-key\npolicy_files: [acme=/p/acme.yaml, /p/first.yaml]\nhandoff_days: 14\n",
        )
        .unwrap();
        assert_eq!(s.db.as_deref(), Some(Path::new("/s/control.db")));
        assert_eq!(s.listen, Some("10.0.0.5:7600".parse().unwrap()));
        assert_eq!(s.admin_token_file.as_deref(), Some(Path::new("/s/admin-token")));
        assert_eq!(s.policy_files.len(), 2);
        assert_eq!(s.handoff_days, Some(14));
        assert_eq!(s.policy_files[0].tenant.as_deref(), Some("acme"));
        assert!(Settings::parse("").unwrap().db.is_none());
        assert!(Settings::parse("lisen: 1.2.3.4:1\n").is_err(), "a misspelt setting is refused");
    }

    #[test]
    fn moved_to_holds_a_link_url_and_a_proof_or_nothing() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join(MOVED_TO);
        assert_eq!(read_moved_to(&f), None);
        let nonce = "n".repeat(32);
        std::fs::write(&f, format!("wss://control.example/v1/link\n{nonce}\n")).unwrap();
        assert_eq!(read_moved_to(&f), Some(("wss://control.example/v1/link".into(), nonce.clone())));
        std::fs::write(&f, "wss://control.example/v1/link\n").unwrap();
        assert_eq!(read_moved_to(&f), None, "no proof, no move");
        std::fs::write(&f, format!("http://nope\n{nonce}\n")).unwrap();
        assert_eq!(read_moved_to(&f), None);
        let p = d.path().join(PROOF);
        assert_eq!(proof(&p, &nonce), None);
        std::fs::write(&p, format!("{{\"nonce\":\"{nonce}\",\"confirmed\":false}}")).unwrap();
        assert_eq!(proof(&p, &nonce), Some(false));
        assert_eq!(proof(&p, &"x".repeat(32)), None);
        assert_eq!(proof(&p, ""), None);
        std::fs::write(&p, format!("{{\"nonce\":\"{nonce}\",\"confirmed\":true}}")).unwrap();
        assert_eq!(proof(&p, &nonce), Some(true));
    }
}
