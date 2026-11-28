//! A command whose stdout has gone away (a pager or `head` that quit, a client that hung up) ends quietly instead of
//! panicking with "failed printing to stdout: Broken pipe". A fake node answers on a socket of its own.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};

#[test]
fn a_closed_stdout_ends_a_command_quietly() {
    let dir = std::env::temp_dir().join(format!("fl-stdout-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("n.sock");
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { return };
            let mut out = conn.try_clone().unwrap();
            for line in BufReader::new(conn).lines() {
                let Ok(line) = line else { break };
                let v: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
                let big: Vec<String> = (0..2000).map(|i| format!("line {i}")).collect();
                let answer =
                    serde_json::json!({"jsonrpc": "2.0", "id": v["id"], "result": {"node": "probe", "lines": big}});
                let _ = writeln!(out, "{answer}");
            }
        }
    });

    // A pipe whose reading end is closed before the command starts: its first write fails with EPIPE.
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let out = Command::new(env!("CARGO_BIN_EXE_fleetlayer"))
        .args(["--socket", socket.to_str().unwrap(), "--json", "status"])
        .stdout(Stdio::from(writer))
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}
