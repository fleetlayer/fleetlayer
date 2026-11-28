//! The tamper-evident audit log of access decisions, after Rack Gateway's (DocSpring/rack-gateway).
//!
//! - Every decision is written before the request goes ahead; when it can't be written, the request is refused (the
//!   caller fails closed).
//! - Each row's hash is an HMAC-SHA256, keyed with a secret kept outside the database (`--audit-key-file`), over every
//!   field, length-prefixed, and the previous row's hash. Changing, reordering or removing a row breaks the chain at
//!   that row; only removing the newest rows leaves a valid chain, which anchors catch.
//! - Without a key the chain is plain SHA-256: it still catches accidents and naive edits, not someone who can
//!   recompute it. `audit verify` says which.
//! - Optionally, an anchor (the newest row's position and hash, and the previous anchor's SHA-256) is written every
//!   hour to a directory outside the database: another machine's disk, a git checkout, write-once storage.
//! - Repeats of one denial (or one request waiting for approval) within a minute are written as one row with a count,
//!   so a looping agent can't flood it.
//! - SQL triggers make the table append-only.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use sqlx::Row;
use tokio::sync::Mutex;

use crate::db::{Db, now_ms};

const GENESIS: &str = "genesis";
const REPEAT_WINDOW_MS: i64 = 60_000;
/// Tries of a write that finds the database busy, and the pause before each retry.
const WRITE_TRIES: u32 = 3;
const RETRY_AFTER: [Duration; 2] = [Duration::from_millis(250), Duration::from_secs(1)];
/// How long `/healthz` reports degraded after a write failed.
pub const DEGRADED_FOR: Duration = Duration::from_secs(300);

/// Run `op`, again after a pause while it fails with SQLite's busy or locked, up to [`WRITE_TRIES`] times in all.
async fn retry_busy<F, Fut>(mut op: F) -> anyhow::Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let mut tries = 1;
    loop {
        match op().await {
            Err(e) if tries < WRITE_TRIES && crate::db::is_busy(&e) => {
                tracing::warn!("audit: the database is busy ({e}); trying again");
                tokio::time::sleep(RETRY_AFTER[(tries - 1) as usize]).await;
                tries += 1;
            }
            r => return r,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// `allowed`, `denied`, `audit` (an audit rule would have refused it).
    pub decision: String,
    pub subject: String,
    pub verb: String,
    pub target: String,
    pub reason: String,
}

pub struct Audit {
    key: Option<Vec<u8>>,
    /// Where hourly anchors go (`--audit-anchor-dir`), for `GET /v1/audit/verify`.
    anchors: Option<PathBuf>,
    write: Mutex<()>,
    /// The last time a row couldn't be written, and why: `/healthz` reports degraded for a while after.
    failed: std::sync::Mutex<Option<(std::time::Instant, String)>>,
    /// Denials seen again within the window: key → (first seen, repeats since, entry).
    repeats: Mutex<HashMap<String, (i64, i64, String, Entry)>>,
    /// Since when `/healthz` has found the database busy (no connection free), while it is.
    pub db_busy_since: std::sync::Mutex<Option<std::time::Instant>>,
}

fn lp(out: &mut Vec<u8>, v: &str) {
    out.extend_from_slice(&(v.len() as u64).to_be_bytes());
    out.extend_from_slice(v.as_bytes());
}

impl Audit {
    pub fn new(key: Option<Vec<u8>>) -> Audit {
        Audit {
            key,
            anchors: None,
            write: Mutex::new(()),
            failed: Default::default(),
            repeats: Mutex::default(),
            db_busy_since: Default::default(),
        }
    }

    pub fn with_anchors(mut self, dir: Option<PathBuf>) -> Audit {
        self.anchors = dir;
        self
    }

    pub fn anchors(&self) -> Option<&Path> {
        self.anchors.as_deref()
    }

    /// Why the audit log is failing, if a write failed within the last [`DEGRADED_FOR`].
    pub fn degraded(&self) -> Option<String> {
        let failed = self.failed.lock().unwrap_or_else(|e| e.into_inner());
        failed.as_ref().filter(|(at, _)| at.elapsed() < DEGRADED_FOR).map(|(_, why)| why.clone())
    }

    fn failed(&self, why: String) {
        *self.failed.lock().unwrap_or_else(|e| e.into_inner()) = Some((std::time::Instant::now(), why));
    }

    pub fn keyed(&self) -> bool {
        self.key.is_some()
    }

    #[allow(clippy::too_many_arguments)]
    fn hash(&self, tenant: &str, seq: i64, at: i64, e: &Entry, count: i64, prev: &str) -> String {
        let mut b = Vec::new();
        for v in [tenant, &seq.to_string(), &at.to_string(), &e.decision, &e.subject, &e.verb, &e.target, &e.reason] {
            lp(&mut b, v);
        }
        lp(&mut b, &count.to_string());
        lp(&mut b, prev);
        match &self.key {
            Some(k) => {
                let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(k).expect("HMAC takes any key length");
                m.update(&b);
                hex::encode(m.finalize().into_bytes())
            }
            None => hex::encode(Sha256::digest(&b)),
        }
    }

    /// Append one row. When the database is busy or locked (another process holds its write lock), it's tried again
    /// up to [`WRITE_TRIES`] times in all; a write that still fails marks the audit log degraded (`/healthz`).
    pub async fn write(&self, db: &Db, tenant: &str, e: &Entry, count: i64) -> anyhow::Result<()> {
        let _guard = self.write.lock().await;
        let r = retry_busy(|| self.write_once(db, tenant, e, count)).await;
        if let Err(err) = &r {
            tracing::error!("audit: a decision couldn't be written: {err}");
            self.failed(format!("the audit log can't be written: {err}"));
        }
        r
    }

    async fn write_once(&self, db: &Db, tenant: &str, e: &Entry, count: i64) -> anyhow::Result<()> {
        let last = sqlx::query("SELECT seq, hash FROM audit_log ORDER BY seq DESC LIMIT 1").fetch_optional(db).await?;
        let (seq, prev) = match last {
            Some(r) => (r.get::<i64, _>(0) + 1, r.get::<String, _>(1)),
            None => (1, GENESIS.to_string()),
        };
        let at = now_ms();
        let hash = self.hash(tenant, seq, at, e, count, &prev);
        sqlx::query(
            "INSERT INTO audit_log (tenant, seq, at, decision, subject, verb, target, reason, count, prev_hash, hash)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(tenant)
        .bind(seq)
        .bind(at)
        .bind(&e.decision)
        .bind(&e.subject)
        .bind(&e.verb)
        .bind(&e.target)
        .bind(&e.reason)
        .bind(count)
        .bind(&prev)
        .bind(&hash)
        .execute(db)
        .await?;
        Ok(())
    }

    /// Record a decision. A denial, or a request still waiting for the person (an agent asking again, the MCP gate
    /// waiting for an approval), seen again within a minute is counted, and written as one row when the minute is up
    /// ([`Audit::flush`]).
    pub async fn record(&self, db: &Db, tenant: &str, e: Entry) -> anyhow::Result<()> {
        if e.decision == "denied" || e.decision == "asked" {
            let key = format!("{tenant}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}", e.decision, e.subject, e.verb, e.target);
            let mut repeats = self.repeats.lock().await;
            if let Some(r) = repeats.get_mut(&key)
                && now_ms() - r.0 < REPEAT_WINDOW_MS
            {
                r.1 += 1;
                return Ok(());
            }
            repeats.insert(key, (now_ms(), 0, tenant.to_string(), e.clone()));
        }
        self.write(db, tenant, &e, 1).await
    }

    /// Write the counted repeats of denials whose minute is up.
    pub async fn flush(&self, db: &Db) -> anyhow::Result<()> {
        let due: Vec<(String, i64, Entry)> = {
            let mut repeats = self.repeats.lock().await;
            let now = now_ms();
            let keys: Vec<String> =
                repeats.iter().filter(|(_, r)| now - r.0 >= REPEAT_WINDOW_MS).map(|(k, _)| k.clone()).collect();
            keys.into_iter()
                .filter_map(|k| repeats.remove(&k))
                .filter(|r| r.1 > 0)
                .map(|(_, n, t, e)| (t, n, Entry { reason: format!("{} (repeated)", e.reason), ..e }))
                .collect()
        };
        for (tenant, n, e) in due {
            self.write(db, &tenant, &e, n).await?;
        }
        Ok(())
    }

    /// Write an anchor for the newest row into `dir`.
    pub async fn anchor(&self, db: &Db, dir: &Path) -> anyhow::Result<Option<PathBuf>> {
        let Some(r) =
            sqlx::query("SELECT seq, hash FROM audit_log ORDER BY seq DESC LIMIT 1").fetch_optional(db).await?
        else {
            return Ok(None);
        };
        let (seq, hash): (i64, String) = (r.get(0), r.get(1));
        std::fs::create_dir_all(dir)?;
        let prev = latest_anchor(dir)?;
        if prev.as_ref().is_some_and(|(_, a)| a["seq"].as_i64() == Some(seq)) {
            return Ok(None);
        }
        let prev_sha = prev.map(|(text, _)| hex::encode(Sha256::digest(text.as_bytes()))).unwrap_or_default();
        let at = jiff::Timestamp::now();
        let body = serde_json::json!({"seq": seq, "hash": hash, "at": at.to_string(), "prev_anchor_sha256": prev_sha});
        let path = dir.join(format!("anchor-{seq:012}.json"));
        std::fs::write(&path, serde_json::to_string_pretty(&body)?)?;
        Ok(Some(path))
    }
}

fn latest_anchor(dir: &Path) -> anyhow::Result<Option<(String, serde_json::Value)>> {
    let mut names: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| is_anchor(p)).collect(),
        Err(_) => return Ok(None),
    };
    names.sort();
    match names.last() {
        Some(p) => {
            let text = std::fs::read_to_string(p)?;
            let v = serde_json::from_str(&text)?;
            Ok(Some((text, v)))
        }
        None => Ok(None),
    }
}

pub fn is_anchor(p: &Path) -> bool {
    p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("anchor-") && n.ends_with(".json"))
}

/// Whether a line of [`verify`]'s report is a problem found, not one of its summary lines.
pub fn is_problem(line: &str) -> bool {
    !(line.starts_with("Audit chain:")
        || line.starts_with("Anchors:")
        || line.starts_with("No anchors given")
        || line.starts_with("Result:")
        || line.ends_with("aren't anchored yet"))
}

/// Check the chain (and the anchors in `anchors`, if given). Returns the report's lines and whether it's intact.
pub async fn verify(audit: &Audit, db: &Db, anchors: Option<&Path>) -> anyhow::Result<(Vec<String>, bool)> {
    let rows = sqlx::query(
        "SELECT tenant, seq, at, decision, subject, verb, target, reason, count, prev_hash, hash FROM audit_log ORDER BY seq",
    )
    .fetch_all(db)
    .await?;
    let mut report = vec![];
    let mut ok = true;
    let mut prev = GENESIS.to_string();
    let mut hashes: HashMap<i64, String> = HashMap::new();
    let mut last_seq = 0;
    for r in &rows {
        let seq: i64 = r.get(1);
        let e = Entry { decision: r.get(3), subject: r.get(4), verb: r.get(5), target: r.get(6), reason: r.get(7) };
        let (stored_prev, stored): (String, String) = (r.get(9), r.get(10));
        if seq != last_seq + 1 {
            report.push(format!("row {seq}: rows {}..{} are missing", last_seq + 1, seq - 1));
            ok = false;
        }
        if stored_prev != prev {
            report.push(format!("row {seq}: its previous hash doesn't match row {}'s hash", seq - 1));
            ok = false;
        }
        let want = audit.hash(&r.get::<String, _>(0), seq, r.get(2), &e, r.get(8), &stored_prev);
        if want != stored {
            report.push(format!("row {seq}: its content doesn't match its hash (changed, or a different key)"));
            ok = false;
        }
        hashes.insert(seq, stored.clone());
        prev = stored;
        last_seq = seq;
    }
    report.insert(
        0,
        format!(
            "Audit chain: {} rows checked, {}",
            rows.len(),
            if audit.keyed() {
                "HMAC recomputed with the key"
            } else {
                "no key: plain SHA-256, which someone able to edit the database could recompute"
            }
        ),
    );
    match anchors {
        Some(dir) => {
            let mut files: Vec<PathBuf> =
                std::fs::read_dir(dir)?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| is_anchor(p)).collect();
            files.sort();
            let mut prev_text: Option<String> = None;
            for f in &files {
                let text = std::fs::read_to_string(f)?;
                let a: serde_json::Value = serde_json::from_str(&text)?;
                let seq = a["seq"].as_i64().unwrap_or(-1);
                match hashes.get(&seq) {
                    Some(h) if a["hash"].as_str() == Some(h.as_str()) => {}
                    Some(_) => {
                        report.push(format!("{}: row {seq}'s hash has changed since it was anchored", f.display()));
                        ok = false;
                    }
                    None => {
                        report.push(format!("{}: row {seq} was anchored and is gone", f.display()));
                        ok = false;
                    }
                }
                let want_prev =
                    prev_text.as_ref().map(|t| hex::encode(Sha256::digest(t.as_bytes()))).unwrap_or_default();
                if a["prev_anchor_sha256"].as_str() != Some(want_prev.as_str()) {
                    report.push(format!("{}: doesn't follow the anchor before it", f.display()));
                    ok = false;
                }
                prev_text = Some(text);
            }
            report.push(format!("Anchors: {} checked", files.len()));
            if let Some(Ok(a)) = prev_text.as_deref().map(serde_json::from_str::<serde_json::Value>) {
                let anchored = a["seq"].as_i64().unwrap_or(0);
                if last_seq > anchored {
                    report.push(format!(
                        "rows {}..{} were written after the latest anchor and aren't anchored yet",
                        anchored + 1,
                        last_seq
                    ));
                }
            }
        }
        None => report.push("No anchors given: deleting the newest rows can't be detected.".into()),
    }
    report.push(if ok { "Result: OK".into() } else { "Result: FAILED".into() });
    Ok((report, ok))
}

/// Flush counted denials every 30 seconds, and write an anchor every hour when a directory is given.
/// Chain the entries commands queued in `audit_outbox` (membership changes made with `fleetlayer-control`), oldest
/// first. Returns how many.
pub async fn drain_outbox(audit: &Audit, db: &Db) -> anyhow::Result<usize> {
    let rows = sqlx::query(
        "SELECT id, tenant, at, decision, subject, verb, target, reason FROM audit_outbox ORDER BY id LIMIT 500",
    )
    .fetch_all(db)
    .await?;
    for r in &rows {
        let at = jiff::Timestamp::from_millisecond(r.get::<i64, _>(2)).map(|t| t.to_string()).unwrap_or_default();
        let reason: String = r.get(7);
        let e = Entry {
            decision: r.get(3),
            subject: r.get(4),
            verb: r.get(5),
            target: r.get(6),
            reason: if reason.is_empty() { format!("made {at}") } else { format!("{reason} (made {at})") },
        };
        audit.write(db, &r.get::<String, _>(1), &e, 1).await?;
        sqlx::query("DELETE FROM audit_outbox WHERE id = ?").bind(r.get::<i64, _>(0)).execute(db).await?;
    }
    Ok(rows.len())
}

pub async fn run(app: std::sync::Arc<crate::App>, anchors: Option<PathBuf>) {
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    let mut last_anchor = std::time::Instant::now() - Duration::from_secs(3600);
    loop {
        tick.tick().await;
        if let Err(e) = drain_outbox(&app.audit, &app.db).await {
            tracing::error!("audit: chaining queued membership changes: {e}");
        }
        if let Err(e) = app.audit.flush(&app.db).await {
            tracing::error!("audit: writing counted denials: {e}");
        }
        if let Some(dir) = &anchors
            && last_anchor.elapsed() >= Duration::from_secs(3600)
        {
            match app.audit.anchor(&app.db, dir).await {
                Ok(Some(p)) => tracing::info!(anchor = %p.display(), "audit anchor written"),
                Ok(None) => {}
                Err(e) => tracing::error!("audit: writing an anchor: {e}"),
            }
            last_anchor = std::time::Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Entries written before the people migration still verify after it, after a tenant rename, and with the
    /// membership changes chained after them.
    #[tokio::test]
    async fn the_chain_verifies_across_the_people_migration() {
        use sqlx::migrate::Migrator;
        let db = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        let all = sqlx::migrate!("./migrations");
        let before: Vec<_> = all.migrations.iter().filter(|m| m.version < 13).cloned().collect();
        let mut early = Migrator::new(std::path::Path::new("./migrations")).await.unwrap();
        early.migrations = before.into();
        early.run(&db).await.unwrap();
        sqlx::query("INSERT INTO nodes (tenant, name, token_hash, created_at) VALUES ('default', 'laptop', 'h', 0)")
            .execute(&db)
            .await
            .unwrap();
        let a = Audit::new(Some(b"a key of at least thirty-two characters".to_vec()));
        for i in 0..3 {
            a.record(&db, "default", e("allowed", &format!("b{i}@y"))).await.unwrap();
        }
        all.run(&db).await.unwrap();
        let admitted: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM member_agents WHERE pattern = '*@laptop'")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(admitted, 1, "existing nodes' agents stay members of default");
        crate::people::tenant_rename(&db, "default", "home", Some("Home"), "admin").await.unwrap();
        crate::people::team_add(&db, "default", "family", None, "admin").await.unwrap();
        assert_eq!(drain_outbox(&a, &db).await.unwrap(), 2);
        a.record(&db, "default", e("allowed", "c@y")).await.unwrap();
        let (report, ok) = verify(&a, &db, None).await.unwrap();
        assert!(ok, "{report:?}");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log").fetch_one(&db).await.unwrap();
        assert_eq!(n, 6);
    }

    fn e(decision: &str, target: &str) -> Entry {
        Entry {
            decision: decision.into(),
            subject: "a@x".into(),
            verb: "message".into(),
            target: target.into(),
            reason: "r".into(),
        }
    }

    #[tokio::test]
    async fn the_chain_verifies_and_catches_edits() {
        let db = crate::db::open_memory().await.unwrap();
        let a = Audit::new(Some(b"secret".to_vec()));
        for i in 0..5 {
            a.record(&db, "default", e("allowed", &format!("b{i}@y"))).await.unwrap();
        }
        let (report, ok) = verify(&a, &db, None).await.unwrap();
        assert!(ok, "{report:?}");
        assert!(report[0].contains("5 rows checked, HMAC"));
        // The triggers refuse changes through SQL.
        assert!(sqlx::query("UPDATE audit_log SET target = 'evil' WHERE seq = 3").execute(&db).await.is_err());
        assert!(sqlx::query("DELETE FROM audit_log WHERE seq = 3").execute(&db).await.is_err());
        // Someone editing the file itself (here: dropping the trigger first) is caught by the chain.
        sqlx::query("DROP TRIGGER audit_log_no_update").execute(&db).await.unwrap();
        sqlx::query("UPDATE audit_log SET target = 'evil@y' WHERE seq = 3").execute(&db).await.unwrap();
        let (report, ok) = verify(&a, &db, None).await.unwrap();
        assert!(!ok);
        assert!(report.iter().any(|l| l.starts_with("row 3: its content doesn't match")), "{report:?}");
        // A different key doesn't verify either.
        assert!(!verify(&Audit::new(Some(b"other".to_vec())), &db, None).await.unwrap().1);
    }

    #[tokio::test]
    async fn repeated_denials_become_one_counted_row_and_anchors_pin_the_chain() {
        let db = crate::db::open_memory().await.unwrap();
        let a = Audit::new(None);
        for _ in 0..10 {
            a.record(&db, "default", e("denied", "c@z")).await.unwrap();
        }
        let n: i64 = sqlx::query("SELECT COUNT(*) FROM audit_log").fetch_one(&db).await.unwrap().get(0);
        assert_eq!(n, 1, "the first is written at once, the rest counted");
        // Pretend the minute is up.
        a.repeats.lock().await.values_mut().for_each(|r| r.0 -= REPEAT_WINDOW_MS);
        a.flush(&db).await.unwrap();
        let r = sqlx::query("SELECT count, reason FROM audit_log WHERE seq = 2").fetch_one(&db).await.unwrap();
        assert_eq!((r.get::<i64, _>(0), r.get::<String, _>(1)), (9, "r (repeated)".to_string()));

        let dir = tempfile::tempdir().unwrap();
        assert!(a.anchor(&db, dir.path()).await.unwrap().is_some());
        a.record(&db, "default", e("allowed", "d@z")).await.unwrap();
        let (report, ok) = verify(&a, &db, Some(dir.path())).await.unwrap();
        assert!(ok, "{report:?}");
        assert!(report.iter().any(|l| l.contains("rows 3..3 were written after the latest anchor")), "{report:?}");
        assert!(report[0].contains("no key"));
    }

    /// Two pools on one file: `a` (the server's) and `b`, whose statements give up at once when `a` holds the lock.
    async fn two_pools() -> (tempfile::TempDir, Db, Db) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.db");
        let a = crate::db::open(&path).await.unwrap();
        let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&path).busy_timeout(Duration::ZERO);
        let b = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
        (dir, a, b)
    }

    fn entry() -> Entry {
        Entry {
            decision: "allowed".into(),
            subject: "a@laptop".into(),
            verb: "message".into(),
            target: "b@server".into(),
            reason: "test".into(),
        }
    }

    #[tokio::test]
    async fn a_write_that_finds_the_database_locked_is_tried_again() {
        let (_dir, a, b) = two_pools().await;
        let mut lock = a.acquire().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *lock).await.unwrap();
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            sqlx::query("COMMIT").execute(&mut *lock).await.unwrap();
        });
        let audit = Audit::new(None);
        audit.write(&b, "default", &entry(), 1).await.expect("written once the lock is released");
        release.await.unwrap();
        assert_eq!(audit.degraded(), None);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log").fetch_one(&a).await.unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn a_write_that_keeps_failing_marks_the_log_degraded() {
        let (_dir, a, b) = two_pools().await;
        let mut lock = a.acquire().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *lock).await.unwrap();
        let audit = Audit::new(None);
        let started = std::time::Instant::now();
        let e = audit.write(&b, "default", &entry(), 1).await.unwrap_err();
        assert!(crate::db::is_busy(&e), "{e}");
        assert!(started.elapsed() >= Duration::from_millis(1200), "tried three times: {:?}", started.elapsed());
        let why = audit.degraded().expect("degraded");
        assert!(why.starts_with("the audit log can't be written"), "{why}");
        sqlx::query("ROLLBACK").execute(&mut *lock).await.unwrap();
    }
}
