//! The module's own state on the node: byte offsets per transcript, and each session's latest page (the next call
//! rewrites it, so it's kept here rather than fetched from the control plane). One JSON file, written atomically.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use fleetlayer_protocol::summary::Catchup;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FileState {
    pub offset: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip: Option<String>,
    /// The chunk end that failed, and how many runs in a row it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_at: Option<u64>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub failures: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    #[serde(default)]
    pub page: String,
    /// The hour the page was written under, for the prompt's "Current page (written …)".
    #[serde(default)]
    pub page_hour: String,
    #[serde(default)]
    pub last_activity: Option<Timestamp>,
    #[serde(default)]
    pub transcripts: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub files: BTreeMap<PathBuf, FileState>,
    #[serde(default)]
    pub sessions: BTreeMap<String, SessionState>,
    #[serde(default)]
    pub catchups: BTreeMap<String, Catchup>,
    #[serde(default)]
    pub last_sweep: Option<Timestamp>,
    /// When claude-summarizer's history was imported (once).
    #[serde(default)]
    pub imported_at: Option<Timestamp>,
    /// When claude-summarizer's offsets and latest pages were adopted at start-up (without sending its records).
    #[serde(default)]
    pub seeded_at: Option<Timestamp>,
}

impl State {
    pub fn load(path: &Path) -> std::io::Result<State> {
        match std::fs::read(path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).expect("state serialises"))?;
        std::fs::rename(&tmp, path)
    }
}
