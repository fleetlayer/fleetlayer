//! The control plane's store: SQLite through sqlx, migrations in `migrations/`. Every query is scoped to a tenant.

use std::path::Path;

use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

pub type Db = SqlitePool;

pub const DEFAULT_TENANT: &str = "default";

pub async fn open(path: &Path) -> anyhow::Result<Db> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);
    let db = SqlitePoolOptions::new().max_connections(8).connect_with(opts).await?;
    sqlx::migrate!("./migrations").run(&db).await?;
    Ok(db)
}

/// A database in memory: for tests, and commands that read only their own files (`acl test`).
pub async fn open_memory() -> anyhow::Result<Db> {
    let db = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await?;
    sqlx::migrate!("./migrations").run(&db).await?;
    Ok(db)
}

pub fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

pub fn hash(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

/// A random secret: 32 bytes as hex.
pub fn new_secret() -> String {
    let bytes: [u8; 32] = rand::random();
    hex::encode(bytes)
}

/// A short random ID: 8 bytes as hex.
pub fn new_id() -> String {
    let bytes: [u8; 8] = rand::random();
    hex::encode(bytes)
}

// ---------------------------------------------------------------- nodes

/// Enrol a node: returns its token, shown once. Enrolling a name again replaces its token.
pub async fn enrol(db: &Db, tenant: &str, name: &str) -> anyhow::Result<String> {
    anyhow::ensure!(valid_name(name), "a node name is letters, digits, - and _ (got {name:?})");
    let token = format!("fln_{}", new_secret());
    sqlx::query(
        "INSERT INTO nodes (tenant, name, token_hash, created_at) VALUES (?, ?, ?, ?)
         ON CONFLICT (tenant, name) DO UPDATE SET token_hash = excluded.token_hash, public_key = NULL",
    )
    .bind(tenant)
    .bind(name)
    .bind(hash(&token))
    .bind(now_ms())
    .execute(db)
    .await?;
    Ok(token)
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The node a token belongs to: (tenant, name).
pub async fn node_for_token(db: &Db, token: &str) -> anyhow::Result<Option<(String, String)>> {
    let row =
        sqlx::query("SELECT tenant, name FROM nodes WHERE token_hash = ?").bind(hash(token)).fetch_optional(db).await?;
    Ok(row.map(|r| (r.get(0), r.get(1))))
}

/// Record a node's public key the first time; afterwards, only the same key is accepted. Err names the mismatch.
pub async fn check_key(db: &Db, tenant: &str, name: &str, key: &str) -> anyhow::Result<Result<(), String>> {
    let row = sqlx::query("SELECT public_key FROM nodes WHERE tenant = ? AND name = ?")
        .bind(tenant)
        .bind(name)
        .fetch_one(db)
        .await?;
    match row.get::<Option<String>, _>(0) {
        None => {
            sqlx::query("UPDATE nodes SET public_key = ? WHERE tenant = ? AND name = ?")
                .bind(key)
                .bind(tenant)
                .bind(name)
                .execute(db)
                .await?;
            Ok(Ok(()))
        }
        Some(k) if k == key => Ok(Ok(())),
        Some(k) => Ok(Err(format!(
            "node {name} presented a different key than the one it enrolled with ({k}); if it was rebuilt, enrol it \
             again with fleetlayer-control node add {name}"
        ))),
    }
}

/// A linked node answered: it's still here.
pub async fn node_alive(db: &Db, tenant: &str, name: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE nodes SET last_seen_at = ? WHERE tenant = ? AND name = ?")
        .bind(now_ms())
        .bind(tenant)
        .bind(name)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn node_seen(
    db: &Db,
    tenant: &str,
    name: &str,
    machine: &str,
    user: &str,
    version: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE nodes SET machine = ?, user_name = ?, version = ?, last_seen_at = ? WHERE tenant = ? AND name = ?",
    )
    .bind(machine)
    .bind(user)
    .bind(version)
    .bind(now_ms())
    .bind(tenant)
    .bind(name)
    .execute(db)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeRow {
    pub name: String,
    pub machine: Option<String>,
    pub user: Option<String>,
    pub version: Option<String>,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
    pub public_key: Option<String>,
}

pub async fn nodes(db: &Db, tenant: &str) -> anyhow::Result<Vec<NodeRow>> {
    let rows = sqlx::query(
        "SELECT name, machine, user_name, version, created_at, last_seen_at, public_key FROM nodes WHERE tenant = ? ORDER BY name",
    )
    .bind(tenant)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| NodeRow {
            name: r.get(0),
            machine: r.get(1),
            user: r.get(2),
            version: r.get(3),
            created_at: r.get(4),
            last_seen_at: r.get(5),
            public_key: r.get(6),
        })
        .collect())
}

pub async fn remove_node(db: &Db, tenant: &str, name: &str) -> anyhow::Result<bool> {
    let n = sqlx::query("DELETE FROM nodes WHERE tenant = ? AND name = ?").bind(tenant).bind(name).execute(db).await?;
    sqlx::query("DELETE FROM sessions WHERE tenant = ? AND node = ?").bind(tenant).bind(name).execute(db).await?;
    Ok(n.rows_affected() > 0)
}

// ---------------------------------------------------------------- API tokens

/// Add an API token (`read`, `admin`, or `send`: a service that only sends messages, as `service:<name>`): returns it,
/// shown once. Adding a name again replaces its token.
pub async fn add_token(db: &Db, tenant: &str, name: &str, scope: &str) -> anyhow::Result<String> {
    anyhow::ensure!(valid_name(name), "a token name is letters, digits, - and _");
    anyhow::ensure!(matches!(scope, "read" | "admin" | "send"), "a scope is read, admin or send");
    let token = format!("flt_{}", new_secret());
    sqlx::query(
        "INSERT INTO api_tokens (tenant, name, token_hash, scope, created_at) VALUES (?, ?, ?, ?, ?)
         ON CONFLICT (tenant, name) DO UPDATE SET token_hash = excluded.token_hash, scope = excluded.scope",
    )
    .bind(tenant)
    .bind(name)
    .bind(hash(&token))
    .bind(scope)
    .bind(now_ms())
    .execute(db)
    .await?;
    Ok(token)
}

/// The tenant and scope of an API token.
pub async fn token_scope(db: &Db, token: &str) -> anyhow::Result<Option<(String, String)>> {
    Ok(token_info(db, token).await?.map(|(tenant, scope, _)| (tenant, scope)))
}

/// A token's tenant, scope and name.
pub async fn token_info(db: &Db, token: &str) -> anyhow::Result<Option<(String, String, String)>> {
    let row = sqlx::query("SELECT tenant, scope, name FROM api_tokens WHERE token_hash = ?")
        .bind(hash(token))
        .fetch_optional(db)
        .await?;
    Ok(row.map(|r| (r.get(0), r.get(1), r.get(2))))
}

pub async fn tokens(db: &Db, tenant: &str) -> anyhow::Result<Vec<(String, String, i64)>> {
    let rows = sqlx::query("SELECT name, scope, created_at FROM api_tokens WHERE tenant = ? ORDER BY name")
        .bind(tenant)
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().map(|r| (r.get(0), r.get(1), r.get(2))).collect())
}

pub async fn remove_token(db: &Db, tenant: &str, name: &str) -> anyhow::Result<bool> {
    let r =
        sqlx::query("DELETE FROM api_tokens WHERE tenant = ? AND name = ?").bind(tenant).bind(name).execute(db).await?;
    Ok(r.rows_affected() > 0)
}

/// The node a session was last seen on.
pub async fn session_node(db: &Db, tenant: &str, session_id: &str) -> anyhow::Result<Option<String>> {
    let row =
        sqlx::query("SELECT node FROM sessions WHERE tenant = ? AND session_id = ? ORDER BY updated_at DESC LIMIT 1")
            .bind(tenant)
            .bind(session_id)
            .fetch_optional(db)
            .await?;
    Ok(row.map(|r| r.get(0)))
}

// ---------------------------------------------------------------- sessions

/// Replace a node's sessions with the list it pushed.
pub async fn replace_sessions(db: &Db, tenant: &str, node: &str, sessions: &[serde_json::Value]) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM sessions WHERE tenant = ? AND node = ?").bind(tenant).bind(node).execute(&mut *tx).await?;
    for s in sessions {
        let Some(id) = s.get("sessionId").and_then(|v| v.as_str()) else { continue };
        sqlx::query(
            "INSERT OR REPLACE INTO sessions (tenant, node, session_id, data, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(tenant)
        .bind(node)
        .bind(id)
        .bind(s.to_string())
        .bind(now_ms())
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn upsert_session(db: &Db, tenant: &str, node: &str, s: &serde_json::Value) -> anyhow::Result<()> {
    let Some(id) = s.get("sessionId").and_then(|v| v.as_str()) else { return Ok(()) };
    sqlx::query("INSERT OR REPLACE INTO sessions (tenant, node, session_id, data, updated_at) VALUES (?, ?, ?, ?, ?)")
        .bind(tenant)
        .bind(node)
        .bind(id)
        .bind(s.to_string())
        .bind(now_ms())
        .execute(db)
        .await?;
    Ok(())
}

pub async fn remove_session(db: &Db, tenant: &str, node: &str, id: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM sessions WHERE tenant = ? AND node = ? AND session_id = ?")
        .bind(tenant)
        .bind(node)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Every session in the tenant, as (node, session JSON).
pub async fn sessions(db: &Db, tenant: &str) -> anyhow::Result<Vec<(String, serde_json::Value)>> {
    let rows = sqlx::query("SELECT node, data FROM sessions WHERE tenant = ? ORDER BY node, session_id")
        .bind(tenant)
        .fetch_all(db)
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let data: String = r.get(1);
            Some((r.get(0), serde_json::from_str(&data).ok()?))
        })
        .collect())
}

// ---------------------------------------------------------------- channels, subscriptions, endpoints, events

pub async fn set_fallback(db: &Db, tenant: &str, channel: &str, target: Option<&str>) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO channels (tenant, name, fallback) VALUES (?, ?, ?)
         ON CONFLICT (tenant, name) DO UPDATE SET fallback = excluded.fallback",
    )
    .bind(tenant)
    .bind(channel)
    .bind(target)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn fallback(db: &Db, tenant: &str, channel: &str) -> anyhow::Result<Option<String>> {
    let row = sqlx::query("SELECT fallback FROM channels WHERE tenant = ? AND name = ?")
        .bind(tenant)
        .bind(channel)
        .fetch_optional(db)
        .await?;
    Ok(row.and_then(|r| r.get(0)))
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Subscription {
    pub channel: String,
    pub subject: Option<String>,
    pub target: String,
}

pub async fn subscribe(db: &Db, tenant: &str, s: &Subscription) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO subscriptions (tenant, channel, subject, target, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(tenant)
    .bind(&s.channel)
    .bind(&s.subject)
    .bind(&s.target)
    .bind(now_ms())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn unsubscribe(db: &Db, tenant: &str, s: &Subscription) -> anyhow::Result<bool> {
    let n = sqlx::query("DELETE FROM subscriptions WHERE tenant = ? AND channel = ? AND subject IS ? AND target = ?")
        .bind(tenant)
        .bind(&s.channel)
        .bind(&s.subject)
        .bind(&s.target)
        .execute(db)
        .await?;
    Ok(n.rows_affected() > 0)
}

pub async fn subscriptions(db: &Db, tenant: &str) -> anyhow::Result<Vec<Subscription>> {
    let rows =
        sqlx::query("SELECT channel, subject, target FROM subscriptions WHERE tenant = ? ORDER BY channel, target")
            .bind(tenant)
            .fetch_all(db)
            .await?;
    Ok(rows.into_iter().map(|r| Subscription { channel: r.get(0), subject: r.get(1), target: r.get(2) }).collect())
}

/// The targets an event on `channel` goes to when it matches any of `subjects` (a GitHub workflow run matches its
/// branch and its commit): each target once.
pub async fn targets_any(db: &Db, tenant: &str, channel: &str, subjects: &[&str]) -> anyhow::Result<Vec<String>> {
    let mut out = targets(db, tenant, channel, subjects.first().copied()).await?;
    for s in subjects.iter().skip(1) {
        out.extend(targets(db, tenant, channel, Some(s)).await?);
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// The targets an event on `channel` about `subject` goes to: subscriptions to the whole channel, and those to its
/// subject.
pub async fn targets(db: &Db, tenant: &str, channel: &str, subject: Option<&str>) -> anyhow::Result<Vec<String>> {
    let rows = sqlx::query(
        "SELECT DISTINCT target FROM subscriptions WHERE tenant = ? AND channel = ? AND (subject IS NULL OR subject = ?)
         ORDER BY target",
    )
    .bind(tenant)
    .bind(channel)
    .bind(subject)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|r| r.get(0)).collect())
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
    pub id: String,
    pub label: String,
    pub channel: String,
    pub created_at: i64,
    pub event_count: i64,
    pub last_event_at: Option<i64>,
    /// GitHub's signing secret (X-Hub-Signature-256); never shown.
    #[serde(skip)]
    pub github_secret: Option<String>,
    /// Deliver every action of a workflow run, not only `completed`.
    pub all_actions: bool,
}

/// Add a webhook endpoint: returns it and its secret, shown once.
pub async fn add_endpoint(
    db: &Db,
    tenant: &str,
    label: &str,
    channel: &str,
    github_secret: Option<&str>,
    all_actions: bool,
) -> anyhow::Result<(Endpoint, String)> {
    let (id, secret) = (new_id(), new_secret());
    let now = now_ms();
    sqlx::query(
        "INSERT INTO endpoints (tenant, id, secret_hash, label, channel, created_at, github_secret, all_actions)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(tenant)
    .bind(&id)
    .bind(hash(&secret))
    .bind(label)
    .bind(channel)
    .bind(now)
    .bind(github_secret)
    .bind(all_actions)
    .execute(db)
    .await?;
    let e = Endpoint {
        id,
        label: label.into(),
        channel: channel.into(),
        created_at: now,
        event_count: 0,
        last_event_at: None,
        github_secret: github_secret.map(str::to_string),
        all_actions,
    };
    Ok((e, secret))
}

pub async fn endpoints(db: &Db, tenant: &str) -> anyhow::Result<Vec<Endpoint>> {
    let rows = sqlx::query(
        "SELECT id, label, channel, created_at, event_count, last_event_at, github_secret, all_actions FROM endpoints
         WHERE tenant = ? ORDER BY label",
    )
    .bind(tenant)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Endpoint {
            id: r.get(0),
            label: r.get(1),
            channel: r.get(2),
            created_at: r.get(3),
            event_count: r.get(4),
            last_event_at: r.get(5),
            github_secret: r.get(6),
            all_actions: r.get(7),
        })
        .collect())
}

/// The endpoint an incoming webhook names, if its secret matches: (tenant, endpoint).
pub async fn endpoint_for(db: &Db, id: &str, secret: &str) -> anyhow::Result<Option<(String, Endpoint)>> {
    let row = sqlx::query(
        "SELECT tenant, id, label, channel, created_at, event_count, last_event_at, secret_hash, github_secret, all_actions
         FROM endpoints WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(db)
    .await?;
    let Some(r) = row else { return Ok(None) };
    let stored: String = r.get(7);
    let given = hash(secret);
    if !bool::from(subtle::ConstantTimeEq::ct_eq(stored.as_bytes(), given.as_bytes())) {
        return Ok(None);
    }
    Ok(Some((
        r.get(0),
        Endpoint {
            id: r.get(1),
            label: r.get(2),
            channel: r.get(3),
            created_at: r.get(4),
            event_count: r.get(5),
            last_event_at: r.get(6),
            github_secret: r.get(8),
            all_actions: r.get(9),
        },
    )))
}

/// Set or clear an endpoint's GitHub signing secret. False when there's no such endpoint.
pub async fn set_github_secret(db: &Db, tenant: &str, id: &str, secret: Option<&str>) -> anyhow::Result<bool> {
    let r = sqlx::query("UPDATE endpoints SET github_secret = ? WHERE tenant = ? AND id = ?")
        .bind(secret)
        .bind(tenant)
        .bind(id)
        .execute(db)
        .await?;
    Ok(r.rows_affected() > 0)
}

// ---------------------------------------------------------------- workflow runs per commit

/// How long runs and commit subscriptions are kept.
pub const COMMIT_KEEP_MS: i64 = 24 * 3600 * 1000;

/// A workflow run's state, as GitHub's last action for it said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRun {
    pub repo: String,
    pub sha: String,
    pub run_id: i64,
    pub workflow: String,
    pub attempt: i64,
    pub status: String,
}

pub async fn record_run(db: &Db, tenant: &str, channel: &str, r: &CommitRun, event: Option<i64>) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO commit_runs (tenant, channel, repo, sha, run_id, workflow, attempt, status, event_id, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (tenant, channel, repo, run_id) DO UPDATE SET sha = excluded.sha, workflow = excluded.workflow,
           attempt = excluded.attempt, status = excluded.status, event_id = excluded.event_id,
           updated_at = excluded.updated_at
         -- GitHub doesn't promise order: a late 'in_progress' never undoes 'completed' of the same attempt.
         WHERE excluded.attempt > commit_runs.attempt
            OR (excluded.attempt = commit_runs.attempt AND commit_runs.status != 'completed')",
    )
    .bind(tenant)
    .bind(channel)
    .bind(&r.repo)
    .bind(&r.sha)
    .bind(r.run_id)
    .bind(&r.workflow)
    .bind(r.attempt)
    .bind(&r.status)
    .bind(event)
    .bind(now_ms())
    .execute(db)
    .await?;
    Ok(())
}

/// A commit's runs: the workflows still pending, and the stored events of those completed (oldest first).
pub async fn commit_state(
    db: &Db,
    tenant: &str,
    channel: &str,
    repo: &str,
    sha: &str,
) -> anyhow::Result<(Vec<String>, Vec<i64>)> {
    let rows = sqlx::query(
        "SELECT workflow, status, event_id FROM commit_runs WHERE tenant = ? AND channel = ? AND repo = ? AND sha = ?
         ORDER BY updated_at, run_id",
    )
    .bind(tenant)
    .bind(channel)
    .bind(repo)
    .bind(sha)
    .fetch_all(db)
    .await?;
    let (mut pending, mut done) = (vec![], vec![]);
    for r in rows {
        let (workflow, status, event): (String, String, Option<i64>) = (r.get(0), r.get(1), r.get(2));
        if status == "completed" {
            done.extend(event);
        } else {
            pending.push(workflow);
        }
    }
    Ok((pending, done))
}

/// Remove every subscription to `subject` on `channel`: the targets it had.
pub async fn drop_subject(db: &Db, tenant: &str, channel: &str, subject: &str) -> anyhow::Result<Vec<String>> {
    let rows =
        sqlx::query("DELETE FROM subscriptions WHERE tenant = ? AND channel = ? AND subject = ? RETURNING target")
            .bind(tenant)
            .bind(channel)
            .bind(subject)
            .fetch_all(db)
            .await?;
    Ok(rows.into_iter().map(|r| r.get(0)).collect())
}

/// Forget runs, and subscriptions to commits (`<repo>@<40 hex>`), older than a day.
pub async fn prune_commits(db: &Db) -> anyhow::Result<()> {
    let before = now_ms() - COMMIT_KEEP_MS;
    sqlx::query("DELETE FROM commit_runs WHERE updated_at < ?").bind(before).execute(db).await?;
    let sha = format!("*@{}", "[0-9a-f]".repeat(40));
    sqlx::query("DELETE FROM subscriptions WHERE created_at < ? AND subject GLOB ?")
        .bind(before)
        .bind(sha)
        .execute(db)
        .await?;
    Ok(())
}

/// A stored event and its source.
pub async fn event(db: &Db, id: i64) -> anyhow::Result<Option<(fleetlayer_protocol::ext::Event, String)>> {
    let row =
        sqlx::query("SELECT channel, kind, subject, summary, body, source, dedupe_key, at FROM events WHERE id = ?")
            .bind(id)
            .fetch_optional(db)
            .await?;
    Ok(row.map(|r| {
        let body: String = r.get(4);
        let e = fleetlayer_protocol::ext::Event {
            channel: r.get(0),
            kind: r.get(1),
            subject: r.get(2),
            summary: r.get(3),
            body: serde_json::from_str(&body).unwrap_or(serde_json::Value::String(body)),
            dedupe_key: r.get(6),
            at: jiff::Timestamp::from_millisecond(r.get(7)).unwrap_or_else(|_| jiff::Timestamp::now()),
        };
        (e, r.get(5))
    }))
}

pub async fn endpoint_hit(db: &Db, id: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE endpoints SET event_count = event_count + 1, last_event_at = ? WHERE id = ?")
        .bind(now_ms())
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Store an event. None when its dedupe key was seen before on that channel.
pub async fn add_event(
    db: &Db,
    tenant: &str,
    e: &fleetlayer_protocol::ext::Event,
    source: &str,
) -> anyhow::Result<Option<i64>> {
    let r = sqlx::query(
        "INSERT OR IGNORE INTO events (tenant, channel, kind, subject, summary, body, source, dedupe_key, at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(tenant)
    .bind(&e.channel)
    .bind(&e.kind)
    .bind(&e.subject)
    .bind(&e.summary)
    .bind(e.body.to_string())
    .bind(source)
    .bind(&e.dedupe_key)
    .bind(e.at.as_millisecond())
    .execute(db)
    .await?;
    Ok((r.rows_affected() > 0).then(|| r.last_insert_rowid()))
}

pub async fn event_delivered(db: &Db, id: i64, deliveries: &serde_json::Value) -> anyhow::Result<()> {
    sqlx::query("UPDATE events SET deliveries = ? WHERE id = ?")
        .bind(deliveries.to_string())
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn recent_events(db: &Db, tenant: &str, limit: i64) -> anyhow::Result<Vec<serde_json::Value>> {
    let rows = sqlx::query(
        "SELECT id, channel, kind, subject, summary, source, at, deliveries FROM events WHERE tenant = ?
         ORDER BY id DESC LIMIT ?",
    )
    .bind(tenant)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let deliveries: String = r.get(7);
            serde_json::json!({
                "id": r.get::<i64, _>(0), "channel": r.get::<String, _>(1), "kind": r.get::<String, _>(2),
                "subject": r.get::<Option<String>, _>(3), "summary": r.get::<String, _>(4),
                "source": r.get::<String, _>(5), "at": r.get::<i64, _>(6),
                "deliveries": serde_json::from_str::<serde_json::Value>(&deliveries).unwrap_or_default(),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tokens_find_their_node_and_reenrolling_replaces_the_token() {
        let db = open_memory().await.unwrap();
        let t1 = enrol(&db, DEFAULT_TENANT, "laptop").await.unwrap();
        assert_eq!(node_for_token(&db, &t1).await.unwrap(), Some(("default".into(), "laptop".into())));
        assert_eq!(check_key(&db, DEFAULT_TENANT, "laptop", "age1a").await.unwrap(), Ok(()));
        assert_eq!(check_key(&db, DEFAULT_TENANT, "laptop", "age1a").await.unwrap(), Ok(()));
        assert!(check_key(&db, DEFAULT_TENANT, "laptop", "age1b").await.unwrap().is_err());
        let t2 = enrol(&db, DEFAULT_TENANT, "laptop").await.unwrap();
        assert_eq!(
            check_key(&db, DEFAULT_TENANT, "laptop", "age1b").await.unwrap(),
            Ok(()),
            "enrolling again resets the key"
        );
        assert_eq!(node_for_token(&db, &t1).await.unwrap(), None);
        assert!(node_for_token(&db, &t2).await.unwrap().is_some());
        assert!(enrol(&db, DEFAULT_TENANT, "bad name").await.is_err());
    }

    #[tokio::test]
    async fn a_send_token_makes_its_service_a_member_of_its_tenant() {
        let db = open_memory().await.unwrap();
        let read = add_token(&db, DEFAULT_TENANT, "deck-read", "read").await.unwrap();
        let send = add_token(&db, DEFAULT_TENANT, "quietdeck", "send").await.unwrap();
        assert!(add_token(&db, DEFAULT_TENANT, "x", "write").await.is_err(), "only read, admin and send");
        assert_eq!(token_info(&db, &send).await.unwrap(), Some(("default".into(), "send".into(), "quietdeck".into())));
        assert_eq!(token_scope(&db, &read).await.unwrap(), Some(("default".into(), "read".into())));
        let before = crate::directory::Fleet::version(&db).await.unwrap();
        let fleet = crate::directory::Fleet::load(&db).await.unwrap();
        let t = fleet.tenants.first().unwrap();
        assert!(t.is_member(&fleet, "service:quietdeck"));
        assert!(!t.is_member(&fleet, "service:deck-read"), "a read token isn't a sender");
        assert!(!t.is_member(&fleet, "service:other"));
        add_token(&db, DEFAULT_TENANT, "another", "send").await.unwrap();
        assert_ne!(crate::directory::Fleet::version(&db).await.unwrap(), before, "a new send token is seen");
    }

    #[tokio::test]
    async fn subscriptions_match_whole_channels_and_subjects() {
        let db = open_memory().await.unwrap();
        let sub = |subject: Option<&str>, target: &str| Subscription {
            channel: "ci".into(),
            subject: subject.map(str::to_string),
            target: target.into(),
        };
        subscribe(&db, DEFAULT_TENANT, &sub(None, "sre@server")).await.unwrap();
        subscribe(&db, DEFAULT_TENANT, &sub(Some("repo@fix"), "fix@laptop")).await.unwrap();
        subscribe(&db, DEFAULT_TENANT, &sub(Some("repo@fix"), "fix@laptop")).await.unwrap();
        assert_eq!(targets(&db, DEFAULT_TENANT, "ci", Some("repo@fix")).await.unwrap(), ["fix@laptop", "sre@server"]);
        assert_eq!(targets(&db, DEFAULT_TENANT, "ci", Some("repo@main")).await.unwrap(), ["sre@server"]);
        assert!(unsubscribe(&db, DEFAULT_TENANT, &sub(None, "sre@server")).await.unwrap());
        assert!(targets(&db, DEFAULT_TENANT, "ci", None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_endpoint_needs_its_secret_and_events_dedupe() {
        let db = open_memory().await.unwrap();
        let (e, secret) = add_endpoint(&db, DEFAULT_TENANT, "github", "ci", None, false).await.unwrap();
        assert!(endpoint_for(&db, &e.id, &secret).await.unwrap().is_some());
        assert!(endpoint_for(&db, &e.id, "wrong").await.unwrap().is_none());
        let ev = fleetlayer_protocol::ext::Event {
            channel: "ci".into(),
            kind: "ci.failed".into(),
            subject: None,
            at: jiff::Timestamp::now(),
            dedupe_key: Some("k1".into()),
            summary: "s".into(),
            body: serde_json::Value::Null,
        };
        assert!(add_event(&db, DEFAULT_TENANT, &ev, "test").await.unwrap().is_some());
        assert!(add_event(&db, DEFAULT_TENANT, &ev, "test").await.unwrap().is_none());
    }
}
