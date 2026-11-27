//! Other machines' Claude Code transcripts, copied here for clients that read transcript files themselves: herdr's
//! phone relay's Conversation view, pointed at this folder with `HERDR_CLAUDE_CONFIG_DIRS=<state_dir>/claude`.
//!
//! The copy is laid out as Claude Code lays out its own: `<root>/projects/<the session's folder, every character that
//! isn't a letter or digit as '-'>/<session id>.jsonl`. A session seen for the first time gets the end of its
//! transcript (`tail_bytes`, from a line start); after that only what's appended (one read per session per round),
//! while it works and once after each change of status. Reads go through the node (`_fleetlayer.dev/transcript/read`,
//! grant `transcript.read`), which the access policy checks as `read` on the session and which redacts delivered
//! secrets on the machine the transcript is on: without the grant, nothing is copied.
//!
//! What's copied stays bounded and current:
//! - a copy goes when its session ends, when a read is refused (access revoked), and when the plugin starts;
//! - a copy that would pass `file_cap` starts again from the transcript's end; past `total_cap` nothing new is copied
//!   and the round says so, for the plugin's status;
//! - a session ID two machines both claim, or one this machine's own Claude Code has, isn't copied: the client finds
//!   transcripts by session ID, and a copy must never stand in for another session's conversation.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::fleet::Remote;

/// Part of a remote transcript, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub offset: u64,
    /// Where the next read starts in the remote file (the node redacts secrets, so the bytes can be shorter).
    pub next: u64,
    pub size: u64,
    pub bytes: Vec<u8>,
}

/// Why a read failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// The node doesn't grant this plugin transcript reads: mirroring is off.
    NotGranted(String),
    /// The access policy refused this session (or is waiting for the person): its copy goes.
    Refused(String),
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

/// How much is copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Copied from a transcript's end when it's first seen, or starts again.
    pub tail_bytes: u64,
    /// The most one copy may grow to before it starts again from the end.
    pub file_cap: u64,
    /// The most all copies together may hold.
    pub total_cap: u64,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { tail_bytes: 1 << 20, file_cap: 8 << 20, total_cap: 256 << 20 }
    }
}

#[derive(Debug)]
struct Follow {
    path: PathBuf,
    /// The remote byte offset to read from next.
    next: u64,
    /// The copy's size here.
    written: u64,
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
    /// Sessions left out because the copies reached `total_cap`.
    pub over_total: usize,
    /// Sessions left out because their ID isn't theirs alone.
    pub ambiguous: Vec<String>,
}

/// How long a refused session waits before it's tried again.
const REFUSED_WAIT: Duration = Duration::from_secs(600);
/// How long the list of this machine's own session IDs is kept before it's read again.
const LOCAL_IDS_TTL: Duration = Duration::from_secs(60);

pub struct Mirror {
    root: PathBuf,
    limits: Limits,
    /// This machine's own Claude Code config folders (`~/.claude`): their sessions are never shadowed.
    local_roots: Vec<PathBuf>,
    /// By `<node>/<session id>`.
    follows: HashMap<String, Follow>,
    refused: HashMap<String, Instant>,
    local_ids: Option<(Instant, HashSet<String>)>,
    /// Off for good once the node said the grant is missing (until the plugin restarts with new config).
    pub off: Option<String>,
}

fn key(r: &Remote) -> String {
    format!("{}/{}", r.node, r.session_id)
}

/// Every session ID that has a transcript under these Claude Code config folders.
fn ids_under(roots: &[PathBuf]) -> HashSet<String> {
    let mut out = HashSet::new();
    for root in roots {
        let Ok(projects) = std::fs::read_dir(root.join("projects")) else { continue };
        for p in projects.flatten() {
            let Ok(files) = std::fs::read_dir(p.path()) else { continue };
            for f in files.flatten() {
                if let Some(id) = f.file_name().to_str().and_then(|n| n.strip_suffix(".jsonl")) {
                    out.insert(id.to_string());
                }
            }
        }
    }
    out
}

impl Mirror {
    pub fn new(root: PathBuf, limits: Limits, local_roots: Vec<PathBuf>) -> Mirror {
        // A fresh start: copies from before may be out of step with their sessions, or no longer allowed.
        let _ = std::fs::remove_dir_all(root.join("projects"));
        let limits = Limits { tail_bytes: limits.tail_bytes.min(limits.file_cap / 2).max(1), ..limits };
        Mirror {
            root,
            limits,
            local_roots,
            follows: HashMap::new(),
            refused: HashMap::new(),
            local_ids: None,
            off: None,
        }
    }

    pub fn len(&self) -> usize {
        self.follows.len()
    }

    /// What the copies hold now.
    pub fn total(&self) -> u64 {
        self.follows.values().map(|f| f.written).sum()
    }

    fn path_for(&self, r: &Remote) -> Option<PathBuf> {
        let cwd = r.cwd.as_deref()?;
        let ok_id =
            !r.session_id.is_empty() && r.session_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        ok_id.then(|| self.root.join("projects").join(project_dir(cwd)).join(format!("{}.jsonl", r.session_id)))
    }

    fn drop_copy(&mut self, k: &str) {
        if let Some(f) = self.follows.remove(k) {
            let _ = std::fs::remove_file(&f.path);
        }
    }

    fn local_ids(&mut self) -> &HashSet<String> {
        let stale = self.local_ids.as_ref().is_none_or(|(at, _)| at.elapsed() > LOCAL_IDS_TTL);
        if stale {
            self.local_ids = Some((Instant::now(), ids_under(&self.local_roots)));
        }
        &self.local_ids.as_ref().expect("set above").1
    }

    /// Bring every Claude Code session's copy up to date, and drop the copies that must go.
    pub async fn round(&mut self, sessions: &[Remote], reader: &dyn Reader) -> Round {
        let mut out = Round::default();
        if self.off.is_some() {
            out.not_granted = true;
            return out;
        }
        let claude: Vec<&Remote> = sessions.iter().filter(|r| r.harness == "claude").collect();
        // A session ID claimed by more than one machine, or by this one, belongs to nobody here.
        let mut claims: HashMap<&str, usize> = HashMap::new();
        for r in &claude {
            *claims.entry(r.session_id.as_str()).or_default() += 1;
        }
        let local = self.local_ids().clone();
        let ok = |r: &Remote| claims.get(r.session_id.as_str()) == Some(&1) && !local.contains(&r.session_id);
        let live: HashSet<String> = claude.iter().filter(|r| ok(r)).map(|r| key(r)).collect();
        let gone: Vec<String> = self.follows.keys().filter(|k| !live.contains(*k)).cloned().collect();
        for k in gone {
            self.drop_copy(&k);
        }
        self.refused.retain(|_, at| at.elapsed() < REFUSED_WAIT);
        for r in claude {
            if !ok(r) {
                out.ambiguous.push(r.address());
                continue;
            }
            let k = key(r);
            let Some(path) = self.path_for(r) else { continue };
            if self.refused.contains_key(&k) {
                continue;
            }
            let due = match self.follows.get(&k) {
                None => true,
                Some(f) => r.status == "working" || f.status != r.status || f.path != path,
            };
            if !due {
                continue;
            }
            if self.total() >= self.limits.total_cap {
                out.over_total += 1;
                continue;
            }
            match self.bring_up_to_date(r, &k, path, reader).await {
                Ok(n) => {
                    if n > 0 {
                        out.copied += 1;
                        out.bytes += n;
                    }
                }
                Err(ReadError::NotGranted(m)) => {
                    tracing::info!("transcripts aren't copied: {m}");
                    self.off = Some(m);
                    for k in self.follows.keys().cloned().collect::<Vec<_>>() {
                        self.drop_copy(&k);
                    }
                    out.not_granted = true;
                    return out;
                }
                Err(ReadError::Refused(m)) => {
                    self.drop_copy(&k);
                    self.refused.insert(k, Instant::now());
                    out.errors.push(format!("{}: {m}", r.address()));
                }
                Err(ReadError::Other(m)) => out.errors.push(format!("{}: {m}", r.address())),
            }
        }
        out
    }

    /// One read: from the end for a new copy (or one that got too big, or a transcript that got shorter), else what was
    /// appended.
    async fn bring_up_to_date(
        &mut self,
        r: &Remote,
        k: &str,
        path: PathBuf,
        reader: &dyn Reader,
    ) -> Result<u64, ReadError> {
        let address = r.address();
        let mut next = match self.follows.get(k) {
            Some(f) if f.path == path && f.written < self.limits.file_cap => Some((f.next, f.written)),
            Some(_) => {
                self.drop_copy(k);
                None
            }
            None => None,
        };
        // At most two reads: the second only when the transcript got shorter and the copy starts again.
        for _ in 0..2 {
            let chunk = match next {
                None => reader.read(&address, None, Some(self.limits.tail_bytes)).await?,
                Some((n, _)) => reader.read(&address, Some(n), None).await?,
            };
            if let Some((n, _)) = next
                && chunk.size < n
            {
                self.drop_copy(k);
                next = None;
                continue;
            }
            let written = match next {
                Some((_, w)) if w + chunk.bytes.len() as u64 > self.limits.file_cap => {
                    // This much more would pass the cap: start again from the end next round.
                    self.drop_copy(k);
                    return Ok(0);
                }
                Some((_, w)) => w,
                None => 0,
            };
            write(&path, &chunk.bytes, next.is_none())
                .map_err(|e| ReadError::Other(format!("writing {}: {e}", path.display())))?;
            let n = chunk.bytes.len() as u64;
            self.follows.insert(
                k.to_string(),
                Follow { path, next: chunk.next, written: written + n, status: r.status.clone() },
            );
            return Ok(n);
        }
        Ok(0)
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

    /// Transcripts by address, served as the node would: whole lines only, a tail from a line start. Addresses
    /// starting "secret" aren't granted; "denied" ones are refused by the policy.
    struct Fake {
        files: Mutex<HashMap<String, Vec<u8>>>,
        reads: Mutex<Vec<String>>,
        max: usize,
    }

    impl Fake {
        fn new(max: usize) -> Fake {
            Fake { files: Mutex::new(HashMap::new()), reads: Mutex::new(vec![]), max }
        }
    }

    #[async_trait]
    impl Reader for Fake {
        async fn read(&self, address: &str, offset: Option<u64>, tail: Option<u64>) -> Result<Chunk, ReadError> {
            self.reads.lock().await.push(format!("{address} {offset:?} {tail:?}"));
            if address.starts_with("secret") {
                return Err(ReadError::NotGranted("transcript.read isn't granted".into()));
            }
            if address.starts_with("denied") {
                return Err(ReadError::Refused("may not read (access control: deny[0])".into()));
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
            let next = (start + bytes.len()) as u64;
            Ok(Chunk { offset: start as u64, next, size: size as u64, bytes })
        }
    }

    fn remote(node: &str, id: &str, tab: &str, status: &str, harness: &str) -> Remote {
        Remote {
            node: node.into(),
            session_id: id.into(),
            tab: Some(tab.into()),
            harness: harness.into(),
            cwd: Some("/Users/a/code/my.app".into()),
            status: status.into(),
            changed_ms: 0,
            workspace_key: format!("{node}/w1"),
            pane_key: format!("{node}/w1/{id}"),
        }
    }

    fn limits(tail: u64) -> Limits {
        Limits { tail_bytes: tail, ..Limits::default() }
    }

    fn copy(dir: &Path, id: &str) -> PathBuf {
        dir.join(format!("claude/projects/-Users-a-code-my-app/{id}.jsonl"))
    }

    #[test]
    fn folders_are_named_as_claude_code_names_them() {
        assert_eq!(project_dir("/home/a/code/fleetlayer"), "-home-a-code-fleetlayer");
        assert_eq!(project_dir("/Users/a/code/my.app"), "-Users-a-code-my-app");
    }

    #[tokio::test]
    async fn a_session_is_copied_from_its_tail_then_followed_while_it_works_and_dropped_when_it_ends() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new(1000);
        fake.files.lock().await.insert("billing@laptop".into(), b"{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n".to_vec());
        let mut m = Mirror::new(dir.path().join("claude"), limits(12), vec![]);
        let path = copy(dir.path(), "s-1");

        let mut working = vec![
            remote("laptop", "s-1", "billing", "working", "claude"),
            remote("laptop", "c-1", "docs", "idle", "codex"),
        ];
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
    async fn a_catch_up_comes_one_read_a_round_and_a_rewritten_transcript_starts_again() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new(8);
        fake.files.lock().await.insert("b@laptop".into(), b"aaaaaaa\nbbbbbbb\n".to_vec());
        let mut m = Mirror::new(dir.path().join("claude"), limits(1 << 20), vec![]);
        let s = vec![remote("laptop", "s2", "b", "working", "claude")];
        let path = copy(dir.path(), "s2");
        m.round(&s, &fake).await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "aaaaaaa\n", "one read per session per round");
        m.round(&s, &fake).await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "aaaaaaa\nbbbbbbb\n");
        fake.files.lock().await.insert("b@laptop".into(), b"new\n".to_vec());
        m.round(&s, &fake).await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
    }

    #[tokio::test]
    async fn without_the_grant_nothing_is_copied_and_it_stops_asking() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new(100);
        let mut m = Mirror::new(dir.path().join("claude"), limits(100), vec![]);
        let s = vec![remote("laptop", "s3", "secret", "working", "claude")];
        assert!(m.round(&s, &fake).await.not_granted);
        assert!(m.round(&s, &fake).await.not_granted);
        assert_eq!(fake.reads.lock().await.len(), 1);
    }

    /// Access revoked: the copy goes at once, and the session isn't asked about again for a while.
    #[tokio::test]
    async fn a_refused_read_deletes_the_copy() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new(100);
        fake.files.lock().await.insert("ops@laptop".into(), b"line\n".to_vec());
        let mut m = Mirror::new(dir.path().join("claude"), limits(100), vec![]);
        let mut s = vec![remote("laptop", "s4", "ops", "working", "claude")];
        m.round(&s, &fake).await;
        let path = copy(dir.path(), "s4");
        assert!(path.exists());
        // The rule changes: the same session, now refused (the fake refuses addresses starting "denied").
        s[0].tab = Some("denied ops".into());
        let r = m.round(&s, &fake).await;
        assert_eq!(r.errors.len(), 1);
        assert!(!path.exists(), "the copy went with the access");
        let reads = fake.reads.lock().await.len();
        m.round(&s, &fake).await;
        assert_eq!(fake.reads.lock().await.len(), reads, "not asked again straight away");
    }

    /// One copy never grows past its cap (it starts again from the end), and past the total nothing new is copied.
    #[tokio::test]
    async fn copies_stay_within_their_limits() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new(1000);
        fake.files.lock().await.insert("a@laptop".into(), b"0123456789\n".to_vec());
        fake.files.lock().await.insert("b@laptop".into(), b"abcdefghij\n".to_vec());
        let caps = Limits { tail_bytes: 100, file_cap: 30, total_cap: 40 };
        let mut m = Mirror::new(dir.path().join("claude"), caps, vec![]);
        let s = vec![remote("laptop", "sa", "a", "working", "claude")];
        m.round(&s, &fake).await;
        for _ in 0..3 {
            fake.files.lock().await.get_mut("a@laptop").unwrap().extend(b"0123456789\n");
            m.round(&s, &fake).await;
        }
        let size = std::fs::metadata(copy(dir.path(), "sa")).map(|m| m.len()).unwrap_or(0);
        assert!(size <= 30, "a copy stays under its cap: {size}");
        // Another session while the total is full: left out, and the round says so.
        let big = Limits { tail_bytes: 100, file_cap: 30, total_cap: 11 };
        let mut m = Mirror::new(dir.path().join("claude2"), big, vec![]);
        let two = vec![remote("laptop", "sa", "a", "idle", "claude"), remote("laptop", "sb", "b", "working", "claude")];
        let r = m.round(&two, &fake).await;
        assert_eq!(r.over_total, 1, "{r:?}");
        assert!(m.total() <= 30);
    }

    /// The client finds a transcript by session ID alone: an ID two machines claim, or one this machine's own Claude
    /// Code has, is never copied, so a node can't put words in another session's conversation.
    #[tokio::test]
    async fn a_session_id_that_isnt_one_machines_alone_is_never_copied() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new(1000);
        fake.files.lock().await.insert("real@laptop".into(), b"real\n".to_vec());
        fake.files.lock().await.insert("fake@server".into(), b"fake\n".to_vec());
        fake.files.lock().await.insert("mine@server".into(), b"fake\n".to_vec());
        let local = dir.path().join("dot-claude");
        std::fs::create_dir_all(local.join("projects/-home-a")).unwrap();
        std::fs::write(local.join("projects/-home-a/local-1.jsonl"), "x\n").unwrap();
        let mut m = Mirror::new(dir.path().join("claude"), limits(100), vec![local]);
        let s = vec![
            remote("laptop", "dup", "real", "working", "claude"),
            remote("server", "dup", "fake", "working", "claude"),
            remote("server", "local-1", "mine", "working", "claude"),
        ];
        let r = m.round(&s, &fake).await;
        assert_eq!(r.ambiguous.len(), 3, "{r:?}");
        assert!(fake.reads.lock().await.is_empty());
        assert!(!copy(dir.path(), "dup").exists());
        // Once the claim is one machine's alone, it's copied.
        let r = m.round(&s[..1], &fake).await;
        assert_eq!(r.copied, 1);
        assert_eq!(std::fs::read_to_string(copy(dir.path(), "dup")).unwrap(), "real\n");
    }
}
