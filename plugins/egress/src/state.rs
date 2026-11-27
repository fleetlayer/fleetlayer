//! What the proxy remembers between restarts: hosts a person approved for a role (so they stay allowed even while the
//! control plane is out of reach) and requests still waiting for an answer.

use std::collections::BTreeMap;
use std::path::Path;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approved {
    /// The access-control target, `host:<name>[:<port>]`.
    pub target: String,
    pub checked_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub target: String,
    /// The approval request's ID (`ap_…`), when the control plane gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub why: String,
    pub asked_at: Timestamp,
    pub checked_at: Timestamp,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleState {
    /// By `host:<name>[:<port>]`.
    #[serde(default)]
    pub approved: BTreeMap<String, Approved>,
    #[serde(default)]
    pub pending: BTreeMap<String, Pending>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub roles: BTreeMap<String, RoleState>,
    /// When the proxy last wrote this (the CLI warns when it's old: the plugin may not be running).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

impl State {
    pub fn load(path: &Path) -> State {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        write_atomic(path, &serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)
    }
}

/// Write a file by renaming a complete copy over it, readable by its owner only.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.tmp", path.file_name().and_then(|n| n.to_str()).unwrap_or("state")));
    let mut f = std::fs::File::create(&tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// The approval request ID in a refusal ("waiting for the person to approve request ap_…").
pub fn approval_id(text: &str) -> Option<String> {
    let at = text.find("ap_")?;
    let id: String = text[at..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
    (id.len() > 3).then_some(id)
}
