//! The registry of secrets delivered to this node: `~/.local/state/fleetlayer/secrets/<source>.json`, one file per
//! plugin that writes them, mode 0600 in a 0700 folder. It holds values, so it's exactly as private as the files the
//! plugins write for tools: readable by the node's Unix user and nobody else. It exists so the node and the
//! `fleetlayer` command know every value to redact and every reference they may fill in, whichever plugin brought it.

use std::path::{Path, PathBuf};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// One delivered value and the reference it's known by (`op://vault/item/field`, `cookie://x/auth_token`,
/// `claude://account/2`), which is what redaction shows in its place.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Secret {
    #[serde(rename = "ref")]
    pub reference: String,
    pub value: String,
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secret").field("ref", &self.reference).field("value", &"…").finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct File {
    source: String,
    updated_at: Timestamp,
    secrets: Vec<Secret>,
}

pub fn dir(home: &Path) -> PathBuf {
    home.join(".local/state/fleetlayer/secrets")
}

fn source_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Make `source`'s list match `secrets` (no file when it's empty).
pub fn register(home: &Path, source: &str, secrets: &[Secret]) -> std::io::Result<()> {
    if !source_ok(source) {
        return Err(std::io::Error::other(format!("{source:?} isn't a source name (a-z, 0-9, - and _)")));
    }
    let path = dir(home).join(format!("{source}.json"));
    if secrets.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let f = File { source: source.into(), updated_at: Timestamp::now(), secrets: secrets.to_vec() };
    crate::write::atomic(&path, &serde_json::to_string(&f).map_err(std::io::Error::other)?)
}

/// Every source's secrets, and a line for each file that couldn't be read.
pub fn load(home: &Path) -> (Vec<Secret>, Vec<String>) {
    let mut out = vec![];
    let mut errors = vec![];
    let Ok(entries) = std::fs::read_dir(dir(home)) else { return (out, errors) };
    let mut paths: Vec<PathBuf> =
        entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
    paths.sort();
    for p in paths {
        match std::fs::read(&p)
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice::<File>(&b).map_err(|e| e.to_string()))
        {
            Ok(f) => out.extend(f.secrets),
            Err(e) => errors.push(format!("{}: {e}", p.display())),
        }
    }
    (out, errors)
}

/// A cheap fingerprint of the folder (names, sizes and modification times), to notice a change without reading it.
pub fn stamp(home: &Path) -> Vec<(String, u64, Option<std::time::SystemTime>)> {
    let mut v: Vec<_> = std::fs::read_dir(dir(home))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((e.file_name().to_string_lossy().to_string(), m.len(), m.modified().ok()))
        })
        .collect();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_register_load_and_clear() {
        let d = tempfile::tempdir().unwrap();
        let s = |r: &str, v: &str| Secret { reference: r.into(), value: v.into() };
        register(d.path(), "1password", &[s("op://a/b/c", "v1")]).unwrap();
        register(d.path(), "cookies", &[s("cookie://x/ct0", "v2")]).unwrap();
        let (all, errors) = load(d.path());
        assert!(errors.is_empty());
        assert_eq!(all, vec![s("op://a/b/c", "v1"), s("cookie://x/ct0", "v2")]);
        use std::os::unix::fs::PermissionsExt;
        let f = dir(d.path()).join("1password.json");
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        register(d.path(), "1password", &[]).unwrap();
        assert!(!f.exists());
        assert!(register(d.path(), "../x", &[]).is_err());
        assert_eq!(format!("{:?}", s("op://a/b/c", "hunter2hunter2")), r#"Secret { ref: "op://a/b/c", value: "…" }"#);
    }
}
