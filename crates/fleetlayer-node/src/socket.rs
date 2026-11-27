//! The node's local sockets (`node.sock`, `node.tools.sock`), claimed once at start, before any plugin runs. A node
//! started by hand while the account's node is running used to remove that node's socket and bind its own, and remove
//! it again on exit, leaving the running node unreachable. Now:
//!
//! - a socket something answers on means another node: refuse to start, naming its pid, once it has kept answering for
//!   10 seconds (a restart or self-update can start the new node while the old one is still shutting down);
//! - a socket nothing answers on is stale (a node that crashed): remove it and bind;
//! - on exit, remove the socket file only if it is still the one this node created and nothing answers on it (one still
//!   listening, such as this node's tools socket while its task winds down, is left for the next start to clear).

use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// The socket file this node created; dropping it removes the file, unless something else has replaced it since or
/// something answers on it. Both checks, since a filesystem may give a new file the inode number (and, with coarse
/// timestamps, the change time) of one just deleted: a socket another node listens on is never removed.
pub struct SocketFile {
    path: PathBuf,
    id: (u64, u64, i64, i64),
}

fn identity(m: &std::fs::Metadata) -> (u64, u64, i64, i64) {
    (m.dev(), m.ino(), m.ctime(), m.ctime_nsec())
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        if let Ok(m) = std::fs::symlink_metadata(&self.path)
            && identity(&m) == self.id
            && UnixStream::connect(&self.path).is_err()
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// How long another node may keep answering before this one gives up.
pub const WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Bind `path` for this node, or refuse when another node still answers there after `wait`. The listener is
/// non-blocking, for tokio.
pub fn claim(path: &Path, wait: std::time::Duration) -> anyhow::Result<(UnixListener, SocketFile)> {
    let deadline = std::time::Instant::now() + wait;
    loop {
        match try_claim(path) {
            Err(Claim::Busy(msg)) if std::time::Instant::now() < deadline => {
                tracing::info!("{msg}; waiting for it to stop");
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            Err(Claim::Busy(msg)) => anyhow::bail!(msg),
            Err(Claim::Failed(e)) => return Err(e),
            Ok(c) => return Ok(c),
        }
    }
}

enum Claim {
    Busy(String),
    Failed(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for Claim {
    fn from(e: E) -> Self {
        Claim::Failed(e.into())
    }
}

fn try_claim(path: &Path) -> Result<(UnixListener, SocketFile), Claim> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if let Ok(m) = std::fs::symlink_metadata(path) {
        if !m.file_type().is_socket() {
            return Err(Claim::Failed(anyhow::anyhow!(
                "{} exists and isn't a socket: move it away, or set socket: in node.yaml",
                path.display()
            )));
        }
        match UnixStream::connect(path) {
            Ok(s) => {
                let pid = peer_pid(&s).map(|p| format!(" (pid {p})")).unwrap_or_default();
                return Err(Claim::Busy(format!(
                    "another node is already running on {}{pid}: stop it first, or use FLEETLAYER_INSTANCE for a second one",
                    path.display()
                )));
            }
            // Nothing answers: a node that crashed left it behind.
            Err(_) => std::fs::remove_file(path)?,
        }
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let m = std::fs::symlink_metadata(path)?;
    Ok((listener, SocketFile { path: path.to_path_buf(), id: identity(&m) }))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_pid(s: &UnixStream) -> Option<i32> {
    nix::sys::socket::getsockopt(s, nix::sys::socket::sockopt::PeerCredentials).ok().map(|c| c.pid())
}

#[cfg(target_vendor = "apple")]
fn peer_pid(s: &UnixStream) -> Option<i32> {
    nix::sys::socket::getsockopt(s, nix::sys::socket::sockopt::LocalPeerPid).ok()
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn peer_pid(_: &UnixStream) -> Option<i32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: std::time::Duration = std::time::Duration::ZERO;

    #[test]
    fn a_node_that_is_shutting_down_is_waited_for() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.sock");
        let (old, old_file) = claim(&path, NOW).unwrap();
        let stopping = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(600));
            drop(old);
            drop(old_file);
        });
        let started = std::time::Instant::now();
        let (_new, _f) =
            claim(&path, std::time::Duration::from_secs(5)).expect("the new node starts once the old one stops");
        assert!(started.elapsed() >= std::time::Duration::from_millis(500));
        stopping.join().unwrap();
        UnixStream::connect(&path).expect("the new node answers");
    }

    #[test]
    fn a_node_that_keeps_answering_is_refused_after_the_wait() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.sock");
        let (_running, _file) = claim(&path, NOW).unwrap();
        let started = std::time::Instant::now();
        assert!(claim(&path, std::time::Duration::from_millis(700)).is_err());
        assert!(started.elapsed() >= std::time::Duration::from_millis(700));
    }

    #[test]
    fn a_live_socket_is_refused_with_its_pid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.sock");
        let (_running, _file) = claim(&path, NOW).unwrap();
        let err = claim(&path, NOW).err().expect("a second claim is refused").to_string();
        assert!(err.contains("another node is already running"), "{err}");
        assert!(err.contains(&format!("(pid {})", std::process::id())), "{err}");
        assert!(path.exists(), "the running node's socket is left alone");
    }

    #[test]
    fn a_stale_socket_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.sock");
        drop(UnixListener::bind(&path).unwrap()); // the file stays, nothing listens
        let (_l, _f) = claim(&path, NOW).expect("a stale socket doesn't block a start");
        UnixStream::connect(&path).expect("the new one answers");
    }

    #[test]
    fn exit_removes_only_the_socket_this_node_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.sock");
        let (l, f) = claim(&path, NOW).unwrap();
        drop(l);
        drop(f);
        assert!(!path.exists(), "its own socket goes on exit");

        let (l, f) = claim(&path, NOW).unwrap();
        drop(l);
        std::fs::remove_file(&path).unwrap();
        let _other = UnixListener::bind(&path).unwrap(); // another node's, made since
        drop(f);
        assert!(path.exists(), "a socket it didn't create stays");
    }

    #[test]
    fn a_file_that_isnt_a_socket_is_never_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.sock");
        std::fs::write(&path, "x").unwrap();
        assert!(claim(&path, NOW).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x");
    }
}
