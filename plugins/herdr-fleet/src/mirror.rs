//! Other machines' Claude Code transcripts, copied here for clients that read transcript files themselves: herdr's
//! phone relay's Conversation view, pointed at this folder with `HERDR_CLAUDE_CONFIG_DIRS=<state_dir>/claude`.
//!
//! The copy is laid out as Claude Code lays out its own: `<root>/projects/<the session's folder, every character that
//! isn't a letter or digit as '-'>/<session id>.jsonl`. A session seen for the first time gets the end of its
//! transcript (`tail_bytes`, from a line start); after that only what's appended, while it works and once after each
//! change of status. A copy goes when its session ends. Reads go through the node (`_fleetlayer.dev/transcript/read`,
//! grant `transcript.read`), which the access policy checks as `read` on the session: without the grant, nothing is
//! copied.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::fleet::Remote;

/// Part of a remote transcript, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub offset: u64,
    pub size: u64,
    pub bytes: Vec<u8>,
}

/// Why a read failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// The node doesn't grant this plugin transcript reads: mirroring is off.
    NotGranted(String),
    Other(String),
}

#[async_trait]
pub trait Reader: Send + Sync {
    async fn read(&self, address: &str, offset: Option<u64>, tail: Option<u64>) -> Result<Chunk, ReadError>;
}

/// Claude Code's folder name for a working folder: every character that isn't an ASCII letter or digit becomes '-'.
pub fn project_dir(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

#[derive(Debug)]
struct Follow {
    path: PathBuf,
    /// The remote byte offset to read from next.
    next: u64,
    /// The status it was last brought up to date at.
    status: String,
}

/// How a round went, for the plugin's status.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Round {
    pub copied: usize,
    pub bytes: u64,
    pub errors: Vec<String>,
    pub not_granted: bool,
}

pub struct Mirror {
    root: PathBuf,
    tail_bytes: u64,
    follows: HashMap<String, Follow>,
    /// Off for good once the node said the grant is missing (until the plugin restarts with new config).
    pub off: Option<String>,
}

/// At most this many chunks per session per round, so one huge catch-up can't hold up the rest.
const CHUNKS_PER_ROUND: usize = 4;

impl Mirror {
    pub fn new(root: PathBuf, tail_bytes: u64) -> Mirror {
        // A fresh start: copies from before may be out of step with their sessions.
        let _ = std::fs::remove_dir_all(root.join("projects"));
        Mirror { root, tail_bytes, follows: HashMap::new(), off: None }
    }

    pub fn len(&self) -> usize {
        self.follows.len()
    }

    fn path_for(&self, r: &Remote) -> Option<PathBuf> {
        let cwd = r.cwd.as_deref()?;
        let ok_id =
            !r.session_id.is_empty() && r.session_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        ok_id.then(|| self.root.join("projects").join(project_dir(cwd)).join(format!("{}.jsonl", r.session_id)))
    }

    /// Bring every Claude Code session's copy up to date, and drop copies of sessions that ended.
    pub async fn round(&mut self, sessions: &[Remote], reader: &dyn Reader) -> Round {
        let mut out = Round::default();
        if self.off.is_some() {
            out.not_granted = true;
            return out;
        }
        let live: std::collections::HashSet<&str> =
            sessions.iter().filter(|r| r.harness == "claude").map(|r| r.session_id.as_str()).collect();
        let gone: Vec<String> = self.follows.keys().filter(|k| !live.contains(k.as_str())).cloned().collect();
        for id in gone {
            if let Some(f) = self.follows.remove(&id) {
                let _ = std::fs::remove_file(&f.path);
            }
        }
        for r in sessions.iter().filter(|r| r.harness == "claude") {
            let Some(path) = self.path_for(r) else { continue };
            let due = match self.follows.get(&r.session_id) {
                None => true,
                Some(f) => r.status == "working" || f.status != r.status || f.path != path,
            };
            if !due {
                continue;
            }
            match self.bring_up_to_date(r, path, reader).await {
                Ok(n) => {
                    if n > 0 {
                        out.copied += 1;
                        out.bytes += n;
                    }
                }
                Err(ReadError::NotGranted(m)) => {
                    tracing::info!("transcripts aren't copied: {m}");
                    self.off = Some(m);
                    out.not_granted = true;
                    return out;
                }
                Err(ReadError::Other(m)) => out.errors.push(format!("{}: {m}", r.address())),
            }
        }
        out
    }

    async fn bring_up_to_date(&mut self, r: &Remote, path: PathBuf, reader: &dyn Reader) -> Result<u64, ReadError> {
        let address = r.address();
        let mut written = 0;
        let fresh = match self.follows.get(&r.session_id) {
            Some(f) if f.path == path => false,
            Some(f) => {
                let _ = std::fs::remove_file(&f.path);
                true
            }
            None => true,
        };
        let mut next = if fresh { None } else { self.follows.get(&r.session_id).map(|f| f.next) };
        for _ in 0..CHUNKS_PER_ROUND {
            let chunk = match next {
                None => reader.read(&address, None, Some(self.tail_bytes)).await?,
                Some(n) => reader.read(&address, Some(n), None).await?,
            };
            if let Some(n) = next
                && chunk.size < n
            {
                // The transcript got shorter (rewritten): start the copy again from its end.
                next = None;
                self.follows.remove(&r.session_id);
                let _ = std::fs::remove_file(&path);
                continue;
            }
            let first = next.is_none();
            write(&path, &chunk.bytes, first)
                .map_err(|e| ReadError::Other(format!("writing {}: {e}", path.display())))?;
            written += chunk.bytes.len() as u64;
            let end = chunk.offset + chunk.bytes.len() as u64;
            next = Some(end);
            self.follows
                .insert(r.session_id.clone(), Follow { path: path.clone(), next: end, status: r.status.clone() });
            if chunk.bytes.is_empty() || end >= chunk.size {
                break;
            }
        }
        Ok(written)
    }
}

/// Write a chunk: a new file for the first, appended after that.
fn write(path: &Path, bytes: &[u8], first: bool) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).write(true).append(!first).truncate(first).open(path)?;
    f.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Mutex;

    /// Transcripts by address, served as the node would: whole lines only, a tail from a line start.
    struct Fake {
        files: Mutex<HashMap<String, Vec<u8>>>,
        reads: Mutex<Vec<String>>,
        max: usize,
    }

    #[async_trait]
    impl Reader for Fake {
        async fn read(&self, address: &str, offset: Option<u64>, tail: Option<u64>) -> Result<Chunk, ReadError> {
            self.reads.lock().await.push(format!("{address} {offset:?} {tail:?}"));
            if address.starts_with("secret") {
                return Err(ReadError::NotGranted("transcript.read isn't granted".into()));
            }
            let files = self.files.lock().await;
            let data = files.get(address).cloned().unwrap_or_default();
            let size = data.len();
            let mut start = match (offset, tail) {
                (Some(o), _) => (o as usize).min(size),
                (None, Some(t)) => size.saturating_sub(t as usize),
                _ => 0,
            };
            if offset.is_none() && start > 0 {
                start = data[start - 1..].iter().position(|b| *b == b'\n').map_or(size, |i| start + i);
            }
            let mut bytes = data[start..(start + self.max).min(size)].to_vec();
            match bytes.iter().rposition(|b| *b == b'\n') {
                Some(i) => bytes.truncate(i + 1),
                None => bytes.clear(),
            }
            Ok(Chunk { offset: start as u64, size: size as u64, bytes })
        }
    }

    fn remote(id: &str, tab: &str, status: &str, harness: &str) -> Remote {
        Remote {
            node: "laptop".into(),
            session_id: id.into(),
            tab: Some(tab.into()),
            harness: harness.into(),
            cwd: Some("/Users/a/code/my.app".into()),
            status: status.into(),
            changed_ms: 0,
            workspace_key: "laptop/w1".into(),
            pane_key: format!("laptop/w1/{id}"),
        }
    }

    #[test]
    fn folders_are_named_as_claude_code_names_them() {
        assert_eq!(project_dir("/home/a/code/fleetlayer"), "-home-a-code-fleetlayer");
        assert_eq!(project_dir("/Users/a/code/my.app"), "-Users-a-code-my-app");
    }

    #[tokio::test]
    async fn a_session_is_copied_from_its_tail_then_followed_while_it_works_and_dropped_when_it_ends() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake { files: Mutex::new(HashMap::new()), reads: Mutex::new(vec![]), max: 1000 };
        fake.files.lock().await.insert("billing@laptop".into(), b"{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n".to_vec());
        let mut m = Mirror::new(dir.path().join("claude"), 12);
        let path = dir.path().join("claude/projects/-Users-a-code-my-app/s-1.jsonl");

        let mut working = vec![remote("s-1", "billing", "working", "claude"), remote("c-1", "docs", "idle", "codex")];
        let r = m.round(&working, &fake).await;
        assert_eq!((r.copied, r.errors.len()), (1, 0));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"n\":3}\n", "the tail, from a line start");

        fake.files.lock().await.get_mut("billing@laptop").unwrap().extend(b"{\"n\":4}\n{\"n\":");
        m.round(&working, &fake).await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"n\":3}\n{\"n\":4}\n", "appended, half a line waits");

        // Idle: one more read for the change of status, then none while nothing changes.
        working[0].status = "idle".into();
        m.round(&working, &fake).await;
        let before = fake.reads.lock().await.len();
        m.round(&working, &fake).await;
        assert_eq!(fake.reads.lock().await.len(), before, "an idle session isn't read again");

        m.round(&[], &fake).await;
        assert!(!path.exists(), "the copy goes when its session ends");
        let reads = fake.reads.lock().await.clone();
        assert!(reads.iter().all(|r| r.starts_with("billing@laptop")), "codex sessions aren't copied: {reads:?}");
    }

    #[tokio::test]
    async fn a_long_catch_up_comes_in_chunks_and_a_rewritten_transcript_starts_again() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake { files: Mutex::new(HashMap::new()), reads: Mutex::new(vec![]), max: 8 };
        fake.files.lock().await.insert("b@laptop".into(), b"aaaaaaa\nbbbbbbb\n".to_vec());
        let mut m = Mirror::new(dir.path().join("claude"), 1 << 20);
        let s = vec![remote("s2", "b", "working", "claude")];
        m.round(&s, &fake).await;
        let path = dir.path().join("claude/projects/-Users-a-code-my-app/s2.jsonl");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "aaaaaaa\nbbbbbbb\n");
        fake.files.lock().await.insert("b@laptop".into(), b"new\n".to_vec());
        m.round(&s, &fake).await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
    }

    #[tokio::test]
    async fn without_the_grant_nothing_is_copied_and_it_stops_asking() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake { files: Mutex::new(HashMap::new()), reads: Mutex::new(vec![]), max: 100 };
        let mut m = Mirror::new(dir.path().join("claude"), 100);
        let s = vec![remote("s3", "secret", "working", "claude")];
        assert!(m.round(&s, &fake).await.not_granted);
        assert!(m.round(&s, &fake).await.not_granted);
        assert_eq!(fake.reads.lock().await.len(), 1);
    }
}
