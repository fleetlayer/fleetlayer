//! The harnesses Fleetlayer reaches through a small shim it installs inside the agent (pi's extension, OpenCode's
//! plugin: `shims/`). Each running agent process writes `<pid>.json` in the harness's state folder, naming its sessions,
//! the pane it runs in (`HERDR_PANE_ID`) and its socket `<pid>.sock`. The socket takes one JSON line:
//!
//! - `{"type":"deliver","sessionId","text"}` → `{"ok":true}`, or `{"ok":false,"code":"gone"|"failed","error"}`;
//! - `{"type":"watch"}` → a `{"type":"state","sessions":[{"id","prompt"}]}` line now and on every change.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// The shim protocol this plugin speaks.
pub const PROTOCOL: u32 = 1;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Registration {
    pub protocol: u32,
    pub pid: u32,
    pub socket: PathBuf,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub sessions: Vec<ShimSession>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ShimSession {
    pub id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    /// pi's session file.
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

/// The agents running now. A registration whose process is gone is removed, with its socket.
pub fn registrations(dir: &Path) -> Vec<Registration> {
    let Ok(entries) = std::fs::read_dir(dir) else { return vec![] };
    let mut out = vec![];
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Some(reg) = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Registration>(&b).ok()) else {
            continue;
        };
        // Only a file named after its own process, with its socket beside it, counts.
        if path.file_stem().and_then(|s| s.to_str()) != Some(&reg.pid.to_string())
            || reg.socket != path.with_extension("sock")
        {
            continue;
        }
        if !crate::files::pid_alive(reg.pid) {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(&reg.socket);
            continue;
        }
        if reg.protocol == PROTOCOL {
            out.push(reg);
        }
    }
    out.sort_by_key(|r| r.pid);
    out
}

/// The registration that runs `session`, else the one in the pane at `address` (a session the shim hasn't seen do
/// anything yet, such as one just resumed).
pub fn find<'a>(regs: &'a [Registration], session: &str, address: Option<&str>) -> Option<&'a Registration> {
    regs.iter()
        .rev()
        .find(|r| r.sessions.iter().any(|s| s.id == session))
        .or_else(|| address.and_then(|a| regs.iter().find(|r| r.address.as_deref() == Some(a))))
}

#[derive(Debug, PartialEq)]
pub enum DeliverError {
    /// The agent isn't in that session any more.
    Gone(String),
    /// No answer on the socket: the agent may have just exited.
    NoChannel(String),
    Failed(String),
}

pub async fn deliver(reg: &Registration, session: &str, text: &str) -> Result<(), DeliverError> {
    let go = async {
        let mut s = UnixStream::connect(&reg.socket)
            .await
            .map_err(|e| DeliverError::NoChannel(format!("can't reach the agent's socket: {e}")))?;
        let mut line = json!({"type": "deliver", "sessionId": session, "text": text}).to_string();
        line.push('\n');
        s.write_all(line.as_bytes()).await.map_err(|e| DeliverError::NoChannel(e.to_string()))?;
        let mut answer = String::new();
        BufReader::new(s).read_line(&mut answer).await.map_err(|e| DeliverError::Failed(e.to_string()))?;
        let v: Value = serde_json::from_str(&answer)
            .map_err(|_| DeliverError::Failed(format!("the agent answered {:?}", answer.trim())))?;
        if v["ok"] == true {
            return Ok(());
        }
        let why = v["error"].as_str().unwrap_or("no reason given").to_string();
        Err(if v["code"] == "gone" { DeliverError::Gone(why) } else { DeliverError::Failed(why) })
    };
    // A delivery the agent took may still be answered late; the node reports a timeout as such, never as "not sent".
    tokio::time::timeout(Duration::from_secs(10), go)
        .await
        .unwrap_or_else(|_| Err(DeliverError::Failed("the agent didn't answer within 10 seconds".into())))
}

/// Follow one agent's sessions until its socket closes: each state line goes to `on_state` as (session, prompt).
pub async fn watch(socket: &Path, mut on_state: impl FnMut(Vec<(String, Option<Value>)>)) -> std::io::Result<()> {
    let mut s = UnixStream::connect(socket).await?;
    s.write_all(b"{\"type\":\"watch\"}\n").await?;
    let mut lines = BufReader::new(s).lines();
    while let Some(l) = lines.next_line().await? {
        let Ok(v) = serde_json::from_str::<Value>(&l) else { continue };
        if v["type"] != "state" {
            continue;
        }
        let sessions = v["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| Some((s["id"].as_str()?.to_string(), Some(s["prompt"].clone()).filter(|p| p.is_object()))))
            .collect();
        on_state(sessions);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_reg(dir: &Path, pid: u32, sessions: Value, address: Option<&str>) {
        let reg = json!({"protocol": PROTOCOL, "pid": pid, "socket": dir.join(format!("{pid}.sock")),
            "address": address, "sessions": sessions});
        std::fs::write(dir.join(format!("{pid}.json")), reg.to_string()).unwrap();
    }

    #[test]
    fn only_live_registrations_named_after_their_process_count() {
        let d = tempfile::tempdir().unwrap();
        let me = std::process::id();
        write_reg(d.path(), me, json!([{"id": "s1", "cwd": "/w"}]), Some("w1:p2"));
        write_reg(d.path(), u32::MAX - 1, json!([]), None);
        std::fs::write(d.path().join("123.json"), json!({"protocol": 1, "pid": me, "socket": "/x"}).to_string())
            .unwrap();
        let regs = registrations(d.path());
        assert_eq!(regs.len(), 1);
        assert_eq!(regs[0].sessions[0].id, "s1");
        assert!(!d.path().join(format!("{}.json", u32::MAX - 1)).exists(), "a dead process's file is cleaned up");
        assert_eq!(find(&regs, "s1", None).map(|r| r.pid), Some(me));
        assert_eq!(find(&regs, "s9", Some("w1:p2")).map(|r| r.pid), Some(me), "by its pane");
        assert!(find(&regs, "s9", Some("w1:p3")).is_none());
    }

    #[tokio::test]
    async fn delivers_and_watches_over_the_socket() {
        let d = tempfile::tempdir().unwrap();
        let sock = d.path().join("1.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            loop {
                let (s, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let (r, mut w) = s.into_split();
                    let mut lines = BufReader::new(r).lines();
                    let v: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                    let answer = match v["type"].as_str() {
                        Some("watch") => {
                            r#"{"type":"state","sessions":[{"id":"s1","prompt":{"id":"p","kind":"question"}},{"id":"s2","prompt":null}]}"#
                        }
                        _ if v["sessionId"] == "s1" => r#"{"ok":true}"#,
                        _ => r#"{"ok":false,"code":"gone","error":"another session now"}"#,
                    };
                    w.write_all(format!("{answer}\n").as_bytes()).await.unwrap();
                });
            }
        });
        let reg = Registration { protocol: 1, pid: 1, socket: sock.clone(), address: None, sessions: vec![] };
        assert_eq!(deliver(&reg, "s1", "hi").await, Ok(()));
        assert!(matches!(deliver(&reg, "s2", "hi").await, Err(DeliverError::Gone(_))));
        let mut seen = vec![];
        let _ = tokio::time::timeout(Duration::from_secs(2), watch(&sock, |s| seen = s)).await;
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].1.as_ref().unwrap()["kind"], "question");
        assert!(seen[1].1.is_none());
        let gone = Registration { socket: d.path().join("none.sock"), ..reg };
        assert!(matches!(deliver(&gone, "s1", "hi").await, Err(DeliverError::NoChannel(_))));
    }
}
