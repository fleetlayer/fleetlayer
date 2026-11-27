//! Commands that only read (`audit verify`, `acl explain`, the lists) never take the database's write lock, so they
//! can run next to a busy server (a scheduled `audit verify`, an operator's `kubectl exec`) without holding up its
//! writes; and they refuse a database whose schema isn't theirs.

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_fleetlayer-control");

fn run(db: &Path, args: &[&str]) -> Output {
    Command::new(BIN).arg("--db").arg(db).args(args).output().unwrap()
}

fn ok(out: &Output) -> bool {
    out.status.success()
}

#[tokio::test]
async fn reading_commands_run_while_another_process_holds_the_write_lock() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("control.db");
    let out = run(&db, &["node", "add", "laptop"]);
    assert!(ok(&out), "{}", String::from_utf8_lossy(&out.stderr));

    // The server's place: a connection holding the write lock, as during a burst of writes.
    let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&db);
    let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
    let mut lock = pool.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *lock).await.unwrap();

    let reads: &[&[&str]] = &[
        &["audit", "verify"],
        &["audit", "tail"],
        &["acl", "explain", "--from", "a@laptop", "--can", "message", "--to", "b@laptop"],
        &["acl", "list"],
        &["acl", "show"],
        &["acl", "log"],
        &["node", "list"],
        &["endpoint", "list"],
        &["token", "list"],
        &["signin", "tokens"],
        &["approvals", "list"],
        &["tools"],
        &["tenant", "list"],
        &["person", "list"],
        &["member", "list"],
        &["team", "list"],
        &["agent", "list"],
        &["invite", "list"],
    ];
    for args in reads {
        let started = Instant::now();
        let out = run(&db, args);
        assert!(ok(&out), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        assert!(started.elapsed() < Duration::from_secs(3), "{args:?} waited for the lock: {:?}", started.elapsed());
    }

    // A command that writes waits for the lock as before (it doesn't sneak past it).
    sqlx::query("COMMIT").execute(&mut *lock).await.unwrap();
    drop(lock);
    assert!(ok(&run(&db, &["node", "add", "server"])));
}

#[tokio::test]
async fn a_reading_command_refuses_an_older_schema() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("control.db");
    assert!(ok(&run(&db, &["node", "add", "laptop"])));
    let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&db);
    let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = (SELECT MAX(version) FROM _sqlx_migrations)")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let out = run(&db, &["node", "list"]);
    assert!(!ok(&out));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("older schema") && err.contains("start the new server once"), "{err}");

    let missing = dir.path().join("nothing.db");
    let out = run(&missing, &["audit", "verify"]);
    assert!(!ok(&out));
    assert!(!missing.exists(), "a reading command never creates a database");
}
