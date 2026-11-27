//! The node's local sockets (`node.sock`, `node.tools.sock`), claimed once at start, before any plugin runs. A node
//! started by hand while the account's node is running used to remove that node's socket and bind its own, and remove
//! it again on exit, leaving the running node unreachable. Now:
//!
//! - a socket something answers on means another node: refuse to start, naming its pid;
//! - a socket nothing answers on is stale (a node that crashed): remove it and bind;
//! - on exit, remove the socket file only if it is still the one this node created.

use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// The socket file this node created; dropping it removes the file, unless something else has replaced it since.
pub struct SocketFile {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        if let Ok(m) = std::fs::symlink_metadata(&self.path)
            && m.dev() == self.dev
            && m.ino() == self.ino
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Bind `path` for this node, or refuse when another node answers there. The listener is non-blocking, for tokio.
pub fn claim(path: &Path) -> anyhow::Result<(UnixListener, SocketFile)> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if let Ok(m) = std::fs::symlink_metadata(path) {
        if !m.file_type().is_socket() {
            anyhow::bail!("{} exists and isn't a socket: move it away, or set socket: in node.yaml", path.display());
        }
        match UnixStream::connect(path) {
            Ok(s) => {
                let pid = peer_pid(&s).map(|p| format!(" (pid {p})")).unwrap_or_default();
                anyhow::bail!(
                    "another node is already running on {}{pid}: stop it first, or use FLEETLAYER_INSTANCE for a second one",
                    path.display()
                );
            }
            // Nothing answers: a node that crashed left it behind.
            Err(_) => std::fs::remove_file(path)?,
        }
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let m = std::fs::symlink_metadata(path)?;
    Ok((listener, SocketFile { path: path.to_path_buf(), dev: m.dev(), ino: m.ino() }))
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

    #[test]
    fn a_live_socket_is_refused_with_its_pid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.sock");
        let (_running, _file) = claim(&path).unwrap();
        let err = claim(&path).err().expect("a second claim is refused").to_string();
        assert!(err.contains("another node is already running"), "{err}");
        assert!(err.contains(&format!("(pid {})", std::process::id())), "{err}");
        assert!(path.exists(), "the running node's socket is left alone");
    }

    #[test]
    fn a_stale_socket_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.sock");
        drop(UnixListener::bind(&path).unwrap()); // the file stays, nothing listens
        let (_l, _f) = claim(&path).expect("a stale socket doesn't block a start");
        UnixStream::connect(&path).expect("the new one answers");
    }

    #[test]
    fn exit_removes_only_the_socket_this_node_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.sock");
        let (l, f) = claim(&path).unwrap();
        drop(l);
        drop(f);
        assert!(!path.exists(), "its own socket goes on exit");

        let (l, f) = claim(&path).unwrap();
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
        assert!(claim(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x");
    }
}
