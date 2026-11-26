//! Claude Code's cross-session messaging, as `SendMessage` uses it: every interactive session registers itself in
//! `~/.claude/sessions/<pid>.json` with a Unix socket and a key file beside it, and takes one `user` frame per message,
//! wrapped in a `<cross-session-message>` envelope. The session queues it like typed input: an idle session starts a
//! turn, a working one reads it at its next tool round, a blocked one after the person answers. The model sees it
//! labelled as coming from another session, never as the person's own words.
//!
//! This is Claude Code's internal protocol (`peerProtocol: 1`), not a documented API, so the version is checked.
//! First written for Quietdeck (`quietdeck-node/src/deliver.rs`, with `docs/claude-code-messaging.md` recording how it
//! was tested) by the same author.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The Claude Code messaging protocol this plugin speaks.
pub const PEER_PROTOCOL: u64 = 1;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Registration {
    pub pid: u32,
    pub session_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub peer_protocol: Option<u64>,
    #[serde(default)]
    pub messaging_socket_path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyFile {
    peer_token: String,
}

/// Why a message couldn't go in.
#[derive(Debug, PartialEq, Eq)]
pub enum SendError {
    /// There's no usable socket for the session (not registered, an older Claude Code, messaging off).
    NoChannel(String),
    /// The socket refused the connection: the session is gone.
    Gone(String),
    /// Anything else.
    Failed(String),
}

pub fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else { return false };
    // Signal 0 checks the process exists; EPERM means it exists but isn't ours.
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(e) => e == nix::errno::Errno::EPERM,
    }
}

pub fn usable(reg: &Registration) -> bool {
    reg.peer_protocol == Some(PEER_PROTOCOL) && reg.messaging_socket_path.is_some() && pid_alive(reg.pid)
}

/// Every interactive session's registry file.
pub async fn registrations(dir: &Path) -> Vec<Registration> {
    let mut out = Vec::new();
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else { return out };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "json")
            && let Ok(bytes) = tokio::fs::read(&p).await
            && let Ok(reg) = serde_json::from_slice::<Registration>(&bytes)
        {
            out.push(reg);
        }
    }
    out
}

/// The live registration for a session. A resumed session can leave a stale file from its old process: the live one
/// wins.
pub async fn find(dir: &Path, session_id: &str) -> Option<Registration> {
    let mut found: Vec<Registration> =
        registrations(dir).await.into_iter().filter(|r| r.session_id == session_id).collect();
    found.sort_by_key(|r| !pid_alive(r.pid));
    found.into_iter().next()
}

/// Claude Code's envelope: the receiver parses and re-serialises it, so the attribute is escaped as it would be. It
/// sets no `from` (replies follow the rendered text's instructions) and no `from-mode`: this plugin isn't a Claude
/// session, and claiming a permission mode would get around the check against permission laundering.
pub fn envelope(from_name: &str, body: &str) -> String {
    let name = from_name.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;");
    format!("<cross-session-message from-name=\"{name}\">\n{body}\n</cross-session-message>")
}

fn key_path(dir: &Path, reg: &Registration, socket: &str) -> PathBuf {
    // The hash is of the path as written, not its resolved form.
    let hash = hex::encode(Sha256::digest(socket.as_bytes()));
    dir.join(format!("{}.{hash}.key", reg.pid))
}

fn uuid_like() -> String {
    let b: [u8; 16] = rand::random();
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// Send one message to a registered session.
pub async fn send(dir: &Path, reg: &Registration, from_name: &str, text: &str) -> Result<(), SendError> {
    if !usable(reg) {
        return Err(SendError::NoChannel(format!("session {} has no usable messaging socket", reg.session_id)));
    }
    let socket = reg.messaging_socket_path.as_deref().unwrap_or_default();
    let key: KeyFile = tokio::fs::read(key_path(dir, reg, socket))
        .await
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| SendError::NoChannel("the session's messaging key isn't readable".into()))?;
    let mut stream = match tokio::time::timeout(Duration::from_secs(3), tokio::net::UnixStream::connect(socket)).await {
        Ok(Ok(s)) => s,
        _ => return Err(SendError::Gone("the session's messaging socket refused the connection".into())),
    };
    let auth = serde_json::json!({"type": "auth", "token": key.peer_token});
    let frame = serde_json::json!({
        "type": "user",
        "message": {"role": "user", "content": envelope(from_name, text)},
        "msg_id": uuid_like(),
        "priority": "next",
        "session_id": reg.session_id,
    });
    stream
        .write_all(format!("{auth}\n{frame}\n").as_bytes())
        .await
        .map_err(|e| SendError::Failed(format!("writing to the session's socket: {e}")))?;
    // An accepted message gets no answer. A refused one may: read briefly for an error line.
    let mut buf = vec![0u8; 4096];
    if let Ok(Ok(n)) = tokio::time::timeout(Duration::from_millis(300), stream.read(&mut buf)).await
        && n > 0
    {
        let reply = String::from_utf8_lossy(&buf[..n]);
        if reply.contains("error") || reply.contains("denied") || reply.contains("unauthor") {
            return Err(SendError::Failed(format!("the session refused the message: {}", reply.trim())));
        }
    }
    Ok(())
}

/// A session's transcript: `~/.claude/projects/<project>/<session id>.jsonl`.
pub fn transcript(claude_dir: &Path, session_id: &str) -> Option<PathBuf> {
    let projects = std::fs::read_dir(claude_dir.join("projects")).ok()?;
    projects.filter_map(|e| e.ok()).map(|e| e.path().join(format!("{session_id}.jsonl"))).find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_envelope_escapes_the_name_and_claims_no_mode() {
        let e = envelope("a \"b\" <c>", "hello");
        assert_eq!(
            e,
            "<cross-session-message from-name=\"a &quot;b&quot; &lt;c&gt;\">\nhello\n</cross-session-message>"
        );
        assert!(!e.contains("from-mode"));
    }

    /// A fake Claude Code session: a registry file, a key file and a socket that records what it gets.
    #[tokio::test]
    async fn a_session_gets_one_frame_on_its_socket() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let sock = home.path().join("s.sock");
        let sock_s = sock.to_str().unwrap().to_string();
        let pid = std::process::id();
        std::fs::write(
            dir.join(format!("{pid}.json")),
            serde_json::json!({"pid": pid, "sessionId": "s1", "peerProtocol": 1, "messagingSocketPath": sock_s})
                .to_string(),
        )
        .unwrap();
        let reg = find(&dir, "s1").await.unwrap();
        std::fs::write(key_path(&dir, &reg, &sock_s), r#"{"peerToken":"abc"}"#).unwrap();
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut got = String::new();
            s.read_to_string(&mut got).await.unwrap();
            got
        });
        send(&dir, &reg, "ci (laptop) via Fleetlayer", "green").await.unwrap();
        let got = server.await.unwrap();
        let mut lines = got.lines();
        assert_eq!(lines.next().unwrap(), serde_json::json!({"type": "auth", "token": "abc"}).to_string());
        let frame: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!((frame["type"].as_str(), frame["priority"].as_str()), (Some("user"), Some("next")));
        assert!(
            frame["message"]["content"]
                .as_str()
                .unwrap()
                .starts_with("<cross-session-message from-name=\"ci (laptop) via Fleetlayer\">\ngreen")
        );
    }

    #[tokio::test]
    async fn no_registration_or_an_old_protocol_is_no_channel() {
        let home = tempfile::tempdir().unwrap();
        assert!(find(home.path(), "nope").await.is_none());
        let reg = Registration {
            pid: std::process::id(),
            session_id: "s".into(),
            cwd: None,
            name: None,
            peer_protocol: Some(0),
            messaging_socket_path: Some("/x".into()),
        };
        assert!(matches!(send(home.path(), &reg, "x", "y").await, Err(SendError::NoChannel(_))));
    }
}
