//! Access control: who may message whom, and which sessions a node can see. Rules are ordered; the first whose
//! patterns match decides, and the tenant's default decides the rest (`allow` for a new self-hosted fleet of one
//! person; set `deny` and list what's allowed once nodes you trust less join, such as an agent that reads untrusted
//! mail).
//!
//! Subjects are `<tab>@<node>`; a sender with no session is `-@<node>`. A node may subscribe only its own sessions,
//! whatever the rules say.

use serde::Serialize;
use sqlx::Row;

use crate::db::{Db, now_ms};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Allow,
    Deny,
}

impl Action {
    pub fn parse(s: &str) -> anyhow::Result<Action> {
        match s {
            "allow" => Ok(Action::Allow),
            "deny" => Ok(Action::Deny),
            _ => anyhow::bail!("an action is allow or deny (got {s})"),
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Action::Allow => "allow",
            Action::Deny => "deny",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rule {
    pub id: i64,
    pub position: i64,
    pub from: String,
    pub to: String,
    pub action: Action,
}

/// A pattern is `*`, `<tab>@<node>`, or a resource: `tool:<name>@<node>`, `sync:<bundle>@<node>` (with `*` anywhere).
pub fn valid_pattern(p: &str) -> bool {
    p == "*" || p.contains('@') || p.starts_with("tool:") || p.starts_with("sync:")
}

/// `*` matches any run of characters; everything else matches itself.
pub fn glob(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !text.starts_with(first) || text.len() < first.len() + last.len() || !text.ends_with(last) {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

/// The decision for `from` → `to` under `rules` (in order) and `default`.
pub fn decide(rules: &[Rule], default: Action, from: &str, to: &str) -> Action {
    rules.iter().find(|r| glob(&r.from, from) && glob(&r.to, to)).map_or(default, |r| r.action)
}

pub async fn rules(db: &Db, tenant: &str) -> anyhow::Result<Vec<Rule>> {
    let rows = sqlx::query(
        "SELECT id, position, from_pattern, to_pattern, action FROM acl_rules WHERE tenant = ? ORDER BY position, id",
    )
    .bind(tenant)
    .fetch_all(db)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(Rule {
                id: r.get(0),
                position: r.get(1),
                from: r.get(2),
                to: r.get(3),
                action: Action::parse(&r.get::<String, _>(4))?,
            })
        })
        .collect()
}

pub async fn default(db: &Db, tenant: &str) -> anyhow::Result<Action> {
    let row =
        sqlx::query("SELECT acl_default FROM tenant_settings WHERE tenant = ?").bind(tenant).fetch_optional(db).await?;
    match row {
        Some(r) => Action::parse(&r.get::<String, _>(0)),
        None => Ok(Action::Allow),
    }
}

pub async fn set_default(db: &Db, tenant: &str, a: Action) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO tenant_settings (tenant, acl_default) VALUES (?, ?)
         ON CONFLICT (tenant) DO UPDATE SET acl_default = excluded.acl_default",
    )
    .bind(tenant)
    .bind(a.as_str())
    .execute(db)
    .await?;
    Ok(())
}

/// Add a rule at the end (or at `position`).
pub async fn add(db: &Db, tenant: &str, from: &str, to: &str, a: Action, position: Option<i64>) -> anyhow::Result<i64> {
    let ok = valid_pattern;
    anyhow::ensure!(ok(from) && ok(to), "patterns are <tab>@<node> (or *), with * as a wildcard");
    let position = match position {
        Some(p) => p,
        None => {
            let row = sqlx::query("SELECT COALESCE(MAX(position), 0) + 10 FROM acl_rules WHERE tenant = ?")
                .bind(tenant)
                .fetch_one(db)
                .await?;
            row.get(0)
        }
    };
    let r = sqlx::query(
        "INSERT INTO acl_rules (tenant, position, from_pattern, to_pattern, action, created_at) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(tenant)
    .bind(position)
    .bind(from)
    .bind(to)
    .bind(a.as_str())
    .bind(now_ms())
    .execute(db)
    .await?;
    Ok(r.last_insert_rowid())
}

pub async fn remove(db: &Db, tenant: &str, id: i64) -> anyhow::Result<bool> {
    let r = sqlx::query("DELETE FROM acl_rules WHERE tenant = ? AND id = ?").bind(tenant).bind(id).execute(db).await?;
    Ok(r.rows_affected() > 0)
}

/// An access-control file: the default and the rules in order.
///
/// ```yaml
/// default: allow
/// rules:
///   - { from: "*@agents-triage", to: "tool:*", action: deny }
///   - { from: "*@agents-talira", to: "*@agents", action: allow }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    pub default: String,
    #[serde(default)]
    pub rules: Vec<FileRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRule {
    pub from: String,
    pub to: String,
    pub action: String,
}

/// Make the stored rules and default match `file` exactly. Returns the changes as lines (`+ …`, `- …`, `default: …`);
/// empty when it already matched. With `check`, changes nothing.
pub async fn apply(db: &Db, tenant: &str, file: &File, check: bool) -> anyhow::Result<Vec<String>> {
    let want_default = Action::parse(&file.default)?;
    let mut want = vec![];
    for r in &file.rules {
        let ok = valid_pattern;
        anyhow::ensure!(ok(&r.from) && ok(&r.to), "patterns are <tab>@<node> (or *): {} -> {}", r.from, r.to);
        want.push((r.from.clone(), r.to.clone(), Action::parse(&r.action)?));
    }
    let have_default = default(db, tenant).await?;
    let have: Vec<(String, String, Action)> =
        rules(db, tenant).await?.into_iter().map(|r| (r.from, r.to, r.action)).collect();
    let line = |(f, t, a): &(String, String, Action)| format!("{} {f} -> {t}", a.as_str());
    let mut changes = vec![];
    if have_default != want_default {
        changes.push(format!("default: {} -> {}", have_default.as_str(), want_default.as_str()));
    }
    if have != want {
        for r in have.iter().filter(|r| !want.contains(r)) {
            changes.push(format!("- {}", line(r)));
        }
        for r in want.iter().filter(|r| !have.contains(r)) {
            changes.push(format!("+ {}", line(r)));
        }
        if changes.iter().all(|c| c.starts_with("default")) {
            changes.push("rules reordered".into());
        }
    }
    if check || changes.is_empty() {
        return Ok(changes);
    }
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM acl_rules WHERE tenant = ?").bind(tenant).execute(&mut *tx).await?;
    for (i, (f, t, a)) in want.iter().enumerate() {
        sqlx::query(
            "INSERT INTO acl_rules (tenant, position, from_pattern, to_pattern, action, created_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(tenant)
        .bind((i as i64 + 1) * 10)
        .bind(f)
        .bind(t)
        .bind(a.as_str())
        .bind(now_ms())
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "INSERT INTO tenant_settings (tenant, acl_default) VALUES (?, ?)
         ON CONFLICT (tenant) DO UPDATE SET acl_default = excluded.acl_default",
    )
    .bind(tenant)
    .bind(want_default.as_str())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(changes)
}

/// How long a session may answer a session that messaged it, whatever the rules say.
pub const REPLY_WINDOW_MS: i64 = 24 * 3600 * 1000;

/// Record that `from` messaged `to`.
pub async fn log_message(db: &Db, tenant: &str, from: &str, to: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO message_log (tenant, from_subject, to_subject, at) VALUES (?, ?, ?, ?)
         ON CONFLICT (tenant, from_subject, to_subject) DO UPDATE SET at = excluded.at",
    )
    .bind(tenant)
    .bind(from)
    .bind(to)
    .bind(now_ms())
    .execute(db)
    .await?;
    Ok(())
}

/// Whether `from` is answering: `to` messaged `from` within the reply window. Subjects are compared as the sender
/// was recorded (`<name>@<node>`), so a session addressed by its tab replies to the tab that wrote.
pub async fn replying(db: &Db, tenant: &str, from: &str, to: &str) -> anyhow::Result<bool> {
    let row = sqlx::query("SELECT at FROM message_log WHERE tenant = ? AND from_subject = ? AND to_subject = ?")
        .bind(tenant)
        .bind(to)
        .bind(from)
        .fetch_optional(db)
        .await?;
    Ok(row.is_some_and(|r| now_ms() - r.get::<i64, _>(0) < REPLY_WINDOW_MS))
}

/// Whether `from` may reach `to`.
pub async fn allowed(db: &Db, tenant: &str, from: &str, to: &str) -> anyhow::Result<bool> {
    Ok(decide(&rules(db, tenant).await?, default(db, tenant).await?, from, to) == Action::Allow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_runs_of_characters() {
        assert!(glob("*@agents-triage", "quietdeck triage@agents-triage"));
        assert!(glob("sre@*", "sre@server"));
        assert!(glob("*", "anything@x"));
        assert!(glob("a*b*c@n", "aXbYc@n"));
        assert!(!glob("a*b*c@n", "aXcYb@n"));
        assert!(glob("ci@laptop", "ci@laptop"));
        assert!(!glob("ci@laptop", "ci@laptop2"));
        assert!(!glob("*@agents", "x@agents-triage"));
    }

    #[test]
    fn the_first_matching_rule_decides_then_the_default() {
        let r = |id, from: &str, to: &str, action| Rule { id, position: id, from: from.into(), to: to.into(), action };
        let rules = vec![
            r(1, "*@agents-triage", "quietdeck@agents", Action::Allow),
            r(2, "*@agents-triage", "*", Action::Deny),
        ];
        assert_eq!(decide(&rules, Action::Allow, "quietdeck triage@agents-triage", "quietdeck@agents"), Action::Allow);
        assert_eq!(
            decide(&rules, Action::Allow, "quietdeck triage@agents-triage", "planning@nathans-mac"),
            Action::Deny
        );
        assert_eq!(decide(&rules, Action::Allow, "ci@agents", "planning@nathans-mac"), Action::Allow);
        assert_eq!(decide(&rules, Action::Deny, "ci@agents", "planning@nathans-mac"), Action::Deny);
    }

    #[tokio::test]
    async fn rules_persist_in_order() {
        let db = crate::db::open_memory().await.unwrap();
        let t = crate::db::DEFAULT_TENANT;
        assert!(allowed(&db, t, "a@x", "b@y").await.unwrap(), "a new fleet allows by default");
        add(&db, t, "*@x", "*", Action::Deny, None).await.unwrap();
        add(&db, t, "a@x", "b@y", Action::Allow, Some(1)).await.unwrap();
        assert!(allowed(&db, t, "a@x", "b@y").await.unwrap(), "position 1 comes before the deny at 10");
        assert!(!allowed(&db, t, "c@x", "b@y").await.unwrap());
        set_default(&db, t, Action::Deny).await.unwrap();
        assert!(!allowed(&db, t, "c@z", "b@y").await.unwrap());
        assert!(add(&db, t, "nope", "b@y", Action::Allow, None).await.is_err());
        // A file makes the rules match exactly, and applying it twice changes nothing.
        let f: File = yaml_serde::from_str(
            "default: allow\nrules:\n  - { from: \"*@t\", to: \"tool:*\", action: deny }\n  - { from: \"a@x\", to: \"*\", action: allow }\n",
        )
        .unwrap();
        let first = apply(&db, t, &f, true).await.unwrap();
        assert!(first.iter().any(|c| c == "default: deny -> allow"), "{first:?}");
        assert!(rules(&db, t).await.unwrap().len() == 2, "--check changes nothing");
        apply(&db, t, &f, false).await.unwrap();
        assert_eq!(rules(&db, t).await.unwrap().len(), 2);
        assert_eq!(rules(&db, t).await.unwrap()[0].from, "*@t");
        assert!(apply(&db, t, &f, false).await.unwrap().is_empty(), "idempotent");
        assert!(!allowed(&db, t, "x@t", "tool:browser@m").await.unwrap());
        // A denied sender may answer someone who wrote to it.
        assert!(!replying(&db, t, "c@x", "b@y").await.unwrap());
        log_message(&db, t, "b@y", "c@x").await.unwrap();
        assert!(replying(&db, t, "c@x", "b@y").await.unwrap());
        assert!(!replying(&db, t, "c@x", "d@y").await.unwrap());
    }
}
