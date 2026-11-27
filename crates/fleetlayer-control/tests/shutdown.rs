//! SIGTERM stops the control plane within a few seconds, and a linked node is told to reconnect (close code 1012)
//! instead of waiting for its link to time out.

#![cfg(unix)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

const BIN: &str = env!("CARGO_BIN_EXE_fleetlayer-control");

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[tokio::test]
async fn sigterm_closes_links_and_exits_quickly() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("control.db");
    let out = Command::new(BIN).arg("--db").arg(&db).args(["node", "add", "laptop"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let token = String::from_utf8(out.stdout).unwrap().trim().to_string();

    let (api, hooks) = (free_port(), free_port());
    let mut child = Command::new(BIN)
        .arg("--db")
        .arg(&db)
        .args(["serve", "--listen", &format!("127.0.0.1:{api}"), "--hooks-listen", &format!("127.0.0.1:{hooks}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // Link as the node, once the control plane listens.
    let started = Instant::now();
    let mut ws = loop {
        let mut req = format!("ws://127.0.0.1:{api}/v1/link").into_client_request().unwrap();
        req.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
        match tokio_tungstenite::connect_async(req).await {
            Ok((ws, _)) => break ws,
            Err(_) if started.elapsed() < Duration::from_secs(20) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => {
                let _ = child.kill();
                panic!("the control plane never accepted the link: {e}");
            }
        }
    };

    let signalled = Instant::now();
    assert!(Command::new("kill").args(["-TERM", &child.id().to_string()]).status().unwrap().success());

    let close = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(m) = ws.next().await {
            match m {
                Ok(Message::Close(frame)) => return frame,
                Ok(_) => continue,
                Err(e) => panic!("the link failed instead of closing: {e}"),
            }
        }
        None
    })
    .await
    .expect("a close frame within 5 s of SIGTERM");
    assert_eq!(close.map(|f| f.code), Some(CloseCode::Restart), "nodes are told the control plane is restarting");

    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if signalled.elapsed() > Duration::from_secs(8) {
            let _ = child.kill();
            panic!("still running 8 s after SIGTERM");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(status.success(), "a clean exit, not killed: {status:?}");
    assert!(signalled.elapsed() < Duration::from_secs(5), "stopped in {:?}", signalled.elapsed());

    // The database is complete without its write-ahead log: SQLite checkpointed it on close.
    let wal = dir.path().join("control.db-wal");
    assert!(!wal.exists() || std::fs::metadata(&wal).unwrap().len() == 0, "the WAL was checkpointed");
}
