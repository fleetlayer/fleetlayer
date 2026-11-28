//! What the reader keeps on disk, so a restart loses nothing and repeats nothing:
//!
//! - `outbox.json`: events read from Matrix that the control plane hasn't taken yet. Each is saved here before the sync
//!   position moves past it, and leaves only once the node answers that it was published (or was a repeat).
//! - `position.json`: the sync token to continue from, and each room's newest message time, for filling a gap when
//!   the homeserver sends only the latest part of a busy room's timeline.
//!
//! Every event carries `dedupeKey: matrix:<event ID>`, and the control plane drops a key it has seen on that channel,
//! so an event read twice (a crash between saving the outbox and the position) reaches nobody twice.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use fleetlayer_protocol::ext::Event;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// Events kept for the control plane at most; past this the oldest go, with a warning.
pub const OUTBOX_MAX: usize = 5_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Waiting {
    pub event: Event,
    /// When it was read.
    pub since: Timestamp,
}

pub struct Outbox {
    path: PathBuf,
    items: Vec<Waiting>,
}

impl Outbox {
    pub fn load(dir: &Path) -> Outbox {
        let path = dir.join("outbox.json");
        let items = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Outbox { path, items }
    }

    fn save(&self) {
        if let Err(e) = write_private(&self.path, &serde_json::to_vec(&self.items).expect("events serialise")) {
            tracing::error!(error = %e, path = %self.path.display(), "saving the outbox");
        }
    }

    /// Add events (a repeat of one waiting is dropped) and save, before the sync position moves on.
    pub fn push(&mut self, events: Vec<Event>) {
        if events.is_empty() {
            return;
        }
        for e in events {
            if self.items.iter().any(|w| w.event.channel == e.channel && w.event.dedupe_key == e.dedupe_key) {
                continue;
            }
            self.items.push(Waiting { event: e, since: Timestamp::now() });
        }
        if self.items.len() > OUTBOX_MAX {
            let drop = self.items.len() - OUTBOX_MAX;
            tracing::error!(dropped = drop, "the outbox is full: dropping the oldest events");
            self.items.drain(..drop);
        }
        self.save();
    }

    pub fn first(&self) -> Option<&Waiting> {
        self.items.first()
    }

    /// The first event was taken: remove it.
    pub fn pop(&mut self) {
        if !self.items.is_empty() {
            self.items.remove(0);
            self.save();
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    /// The `next_batch` of the last sync whose events are all in the outbox.
    pub since: Option<String>,
    /// Each room's newest message time seen (milliseconds), by room ID.
    #[serde(default)]
    pub newest: BTreeMap<String, i64>,
}

impl Position {
    pub fn load(dir: &Path) -> Position {
        std::fs::read(dir.join("position.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        write_private(&dir.join("position.json"), &serde_json::to_vec(self).expect("positions serialise"))
    }
}

pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write a file only this user can read, in one rename.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: &str) -> Event {
        Event {
            channel: "alerts".into(),
            kind: "matrix.message".into(),
            subject: None,
            at: Timestamp::now(),
            dedupe_key: Some(format!("matrix:{id}")),
            summary: "disk full".into(),
            body: serde_json::Value::Null,
        }
    }

    #[test]
    fn the_outbox_keeps_what_wasnt_taken_across_a_restart_once_each() {
        let dir = tempfile::tempdir().unwrap();
        let mut o = Outbox::load(dir.path());
        o.push(vec![ev("$1"), ev("$1"), ev("$2")]);
        o.push(vec![ev("$2")]);
        assert_eq!(o.len(), 2, "repeats are dropped");
        let mut o = Outbox::load(dir.path());
        assert_eq!(o.len(), 2, "kept on disk");
        assert_eq!(o.first().unwrap().event.dedupe_key.as_deref(), Some("matrix:$1"));
        o.pop();
        let o = Outbox::load(dir.path());
        assert_eq!(o.len(), 1);
        assert_eq!(o.first().unwrap().event.dedupe_key.as_deref(), Some("matrix:$2"));
    }

    #[test]
    fn the_position_is_saved_privately() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Position::load(dir.path()), Position::default());
        let mut p = Position { since: Some("s1".into()), ..Default::default() };
        p.newest.insert("!r:example.com".into(), 5);
        p.save(dir.path()).unwrap();
        assert_eq!(Position::load(dir.path()), p);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("position.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0);
        }
    }
}
