//! People, tenants, teams and memberships (`docs/sso.md`): who belongs where. People are global; everything else is
//! per tenant. Nothing here is a credential: passkeys, upstream accounts, emails and sessions stay in the sign-in
//! service. Every change is written to the tenant's audit log through the outbox ([`log`]).

use serde::Serialize;
use sqlx::Row;

use crate::db::{Db, hash, new_secret, now_ms};

/// The team every member of a tenant is in, made with the tenant. Simple views leave it out until there's a second
/// team.
pub const EVERYONE: &str = "everyone";

/// How long an invite holds.
pub const INVITE_DAYS: i64 = 7;

/// A handle or team name: a lowercase slug of at most 64 characters, with no `@` (so it can't clash with
/// `<tab>@<node>`).
pub fn valid_slug(n: &str) -> bool {
    let mut chars = n.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && n.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

fn ensure_slug(what: &str, n: &str) -> anyhow::Result<()> {
    anyhow::ensure!(valid_slug(n), "{n:?} isn't a valid {what}: lowercase letters, digits, . _ and -, at most 64");
    Ok(())
}

fn ensure_role(role: &str) -> anyhow::Result<()> {
    anyhow::ensure!(matches!(role, "admin" | "member"), "a role is admin or member (got {role})");
    Ok(())
}

/// Random lowercase letters and digits.
fn random_id(n: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    (0..n).map(|_| ALPHABET[rand::random_range(0..ALPHABET.len())] as char).collect()
}

/// Queue an audit entry for the running control plane to chain ([`crate::audit::drain_outbox`]).
pub async fn log(db: &Db, tenant: &str, by: &str, verb: &str, target: &str, reason: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO audit_outbox (tenant, at, decision, subject, verb, target, reason) VALUES (?, ?, 'changed', ?, ?, ?, ?)",
    )
    .bind(tenant)
    .bind(now_ms())
    .bind(by)
    .bind(verb)
    .bind(target)
    .bind(reason)
    .execute(db)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------- tenants

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Tenant {
    pub id: String,
    pub slug: String,
    pub name: String,
}

/// The tenant a slug names.
pub async fn tenant(db: &Db, slug: &str) -> anyhow::Result<Tenant> {
    let row = sqlx::query("SELECT id, slug, name FROM tenants WHERE slug = ?").bind(slug).fetch_optional(db).await?;
    let r = row.ok_or_else(|| anyhow::anyhow!("no tenant called {slug} (fleetlayer-control tenant list)"))?;
    Ok(Tenant { id: r.get(0), slug: r.get(1), name: r.get(2) })
}

/// The tenant a slug names, or the deployment's first tenant (a deployment with one tenant never names it).
pub async fn tenant_or_first(db: &Db, slug: Option<&str>) -> anyhow::Result<Tenant> {
    match slug {
        Some(s) => tenant(db, s).await,
        None => tenants(db).await?.into_iter().next().ok_or_else(|| anyhow::anyhow!("this deployment has no tenant")),
    }
}

pub async fn tenants(db: &Db) -> anyhow::Result<Vec<Tenant>> {
    let rows = sqlx::query("SELECT id, slug, name FROM tenants ORDER BY created_at, slug").fetch_all(db).await?;
    Ok(rows.into_iter().map(|r| Tenant { id: r.get(0), slug: r.get(1), name: r.get(2) }).collect())
}

pub async fn tenant_add(db: &Db, slug: &str, name: &str, by: &str) -> anyhow::Result<Tenant> {
    ensure_slug("tenant slug", slug)?;
    let id = format!("t_{}", random_id(16));
    sqlx::query("INSERT INTO tenants (id, slug, name, created_at) VALUES (?, ?, ?, ?)")
        .bind(&id)
        .bind(slug)
        .bind(name)
        .bind(now_ms())
        .execute(db)
        .await
        .map_err(|e| anyhow::anyhow!("adding tenant {slug}: {e}"))?;
    sqlx::query("INSERT INTO teams (tenant, name, parent, created_at) VALUES (?, ?, NULL, ?)")
        .bind(&id)
        .bind(EVERYONE)
        .bind(now_ms())
        .execute(db)
        .await?;
    log(db, &id, by, "tenant.add", &format!("tenant:{slug}"), name).await?;
    Ok(Tenant { id, slug: slug.into(), name: name.into() })
}

/// Change a tenant's slug and name. Its ID, and so every row and audit entry, stays as it is.
pub async fn tenant_rename(db: &Db, slug: &str, new_slug: &str, name: Option<&str>, by: &str) -> anyhow::Result<()> {
    ensure_slug("tenant slug", new_slug)?;
    let t = tenant(db, slug).await?;
    let name = name.unwrap_or(&t.name);
    sqlx::query("UPDATE tenants SET slug = ?, name = ? WHERE id = ?")
        .bind(new_slug)
        .bind(name)
        .bind(&t.id)
        .execute(db)
        .await
        .map_err(|e| anyhow::anyhow!("renaming {slug}: {e}"))?;
    log(db, &t.id, by, "tenant.rename", &format!("tenant:{new_slug}"), &format!("was {slug} ({}), now {name}", t.name))
        .await
}

// ---------------------------------------------------------------- people

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Person {
    pub id: String,
    pub display_name: String,
    /// How many tenants they're a member of.
    pub tenants: i64,
    pub email: Option<String>,
}

pub async fn people(db: &Db) -> anyhow::Result<Vec<Person>> {
    let rows = sqlx::query(
        "SELECT p.id, p.display_name, (SELECT COUNT(*) FROM members m WHERE m.person = p.id AND m.removed_at IS NULL),
         p.email FROM people p ORDER BY p.created_at",
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Person { id: r.get(0), display_name: r.get(1), tenants: r.get(2), email: r.get(3) })
        .collect())
}

/// Set (or with `None`, clear) a person's email address. Services that ask for it treat it as verified, so it's an
/// operator's to set: logged in every tenant the person is in.
pub async fn person_email(db: &Db, person: &str, email: Option<&str>, by: &str) -> anyhow::Result<()> {
    anyhow::ensure!(person_exists(db, person).await?, "no person {person}");
    let email = email.map(str::trim).filter(|e| !e.is_empty());
    if let Some(e) = email {
        let (local, domain) = e.split_once('@').unwrap_or_default();
        anyhow::ensure!(
            !local.is_empty() && domain.contains('.') && !e.contains(char::is_whitespace) && e.len() <= 254,
            "{e} isn't an email address"
        );
    }
    sqlx::query("UPDATE people SET email = ? WHERE id = ?").bind(email).bind(person).execute(db).await?;
    let tenants: Vec<String> = sqlx::query_scalar("SELECT tenant FROM members WHERE person = ? AND removed_at IS NULL")
        .bind(person)
        .fetch_all(db)
        .await?;
    let reason = match email {
        Some(e) => format!("email set to {e}"),
        None => "email cleared".to_string(),
    };
    for t in tenants {
        log(db, &t, by, "person.email", &format!("person:{person}"), &reason).await?;
    }
    Ok(())
}

async fn person_exists(db: &Db, id: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query("SELECT 1 FROM people WHERE id = ?").bind(id).fetch_optional(db).await?.is_some())
}

/// A person named by ID (`p_…`) or by their handle in `tenant`.
pub async fn resolve_person(db: &Db, tenant: &str, who: &str) -> anyhow::Result<String> {
    if who.starts_with("p_") {
        anyhow::ensure!(person_exists(db, who).await?, "no person {who}");
        return Ok(who.to_string());
    }
    let handle = who.strip_prefix("person:").unwrap_or(who);
    let row = sqlx::query("SELECT person FROM members WHERE tenant = ? AND handle = ? AND removed_at IS NULL")
        .bind(tenant)
        .bind(handle)
        .fetch_optional(db)
        .await?;
    row.map(|r| r.get(0)).ok_or_else(|| anyhow::anyhow!("no member with the handle {handle} here"))
}

// ---------------------------------------------------------------- members

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Member {
    pub person: String,
    pub display_name: String,
    pub handle: String,
    pub role: String,
    pub teams: Vec<String>,
}

pub async fn members(db: &Db, tenant: &str) -> anyhow::Result<Vec<Member>> {
    let rows = sqlx::query(
        "SELECT m.person, p.display_name, m.handle, m.role FROM members m JOIN people p ON p.id = m.person
         WHERE m.tenant = ? AND m.removed_at IS NULL ORDER BY m.handle",
    )
    .bind(tenant)
    .fetch_all(db)
    .await?;
    let mut out = vec![];
    for r in rows {
        let person: String = r.get(0);
        let teams = sqlx::query("SELECT team FROM team_members WHERE tenant = ? AND person = ? ORDER BY team")
            .bind(tenant)
            .bind(&person)
            .fetch_all(db)
            .await?
            .into_iter()
            .map(|t| t.get(0))
            .collect();
        out.push(Member { person, display_name: r.get(1), handle: r.get(2), role: r.get(3), teams });
    }
    Ok(out)
}

/// Whether a handle is taken in `tenant`: by a member, a removed member, or an invite still open.
async fn handle_taken(db: &Db, tenant: &str, handle: &str) -> anyhow::Result<bool> {
    let member = sqlx::query("SELECT 1 FROM members WHERE tenant = ? AND handle = ?")
        .bind(tenant)
        .bind(handle)
        .fetch_optional(db)
        .await?
        .is_some();
    let invited = sqlx::query(
        "SELECT 1 FROM invites WHERE tenant = ? AND handle = ? AND used_at IS NULL AND revoked_at IS NULL AND expires_at > ?",
    )
    .bind(tenant)
    .bind(handle)
    .bind(now_ms())
    .fetch_optional(db)
    .await?
    .is_some();
    Ok(member || invited)
}

pub async fn member_role(db: &Db, tenant: &str, handle: &str, role: &str, by: &str) -> anyhow::Result<()> {
    ensure_role(role)?;
    let old: Option<String> =
        sqlx::query_scalar("SELECT role FROM members WHERE tenant = ? AND handle = ? AND removed_at IS NULL")
            .bind(tenant)
            .bind(handle)
            .fetch_optional(db)
            .await?;
    let Some(old) = old else { anyhow::bail!("no member with the handle {handle} here") };
    // One statement, so two admins demoting each other at once can't leave none: the second finds no other admin.
    let n = sqlx::query(
        "UPDATE members SET role = ?1 WHERE tenant = ?2 AND handle = ?3 AND removed_at IS NULL
         AND (?1 = 'admin' OR role != 'admin' OR (SELECT COUNT(*) FROM members WHERE tenant = ?2 AND role = 'admin'
              AND removed_at IS NULL AND handle != ?3) > 0)",
    )
    .bind(role)
    .bind(tenant)
    .bind(handle)
    .execute(db)
    .await?
    .rows_affected();
    anyhow::ensure!(n == 1, "{handle} is the tenant's last admin: make someone else an admin first");
    log(db, tenant, by, "member.role", &format!("person:{handle}"), &format!("{old} -> {role}")).await
}

/// Remove a member from a tenant (and its teams). The handle stays reserved.
pub async fn member_remove(db: &Db, tenant: &str, handle: &str, by: &str) -> anyhow::Result<()> {
    let person = resolve_person(db, tenant, handle).await?;
    let role: Option<String> =
        sqlx::query_scalar("SELECT role FROM members WHERE tenant = ? AND person = ? AND removed_at IS NULL")
            .bind(tenant)
            .bind(&person)
            .fetch_optional(db)
            .await?;
    let Some(role) = role else { anyhow::bail!("no member with the handle {handle} here") };
    // The last-admin check in the same statement as the removal (see member_role).
    let n = sqlx::query(
        "UPDATE members SET removed_at = ?1 WHERE tenant = ?2 AND person = ?3 AND removed_at IS NULL
         AND (role != 'admin' OR (SELECT COUNT(*) FROM members WHERE tenant = ?2 AND role = 'admin'
              AND removed_at IS NULL AND person != ?3) > 0)",
    )
    .bind(now_ms())
    .bind(tenant)
    .bind(&person)
    .execute(db)
    .await?
    .rows_affected();
    anyhow::ensure!(n == 1, "{handle} is the tenant's last admin: make someone else an admin first");
    sqlx::query("DELETE FROM team_members WHERE tenant = ? AND person = ?")
        .bind(tenant)
        .bind(&person)
        .execute(db)
        .await?;
    // Their own nodes' agents lose this tenant (they keep their other tenants); team and tenant nodes stay.
    let own: Vec<String> = sqlx::query("SELECT name FROM nodes WHERE owner = ?")
        .bind(&person)
        .fetch_all(db)
        .await?
        .into_iter()
        .map(|r| r.get(0))
        .collect();
    for node in own {
        sqlx::query("DELETE FROM member_agents WHERE tenant = ? AND pattern LIKE ? ESCAPE '\\'")
            .bind(tenant)
            .bind(format!("%@{}", node.replace('_', "\\_")))
            .execute(db)
            .await?;
    }
    log(
        db,
        tenant,
        by,
        "member.remove",
        &format!("person:{handle}"),
        &format!("removed (was {role}); the handle stays reserved"),
    )
    .await
}

/// How recent a passkey use must be for a change to people made through a delegate token (the dashboard).
pub const PASSKEY_FRESH_MS: i64 = 10 * 60_000;

/// The person just used their passkey at the sign-in service.
pub async fn passkey_used(db: &Db, person: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO passkey_uses (person, at) VALUES (?, ?) ON CONFLICT (person) DO UPDATE SET at = excluded.at",
    )
    .bind(person)
    .bind(now_ms())
    .execute(db)
    .await?;
    Ok(())
}

/// Whether the person used their passkey within `PASSKEY_FRESH_MS`.
pub async fn passkey_fresh(db: &Db, person: &str) -> anyhow::Result<bool> {
    let at: Option<i64> =
        sqlx::query_scalar("SELECT at FROM passkey_uses WHERE person = ?").bind(person).fetch_optional(db).await?;
    Ok(at.is_some_and(|at| now_ms() - at <= PASSKEY_FRESH_MS))
}

// ---------------------------------------------------------------- teams

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Team {
    pub name: String,
    pub parent: Option<String>,
    /// Handle, role, and the upstream provider that put them there (none: an admin).
    pub members: Vec<(String, String, Option<String>)>,
}

pub async fn team_add(db: &Db, tenant: &str, name: &str, parent: Option<&str>, by: &str) -> anyhow::Result<()> {
    ensure_slug("team name", name)?;
    if let Some(p) = parent {
        let found = sqlx::query("SELECT 1 FROM teams WHERE tenant = ? AND name = ?")
            .bind(tenant)
            .bind(p)
            .fetch_optional(db)
            .await?;
        anyhow::ensure!(found.is_some(), "no team called {p}");
    }
    sqlx::query("INSERT INTO teams (tenant, name, parent, created_at) VALUES (?, ?, ?, ?)")
        .bind(tenant)
        .bind(name)
        .bind(parent)
        .bind(now_ms())
        .execute(db)
        .await
        .map_err(|e| anyhow::anyhow!("adding team {name}: {e}"))?;
    log(
        db,
        tenant,
        by,
        "team.add",
        &format!("team:{name}"),
        parent.map(|p| format!("inside {p}")).as_deref().unwrap_or(""),
    )
    .await
}

pub async fn team_remove(db: &Db, tenant: &str, name: &str, by: &str) -> anyhow::Result<()> {
    anyhow::ensure!(name != EVERYONE, "every member is in the team {EVERYONE}; it can't be removed");
    let children = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM teams WHERE tenant = ? AND parent = ?")
        .bind(tenant)
        .bind(name)
        .fetch_one(db)
        .await?;
    anyhow::ensure!(children == 0, "team {name} has teams inside it: remove or move them first");
    let n = sqlx::query("DELETE FROM teams WHERE tenant = ? AND name = ?")
        .bind(tenant)
        .bind(name)
        .execute(db)
        .await?
        .rows_affected();
    anyhow::ensure!(n == 1, "no team called {name}");
    log(db, tenant, by, "team.remove", &format!("team:{name}"), "").await
}

pub async fn teams(db: &Db, tenant: &str) -> anyhow::Result<Vec<Team>> {
    let rows =
        sqlx::query("SELECT name, parent FROM teams WHERE tenant = ? ORDER BY name").bind(tenant).fetch_all(db).await?;
    let mut out = vec![];
    for r in rows {
        let name: String = r.get(0);
        let members = sqlx::query(
            "SELECT m.handle, t.role, t.source FROM team_members t JOIN members m ON m.tenant = t.tenant AND m.person = t.person
             WHERE t.tenant = ? AND t.team = ? ORDER BY m.handle",
        )
        .bind(tenant)
        .bind(&name)
        .fetch_all(db)
        .await?
        .into_iter()
        .map(|m| (m.get(0), m.get(1), m.get(2)))
        .collect();
        out.push(Team { name, parent: r.get(1), members });
    }
    Ok(out)
}

/// What a teams sync changed.
#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub struct TeamsSynced {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Mapped teams the tenant doesn't have (never created).
    pub missing: Vec<String>,
}

/// Make `person`'s memberships from `upstream` exactly `teams` (those that exist), in one transaction: rows that
/// upstream added and that no longer map go, new ones are added with it as their source, and a membership an admin
/// set is left as it is. Audited only when something changed.
pub async fn teams_sync(
    db: &Db,
    tenant: &str,
    person: &str,
    upstream: &str,
    teams: &[String],
    by: &str,
) -> anyhow::Result<TeamsSynced> {
    // It reads before it writes: a deferred transaction would fail at once with "database is locked" whenever another
    // write landed between the two (WAL mode doesn't wait there), so it takes the write lock up front.
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let member: Option<(String,)> = sqlx::query_as("SELECT handle FROM members WHERE tenant = ? AND person = ?")
        .bind(tenant)
        .bind(person)
        .fetch_optional(&mut *tx)
        .await?;
    let Some((handle,)) = member else { anyhow::bail!("{person} isn't a member of this tenant") };
    let existing: std::collections::BTreeSet<String> = sqlx::query_scalar("SELECT name FROM teams WHERE tenant = ?")
        .bind(tenant)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect();
    let rows: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT team, source FROM team_members WHERE tenant = ? AND person = ?")
            .bind(tenant)
            .bind(person)
            .fetch_all(&mut *tx)
            .await?;
    let mut want: Vec<String> = vec![];
    let mut out = TeamsSynced::default();
    for t in teams {
        if t == EVERYONE || want.contains(t) {
            continue;
        }
        if existing.contains(t) {
            want.push(t.clone());
        } else if !out.missing.contains(t) {
            out.missing.push(t.clone());
        }
    }
    for (team, source) in &rows {
        if source.as_deref() == Some(upstream) && !want.contains(team) {
            sqlx::query("DELETE FROM team_members WHERE tenant = ? AND team = ? AND person = ?")
                .bind(tenant)
                .bind(team)
                .bind(person)
                .execute(&mut *tx)
                .await?;
            out.removed.push(team.clone());
        }
    }
    for team in &want {
        // Already in it (by an admin, or this upstream before): nothing to do.
        if rows.iter().any(|(t, _)| t == team) {
            continue;
        }
        sqlx::query("INSERT INTO team_members (tenant, team, person, role, source) VALUES (?, ?, ?, 'member', ?)")
            .bind(tenant)
            .bind(team)
            .bind(person)
            .bind(upstream)
            .execute(&mut *tx)
            .await?;
        out.added.push(team.clone());
    }
    tx.commit().await?;
    for t in &out.added {
        log(db, tenant, by, "team.member", &format!("team:{t}"), &format!("person:{handle} added by {upstream}"))
            .await?;
    }
    for t in &out.removed {
        log(db, tenant, by, "team.member", &format!("team:{t}"), &format!("person:{handle} removed by {upstream}"))
            .await?;
    }
    Ok(out)
}

/// A team and every team inside it, at any depth (`team:engineering` includes `team:platform`).
#[allow(dead_code)] // decisions in the owning tenant (docs/sso.md, step 1b) use it
pub async fn team_and_children(db: &Db, tenant: &str, name: &str) -> anyhow::Result<Vec<String>> {
    let all = teams(db, tenant).await?;
    let mut out = vec![name.to_string()];
    let mut i = 0;
    while i < out.len() {
        let cur = out[i].clone();
        for t in &all {
            if t.parent.as_deref() == Some(&cur) && !out.contains(&t.name) {
                out.push(t.name.clone());
            }
        }
        i += 1;
    }
    Ok(out)
}

pub async fn team_member(
    db: &Db,
    tenant: &str,
    team: &str,
    handle: &str,
    add: bool,
    maintainer: bool,
    by: &str,
) -> anyhow::Result<()> {
    let person = resolve_person(db, tenant, handle).await?;
    anyhow::ensure!(team != EVERYONE, "every member is in the team {EVERYONE} already, and stays in it");
    if add {
        sqlx::query(
            "INSERT INTO team_members (tenant, team, person, role) VALUES (?, ?, ?, ?)
             ON CONFLICT (tenant, team, person) DO UPDATE SET role = excluded.role",
        )
        .bind(tenant)
        .bind(team)
        .bind(&person)
        .bind(if maintainer { "maintainer" } else { "member" })
        .execute(db)
        .await
        .map_err(|e| anyhow::anyhow!("no team called {team}? ({e})"))?;
    } else {
        let n = sqlx::query("DELETE FROM team_members WHERE tenant = ? AND team = ? AND person = ?")
            .bind(tenant)
            .bind(team)
            .bind(&person)
            .execute(db)
            .await?
            .rows_affected();
        anyhow::ensure!(n == 1, "{handle} isn't in team {team}");
    }
    let what = if !add {
        "removed"
    } else if maintainer {
        "added as maintainer"
    } else {
        "added"
    };
    log(
        db,
        tenant,
        by,
        if add { "team.member_add" } else { "team.member_remove" },
        &format!("person:{handle}"),
        &format!("team:{team}: {what}"),
    )
    .await
}

// ---------------------------------------------------------------- member agents

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MemberAgent {
    pub pattern: String,
    pub added_by: String,
    /// Whether another tenant admits agents this pattern may also cover. Never says which.
    pub elsewhere: bool,
}

fn agent_pattern_ok(p: &str) -> bool {
    p.rsplit_once('@').is_some_and(|(_, node)| !node.is_empty()) && !p.starts_with("person:")
}

pub async fn agent_admit(db: &Db, tenant: &str, pattern: &str, by: &str) -> anyhow::Result<()> {
    anyhow::ensure!(agent_pattern_ok(pattern), "{pattern:?} isn't an agent pattern like finances@laptop or *@laptop");
    sqlx::query(
        "INSERT INTO member_agents (tenant, pattern, added_by, added_at) VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING",
    )
    .bind(tenant)
    .bind(pattern)
    .bind(by)
    .bind(now_ms())
    .execute(db)
    .await?;
    log(db, tenant, by, "agent.admit", pattern, "").await
}

pub async fn agent_remove(db: &Db, tenant: &str, pattern: &str, by: &str) -> anyhow::Result<()> {
    let n = sqlx::query("DELETE FROM member_agents WHERE tenant = ? AND pattern = ?")
        .bind(tenant)
        .bind(pattern)
        .execute(db)
        .await?
        .rows_affected();
    anyhow::ensure!(n == 1, "{pattern} isn't admitted here");
    log(db, tenant, by, "agent.remove", pattern, "").await
}

pub async fn agents(db: &Db, tenant: &str) -> anyhow::Result<Vec<MemberAgent>> {
    let rows =
        sqlx::query("SELECT tenant, pattern, added_by FROM member_agents ORDER BY pattern").fetch_all(db).await?;
    let all: Vec<(String, String, String)> = rows.into_iter().map(|r| (r.get(0), r.get(1), r.get(2))).collect();
    Ok(all
        .iter()
        .filter(|(t, _, _)| t == tenant)
        .map(|(_, p, by)| MemberAgent {
            pattern: p.clone(),
            added_by: by.clone(),
            elsewhere: all.iter().any(|(t, q, _)| t != tenant && (crate::acl::glob(p, q) || crate::acl::glob(q, p))),
        })
        .collect())
}

// ---------------------------------------------------------------- invites

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Invite {
    pub handle: String,
    pub role: String,
    pub teams: Vec<String>,
    pub person: Option<String>,
    pub created_by: String,
    pub expires_at: i64,
}

pub struct NewInvite<'a> {
    pub handle: &'a str,
    pub role: &'a str,
    pub teams: &'a [String],
    /// An existing person: recovery, or someone with an account already. They may already be a member.
    pub person: Option<&'a str>,
}

/// Make an invite; returns its code, shown once.
pub async fn invite(db: &Db, tenant: &str, i: NewInvite<'_>, by: &str) -> anyhow::Result<String> {
    ensure_slug("handle", i.handle)?;
    ensure_role(i.role)?;
    let names: Vec<String> = teams(db, tenant).await?.into_iter().map(|t| t.name).collect();
    for t in i.teams {
        anyhow::ensure!(names.contains(t), "no team called {t}");
    }
    let recovering = match i.person {
        Some(p) => {
            anyhow::ensure!(person_exists(db, p).await?, "no person {p}");
            let current =
                sqlx::query("SELECT handle FROM members WHERE tenant = ? AND person = ? AND removed_at IS NULL")
                    .bind(tenant)
                    .bind(p)
                    .fetch_optional(db)
                    .await?
                    .map(|r| r.get::<String, _>(0));
            if let Some(h) = &current {
                anyhow::ensure!(h == i.handle, "{p} is already a member here as {h}");
            }
            current.is_some()
        }
        None => false,
    };
    if !recovering {
        anyhow::ensure!(!handle_taken(db, tenant, i.handle).await?, "the handle {} is taken here", i.handle);
    }
    let code = format!("fli_{}", new_secret());
    let now = now_ms();
    sqlx::query(
        "INSERT INTO invites (tenant, code_hash, handle, role, teams, person, created_by, created_at, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(tenant)
    .bind(hash(&code))
    .bind(i.handle)
    .bind(i.role)
    .bind(serde_json::to_string(i.teams)?)
    .bind(i.person)
    .bind(by)
    .bind(now)
    .bind(now + INVITE_DAYS * 86_400_000)
    .execute(db)
    .await?;
    let what = if recovering { "recovery" } else { i.role };
    log(db, tenant, by, "invite.create", &format!("person:{}", i.handle), what).await?;
    Ok(code)
}

/// Open invites (not used, revoked or expired).
pub async fn invites(db: &Db, tenant: &str) -> anyhow::Result<Vec<Invite>> {
    let rows = sqlx::query(
        "SELECT handle, role, teams, person, created_by, expires_at FROM invites
         WHERE tenant = ? AND used_at IS NULL AND revoked_at IS NULL AND expires_at > ? ORDER BY created_at",
    )
    .bind(tenant)
    .bind(now_ms())
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Invite {
            handle: r.get(0),
            role: r.get(1),
            teams: serde_json::from_str(&r.get::<String, _>(2)).unwrap_or_default(),
            person: r.get(3),
            created_by: r.get(4),
            expires_at: r.get(5),
        })
        .collect())
}

/// Revoke the open invites for a handle.
pub async fn invite_revoke(db: &Db, tenant: &str, handle: &str, by: &str) -> anyhow::Result<()> {
    let n = sqlx::query(
        "UPDATE invites SET revoked_at = ? WHERE tenant = ? AND handle = ? AND used_at IS NULL AND revoked_at IS NULL",
    )
    .bind(now_ms())
    .bind(tenant)
    .bind(handle)
    .execute(db)
    .await?
    .rows_affected();
    anyhow::ensure!(n > 0, "no open invite for {handle}");
    log(db, tenant, by, "invite.revoke", &format!("person:{handle}"), "").await
}

/// Who uses an invite: someone new, or a person who has an account already.
pub enum Redeemer<'a> {
    New { display_name: &'a str },
    Existing { person: &'a str },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Redeemed {
    pub tenant: String,
    pub person: String,
    pub handle: String,
    /// The invite was for someone already a member (recovery): nothing in the tenant changed.
    pub recovery: bool,
}

/// What an invite is for, if it can still be used: the sign-in page checks before anyone makes a passkey.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct InvitePeek {
    pub tenant: String,
    pub tenant_name: String,
    pub handle: String,
    pub role: String,
    /// For an existing person (recovery, or someone with an account already).
    pub for_person: bool,
}

pub async fn invite_peek(db: &Db, code: &str) -> anyhow::Result<InvitePeek> {
    let row = sqlx::query(
        "SELECT i.tenant, t.name, i.handle, i.role, i.person, i.used_at, i.revoked_at, i.expires_at
         FROM invites i JOIN tenants t ON t.id = i.tenant WHERE i.code_hash = ?",
    )
    .bind(hash(code))
    .fetch_optional(db)
    .await?
    .ok_or_else(|| anyhow::anyhow!("that invite doesn't exist"))?;
    anyhow::ensure!(row.get::<Option<i64>, _>(5).is_none(), "that invite has been used");
    anyhow::ensure!(row.get::<Option<i64>, _>(6).is_none(), "that invite was revoked: ask for a new one");
    anyhow::ensure!(row.get::<i64, _>(7) > now_ms(), "that invite has expired: ask for a new one");
    Ok(InvitePeek {
        tenant: row.get(0),
        tenant_name: row.get(1),
        handle: row.get(2),
        role: row.get(3),
        for_person: row.get::<Option<String>, _>(4).is_some(),
    })
}

pub async fn invite_redeem(db: &Db, code: &str, who: Redeemer<'_>) -> anyhow::Result<Redeemed> {
    let row = sqlx::query(
        "SELECT tenant, handle, role, teams, person, used_at, revoked_at, expires_at FROM invites WHERE code_hash = ?",
    )
    .bind(hash(code))
    .fetch_optional(db)
    .await?
    .ok_or_else(|| anyhow::anyhow!("that invite doesn't exist"))?;
    let (tenant, handle, role): (String, String, String) = (row.get(0), row.get(1), row.get(2));
    let teams: Vec<String> = serde_json::from_str(&row.get::<String, _>(3)).unwrap_or_default();
    let for_person: Option<String> = row.get(4);
    anyhow::ensure!(row.get::<Option<i64>, _>(5).is_none(), "that invite has been used");
    anyhow::ensure!(row.get::<Option<i64>, _>(6).is_none(), "that invite was revoked");
    anyhow::ensure!(row.get::<i64, _>(7) > now_ms(), "that invite has expired: ask for a new one");
    let person = match (&who, &for_person) {
        (Redeemer::Existing { person }, Some(p)) => {
            anyhow::ensure!(person == p, "that invite is for someone else");
            person.to_string()
        }
        (Redeemer::Existing { person }, None) => {
            anyhow::ensure!(person_exists(db, person).await?, "no person {person}");
            person.to_string()
        }
        (Redeemer::New { .. }, Some(p)) => p.clone(), // recovery: the sign-in attaches a new passkey to them
        // Made below, in the same transaction as using the invite: a redeem cut off half-way leaves nobody behind.
        (Redeemer::New { .. }, None) => format!("p_{}", random_id(20)),
    };
    let mut tx = db.begin().await?;
    if let (Redeemer::New { display_name }, None) = (&who, &for_person) {
        let name = display_name.trim();
        anyhow::ensure!(!name.is_empty() && name.chars().count() <= 100, "a display name is 1 to 100 characters");
        sqlx::query("INSERT INTO people (id, display_name, created_at) VALUES (?, ?, ?)")
            .bind(&person)
            .bind(name)
            .bind(now_ms())
            .execute(&mut *tx)
            .await?;
    }
    let used = sqlx::query("UPDATE invites SET used_at = ?, used_by = ? WHERE code_hash = ? AND used_at IS NULL")
        .bind(now_ms())
        .bind(&person)
        .bind(hash(code))
        .execute(&mut *tx)
        .await?
        .rows_affected();
    anyhow::ensure!(used == 1, "that invite has been used");
    let current = sqlx::query("SELECT handle FROM members WHERE tenant = ? AND person = ? AND removed_at IS NULL")
        .bind(&tenant)
        .bind(&person)
        .fetch_optional(&mut *tx)
        .await?;
    let recovery = current.is_some();
    if !recovery {
        sqlx::query(
            "INSERT INTO members (tenant, person, handle, role, added_at) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (tenant, person) DO UPDATE SET handle = excluded.handle, role = excluded.role,
                 added_at = excluded.added_at, removed_at = NULL",
        )
        .bind(&tenant)
        .bind(&person)
        .bind(&handle)
        .bind(&role)
        .bind(now_ms())
        .execute(&mut *tx)
        .await
        .map_err(|e| anyhow::anyhow!("the handle {handle} is taken: {e}"))?;
        for t in std::iter::once(&EVERYONE.to_string()).chain(teams.iter()) {
            sqlx::query("INSERT OR IGNORE INTO team_members (tenant, team, person, role) VALUES (?, ?, ?, 'member')")
                .bind(&tenant)
                .bind(t)
                .bind(&person)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    let verb = if recovery { "invite.recover" } else { "member.join" };
    log(db, &tenant, &format!("person:{handle}"), verb, &format!("person:{handle}"), &role).await?;
    Ok(Redeemed { tenant, person, handle, recovery })
}

/// A handle from a hint (an email's local part, a name): lower-case letters, digits, `.`, `_`, `-`, at most 64.
fn handle_from(hint: &str) -> String {
    let mut h: String = hint
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-') { c } else { '-' })
        .collect();
    h = h.trim_matches(|c: char| !c.is_ascii_alphanumeric()).chars().take(56).collect();
    if h.is_empty() { "member".into() } else { h }
}

/// Someone an upstream provider vouches for joins a tenant that lets anyone from its domain in (`join: {role:
/// member}` in the sign-in service's settings): a new person, a member, with a handle made from `hint` and kept
/// unique (`sam`, `sam-2`...), never reusing a handle someone had. Recorded in the tenant's audit log as `by`.
pub async fn join(db: &Db, tenant: &str, display_name: &str, hint: &str, by: &str) -> anyhow::Result<Redeemed> {
    let name = display_name.trim();
    anyhow::ensure!(!name.is_empty() && name.chars().count() <= 100, "a display name is 1 to 100 characters");
    let base = handle_from(hint);
    // The person, the membership and the team in one transaction: a handle taken meanwhile (two joins racing) rolls
    // it all back and the next handle is tried, so no person is left without a membership.
    for n in 1..1000 {
        let handle = if n == 1 { base.clone() } else { format!("{base}-{n}") };
        let taken = sqlx::query("SELECT 1 FROM members WHERE tenant = ? AND handle = ?")
            .bind(tenant)
            .bind(&handle)
            .fetch_optional(db)
            .await?
            .is_some();
        if taken {
            continue;
        }
        let person = format!("p_{}", random_id(20));
        let mut tx = db.begin().await?;
        sqlx::query("INSERT INTO people (id, display_name, created_at) VALUES (?, ?, ?)")
            .bind(&person)
            .bind(name)
            .bind(now_ms())
            .execute(&mut *tx)
            .await?;
        let inserted =
            sqlx::query("INSERT INTO members (tenant, person, handle, role, added_at) VALUES (?, ?, ?, 'member', ?)")
                .bind(tenant)
                .bind(&person)
                .bind(&handle)
                .bind(now_ms())
                .execute(&mut *tx)
                .await;
        if inserted.is_err() {
            tx.rollback().await?;
            continue;
        }
        sqlx::query("INSERT OR IGNORE INTO team_members (tenant, team, person, role) VALUES (?, ?, ?, 'member')")
            .bind(tenant)
            .bind(EVERYONE)
            .bind(&person)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        log(db, tenant, by, "member.join", &format!("person:{handle}"), "member").await?;
        return Ok(Redeemed { tenant: tenant.into(), person, handle, recovery: false });
    }
    anyhow::bail!("no free handle for {base}")
}

// ---------------------------------------------------------------- node owners

/// Who owns a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Owner {
    /// A person's own device: its agents are members of a tenant only where it admits them, and only while the person
    /// is a member there.
    Person(String),
    /// A team's node: it belongs to the team's tenant.
    Team { tenant: String, team: String },
    /// A tenant's node.
    Tenant(String),
}

impl Owner {
    pub fn stored(&self) -> String {
        match self {
            Owner::Person(p) => p.clone(),
            Owner::Team { tenant, team } => format!("team:{tenant}/{team}"),
            Owner::Tenant(t) => format!("tenant:{t}"),
        }
    }

    pub fn parse(s: &str) -> Option<Owner> {
        if let Some(rest) = s.strip_prefix("team:") {
            let (tenant, team) = rest.split_once('/')?;
            return Some(Owner::Team { tenant: tenant.into(), team: team.into() });
        }
        if let Some(t) = s.strip_prefix("tenant:") {
            return Some(Owner::Tenant(t.into()));
        }
        s.starts_with("p_").then(|| Owner::Person(s.into()))
    }

    /// The one tenant a team's or tenant's node belongs to; None for a person's.
    pub fn tenant(&self) -> Option<&str> {
        match self {
            Owner::Person(_) => None,
            Owner::Team { tenant, .. } | Owner::Tenant(tenant) => Some(tenant),
        }
    }
}

/// An owner as typed: a person's ID or handle in `tenant`, `team:<name>` (a team of `tenant`) or `tenant` (`tenant`
/// itself).
pub async fn resolve_owner(db: &Db, tenant: &str, owner: &str) -> anyhow::Result<Owner> {
    if owner == "tenant" {
        return Ok(Owner::Tenant(tenant.into()));
    }
    if let Some(team) = owner.strip_prefix("team:") {
        let found = sqlx::query("SELECT 1 FROM teams WHERE tenant = ? AND name = ?")
            .bind(tenant)
            .bind(team)
            .fetch_optional(db)
            .await?;
        anyhow::ensure!(found.is_some(), "no team called {team} here");
        return Ok(Owner::Team { tenant: tenant.into(), team: team.into() });
    }
    Ok(Owner::Person(resolve_person(db, tenant, owner).await?))
}

/// A node's owner, if it has one.
pub async fn owner_of_node(db: &Db, node: &str) -> anyhow::Result<Option<Owner>> {
    let row = sqlx::query("SELECT owner FROM nodes WHERE name = ?").bind(node).fetch_optional(db).await?;
    Ok(row.and_then(|r| r.get::<Option<String>, _>(0)).and_then(|s| Owner::parse(&s)))
}

/// Set, transfer or clear a node's owner. `by` is recorded, in the audit log of every tenant concerned.
pub async fn node_owner(db: &Db, node: &str, owner: Option<&Owner>, by: &str, tenant: &str) -> anyhow::Result<()> {
    let before = owner_of_node(db, node).await?;
    let n = sqlx::query("UPDATE nodes SET owner = ? WHERE name = ?")
        .bind(owner.map(|o| o.stored()))
        .bind(node)
        .execute(db)
        .await?
        .rows_affected();
    anyhow::ensure!(n > 0, "no node called {node}");
    let what = format!(
        "{} -> {}",
        before.as_ref().map(|o| o.stored()).unwrap_or_else(|| "nobody".into()),
        owner.map(|o| o.stored()).unwrap_or_else(|| "nobody".into())
    );
    let mut tenants = vec![tenant.to_string()];
    for o in [before.as_ref(), owner].into_iter().flatten() {
        if let Some(t) = o.tenant()
            && !tenants.iter().any(|x| x == t)
        {
            tenants.push(t.to_string());
        }
    }
    for t in tenants {
        log(db, &t, by, "node.owner", &format!("-@{node}"), &what).await?;
    }
    Ok(())
}

/// Record who enrolled a node.
pub async fn node_enrolled(db: &Db, node: &str, by: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE nodes SET enrolled_by = ? WHERE name = ?").bind(by).bind(node).execute(db).await?;
    Ok(())
}

/// Whether `agent` (`<tab>@<node>`, `-@<node>`, `plugin:<name>@<node>`) is a member of `tenant`: its node belongs to
/// the tenant or one of its teams, or the tenant admitted it and its person (if a person owns it) is a member.
#[allow(dead_code)] // decisions in the owning tenant (docs/sso.md, step 1b) use it
pub async fn agent_is_member(db: &Db, tenant: &str, agent: &str) -> anyhow::Result<bool> {
    let node = agent.rsplit_once('@').map(|(_, n)| n).unwrap_or_default();
    let owner = owner_of_node(db, node).await?;
    if owner.as_ref().and_then(|o| o.tenant()) == Some(tenant) {
        return Ok(true);
    }
    let rows = sqlx::query("SELECT pattern FROM member_agents WHERE tenant = ?").bind(tenant).fetch_all(db).await?;
    if !rows.iter().any(|r| crate::acl::glob(&r.get::<String, _>(0), agent)) {
        return Ok(false);
    }
    Ok(match owner {
        Some(Owner::Person(p)) => is_member(db, tenant, &p).await?,
        _ => true,
    })
}

#[allow(dead_code)] // decisions in the owning tenant (docs/sso.md, step 1b) use it
async fn is_member(db: &Db, tenant: &str, person: &str) -> anyhow::Result<bool> {
    Ok(sqlx::query("SELECT 1 FROM members WHERE tenant = ? AND person = ? AND removed_at IS NULL")
        .bind(tenant)
        .bind(person)
        .fetch_optional(db)
        .await?
        .is_some())
}

/// Remove a person from the whole deployment: from every tenant and team, and their own nodes' tokens stop working.
/// Team and tenant nodes, even ones they enrolled, are untouched. Nothing is deleted.
pub async fn person_remove(db: &Db, person: &str, by: &str) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(person_exists(db, person).await?, "no person {person}");
    let tenants: Vec<(String, String)> =
        sqlx::query("SELECT tenant, handle FROM members WHERE person = ? AND removed_at IS NULL")
            .bind(person)
            .fetch_all(db)
            .await?
            .into_iter()
            .map(|r| (r.get(0), r.get(1)))
            .collect();
    for (t, handle) in &tenants {
        member_remove(db, t, handle, by).await?;
    }
    let nodes: Vec<String> = sqlx::query("SELECT name FROM nodes WHERE owner = ?")
        .bind(person)
        .fetch_all(db)
        .await?
        .into_iter()
        .map(|r| r.get(0))
        .collect();
    for n in &nodes {
        // A token nobody knows: the node is offline until an admin enrols it again for someone.
        sqlx::query("UPDATE nodes SET token_hash = ? WHERE name = ?")
            .bind(hash(&new_secret()))
            .bind(n)
            .execute(db)
            .await?;
        for (t, _) in &tenants {
            log(db, t, by, "node.revoke", &format!("-@{n}"), "its owner was removed from the deployment").await?;
        }
    }
    Ok(nodes)
}

/// One change to a tenant's people, invites or teams, as the admin pages (the dashboard's, and the sign-in service's)
/// ask for it: `action` and its fields.
#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct Change {
    #[serde(default)]
    pub handle: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub teams: Vec<String>,
    #[serde(default)]
    pub team: Option<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub add: Option<bool>,
    #[serde(default)]
    pub maintainer: Option<bool>,
}

#[derive(Debug)]
pub enum ApplyError {
    NoSuchAction,
    Bad(String),
}

fn need<'a>(v: &'a Option<String>, what: &str) -> Result<&'a str, ApplyError> {
    v.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| ApplyError::Bad(format!("{what} is needed")))
}

/// Make one change (`invite`, `invite_revoke`, `member_role`, `member_remove`, `team_add`, `team_remove`,
/// `team_member`) with the same functions as the command line, recorded as `by`. Answers `{code}` for an invite.
pub async fn apply(db: &Db, tenant: &str, action: &str, c: &Change, by: &str) -> Result<serde_json::Value, ApplyError> {
    let bad = |e: anyhow::Error| ApplyError::Bad(e.to_string());
    Ok(match action {
        "invite" => {
            let i = NewInvite {
                handle: need(&c.handle, "handle")?,
                role: c.role.as_deref().unwrap_or("member"),
                teams: &c.teams,
                person: None,
            };
            serde_json::json!({"code": invite(db, tenant, i, by).await.map_err(bad)?})
        }
        "invite_revoke" => {
            invite_revoke(db, tenant, need(&c.handle, "handle")?, by).await.map_err(bad)?;
            serde_json::json!({})
        }
        "member_role" => {
            member_role(db, tenant, need(&c.handle, "handle")?, need(&c.role, "role")?, by).await.map_err(bad)?;
            serde_json::json!({})
        }
        "member_remove" => {
            member_remove(db, tenant, need(&c.handle, "handle")?, by).await.map_err(bad)?;
            serde_json::json!({})
        }
        "team_add" => {
            let parent = c.parent.as_deref().filter(|p| !p.is_empty());
            team_add(db, tenant, need(&c.team, "team")?, parent, by).await.map_err(bad)?;
            serde_json::json!({})
        }
        "team_remove" => {
            team_remove(db, tenant, need(&c.team, "team")?, by).await.map_err(bad)?;
            serde_json::json!({})
        }
        "team_member" => {
            let (team, handle) = (need(&c.team, "team")?, need(&c.handle, "handle")?);
            team_member(db, tenant, team, handle, c.add.unwrap_or(true), c.maintainer.unwrap_or(false), by)
                .await
                .map_err(bad)?;
            serde_json::json!({})
        }
        _ => return Err(ApplyError::NoSuchAction),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;

    #[tokio::test]
    async fn the_default_tenant_exists_and_renaming_keeps_its_id() {
        let db = open_memory().await.unwrap();
        let t = tenant(&db, "default").await.unwrap();
        assert_eq!(t.id, "default");
        tenant_rename(&db, "default", "home", Some("Home"), "admin").await.unwrap();
        let t = tenant(&db, "home").await.unwrap();
        assert_eq!((t.id.as_str(), t.name.as_str()), ("default", "Home"));
        assert!(tenant(&db, "default").await.is_err());
        assert!(tenant_rename(&db, "home", "Bad Slug", None, "admin").await.is_err());
    }

    #[tokio::test]
    async fn a_redeem_that_fails_part_way_leaves_no_person_behind() {
        let db = open_memory().await.unwrap();
        let acme = tenant_add(&db, "acme", "Acme", "admin").await.unwrap();
        let first = invite(&db, &acme.id, NewInvite { handle: "b", role: "member", teams: &[], person: None }, "admin")
            .await
            .unwrap();
        let second =
            invite(&db, &acme.id, NewInvite { handle: "c", role: "member", teams: &[], person: None }, "admin")
                .await
                .unwrap();
        let peek = invite_peek(&db, &first).await.unwrap();
        assert_eq!((peek.tenant_name.as_str(), peek.handle.as_str(), peek.for_person), ("Acme", "b", false));
        invite_redeem(&db, &first, Redeemer::New { display_name: "B" }).await.unwrap();
        assert!(invite_peek(&db, &first).await.unwrap_err().to_string().contains("used"));
        assert!(invite_peek(&db, "nope").await.is_err());
        // Someone takes the handle after the invite was made: the redeem fails inside its transaction.
        let b: String =
            sqlx::query_scalar("SELECT person FROM members WHERE handle = 'b'").fetch_one(&db).await.unwrap();
        sqlx::query("UPDATE members SET handle = 'c' WHERE person = ?").bind(&b).execute(&db).await.unwrap();
        let before = people(&db).await.unwrap().len();
        let e = invite_redeem(&db, &second, Redeemer::New { display_name: "B too" }).await.unwrap_err();
        assert!(e.to_string().contains("taken"), "{e}");
        assert_eq!(people(&db).await.unwrap().len(), before, "the new person went with the failed redeem");
    }

    #[tokio::test]
    async fn one_person_in_two_tenants_with_different_handles_and_roles() {
        let db = open_memory().await.unwrap();
        let acme = tenant_add(&db, "acme", "Acme", "admin").await.unwrap();
        let home = tenant(&db, "default").await.unwrap();
        let code = invite(&db, &acme.id, NewInvite { handle: "a", role: "admin", teams: &[], person: None }, "admin")
            .await
            .unwrap();
        let r = invite_redeem(&db, &code, Redeemer::New { display_name: "A" }).await.unwrap();
        assert!(!r.recovery);
        assert!(invite_redeem(&db, &code, Redeemer::New { display_name: "A" }).await.is_err(), "used once");
        let code2 = invite(
            &db,
            &home.id,
            NewInvite { handle: "sam", role: "member", teams: &[], person: Some(&r.person) },
            "admin",
        )
        .await
        .unwrap();
        let r2 = invite_redeem(&db, &code2, Redeemer::Existing { person: &r.person }).await.unwrap();
        assert_eq!(r2.person, r.person, "one global person");
        assert_eq!(teams(&db, &acme.id).await.unwrap()[0].name, EVERYONE, "a new tenant has the everyone team");
        let acme_members = members(&db, &acme.id).await.unwrap();
        assert_eq!(acme_members[0].teams, [EVERYONE]);
        let home_members = members(&db, &home.id).await.unwrap();
        assert_eq!((acme_members[0].handle.as_str(), acme_members[0].role.as_str()), ("a", "admin"));
        assert_eq!((home_members[0].handle.as_str(), home_members[0].role.as_str()), ("sam", "member"));
        assert_eq!(people(&db).await.unwrap()[0].tenants, 2);
    }

    #[tokio::test]
    async fn handles_stay_reserved_and_the_last_admin_stays() {
        let db = open_memory().await.unwrap();
        let t = "default";
        let a =
            invite(&db, t, NewInvite { handle: "a", role: "admin", teams: &[], person: None }, "admin").await.unwrap();
        invite_redeem(&db, &a, Redeemer::New { display_name: "A" }).await.unwrap();
        let b =
            invite(&db, t, NewInvite { handle: "b", role: "member", teams: &[], person: None }, "admin").await.unwrap();
        invite_redeem(&db, &b, Redeemer::New { display_name: "B" }).await.unwrap();
        assert!(member_remove(&db, t, "a", "admin").await.unwrap_err().to_string().contains("last admin"));
        member_remove(&db, t, "b", "admin").await.unwrap();
        let again = invite(&db, t, NewInvite { handle: "b", role: "member", teams: &[], person: None }, "admin").await;
        assert!(again.unwrap_err().to_string().contains("taken"), "a removed member's handle isn't reused");
        assert!(
            invite(&db, t, NewInvite { handle: "a", role: "member", teams: &[], person: None }, "x").await.is_err()
        );
    }

    #[tokio::test]
    async fn recovery_invites_change_nothing_in_the_tenant() {
        let db = open_memory().await.unwrap();
        let t = "default";
        let a =
            invite(&db, t, NewInvite { handle: "a", role: "admin", teams: &[], person: None }, "admin").await.unwrap();
        let r = invite_redeem(&db, &a, Redeemer::New { display_name: "A" }).await.unwrap();
        let rec =
            invite(&db, t, NewInvite { handle: "a", role: "admin", teams: &[], person: Some(&r.person) }, "admin")
                .await
                .unwrap();
        let rr = invite_redeem(&db, &rec, Redeemer::New { display_name: "ignored" }).await.unwrap();
        assert!(rr.recovery);
        assert_eq!(rr.person, r.person);
        assert_eq!(people(&db).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn nested_teams_and_their_members() {
        let db = open_memory().await.unwrap();
        let t = "default";
        team_add(&db, t, "engineering", None, "admin").await.unwrap();
        team_add(&db, t, "platform", Some("engineering"), "admin").await.unwrap();
        assert!(team_add(&db, t, "x", Some("nope"), "admin").await.is_err());
        let a = invite(
            &db,
            t,
            NewInvite { handle: "a", role: "member", teams: &["platform".into()], person: None },
            "admin",
        )
        .await
        .unwrap();
        invite_redeem(&db, &a, Redeemer::New { display_name: "A" }).await.unwrap();
        assert_eq!(members(&db, t).await.unwrap()[0].teams, ["everyone", "platform"]);
        assert_eq!(team_and_children(&db, t, "engineering").await.unwrap(), ["engineering", "platform"]);
        team_member(&db, t, "engineering", "a", true, true, "admin").await.unwrap();
        let teams = teams(&db, t).await.unwrap();
        assert_eq!(teams[0].members, [("a".to_string(), "maintainer".to_string(), None)]);
        assert_eq!(teams[1].name, EVERYONE);
        assert!(team_remove(&db, t, EVERYONE, "admin").await.is_err());
        assert!(team_remove(&db, t, "engineering", "admin").await.unwrap_err().to_string().contains("inside"));
        team_remove(&db, t, "platform", "admin").await.unwrap();
    }

    #[tokio::test]
    async fn agents_are_admitted_per_tenant_and_elsewhere_is_only_a_flag() {
        let db = open_memory().await.unwrap();
        let acme = tenant_add(&db, "acme", "Acme", "admin").await.unwrap();
        agent_admit(&db, "default", "finances@laptop", "admin").await.unwrap();
        agent_admit(&db, "default", "recipes@laptop", "admin").await.unwrap();
        agent_admit(&db, &acme.id, "*@laptop", "admin").await.unwrap();
        assert!(agent_admit(&db, &acme.id, "person:a", "admin").await.is_err());
        let home = agents(&db, "default").await.unwrap();
        assert!(home.iter().all(|a| a.elsewhere));
        assert!(agent_is_member(&db, &acme.id, "billing@laptop").await.unwrap());
        assert!(!agent_is_member(&db, "default", "billing@laptop").await.unwrap());
        agent_remove(&db, "default", "recipes@laptop", "admin").await.unwrap();
        assert!(agent_remove(&db, "default", "recipes@laptop", "admin").await.is_err());
    }

    #[tokio::test]
    async fn joining_through_an_upstream_makes_a_member_with_a_unique_handle() {
        let db = open_memory().await.unwrap();
        let a = super::join(&db, "default", "Sam Brown", "sam", "signin:signin via google").await.unwrap();
        assert_eq!(a.handle, "sam");
        let b = super::join(&db, "default", "Sam Other", "Sam", "signin:signin via google").await.unwrap();
        assert_eq!(b.handle, "sam-2");
        assert_ne!(a.person, b.person);
        // A removed member's handle stays theirs.
        member_remove(&db, "default", "sam", "admin").await.unwrap();
        assert_eq!(super::join(&db, "default", "S", "sam", "x").await.unwrap().handle, "sam-3");
        assert_eq!(handle_from("Ünïcode Name+tag"), "n-code-name-tag");
        assert_eq!(handle_from("sam.brown"), "sam.brown");
        let role: String =
            sqlx::query_scalar("SELECT role FROM members WHERE handle = 'sam-2'").fetch_one(&db).await.unwrap();
        assert_eq!(role, "member");
    }

    #[tokio::test]
    async fn a_providers_groups_replace_its_own_team_memberships_and_leave_an_admins_alone() {
        let db = open_memory().await.unwrap();
        let t = "default";
        for team in ["engineering", "support", "ops"] {
            team_add(&db, t, team, None, "admin").await.unwrap();
        }
        let sam = super::join(&db, t, "Sam", "sam", "x").await.unwrap().person;
        team_member(&db, t, "ops", "sam", true, false, "admin").await.unwrap();
        let sync = |teams: &[&str]| {
            let teams: Vec<String> = teams.iter().map(|s| s.to_string()).collect();
            let db = db.clone();
            let sam = sam.clone();
            async move {
                teams_sync(&db, t, &sam, "acme-google", &teams, "signin:s via upstream acme-google").await.unwrap()
            }
        };
        let teams_of = || {
            let db = db.clone();
            async move { members(&db, t).await.unwrap()[0].teams.clone() }
        };
        let audited = || {
            let db = db.clone();
            async move {
                sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM audit_outbox WHERE verb = 'team.member'")
                    .fetch_one(&db)
                    .await
                    .unwrap()
            }
        };

        let r = sync(&["engineering", "ops", "nope", "everyone"]).await;
        assert_eq!(r.added, ["engineering"], "ops was the admin's already; everyone is everyone's");
        assert_eq!(r.missing, ["nope"], "never created");
        assert_eq!(teams_of().await, ["engineering", "everyone", "ops"]);
        let before = audited().await;
        assert_eq!(sync(&["engineering", "ops"]).await, TeamsSynced::default(), "nothing changed");
        assert_eq!(audited().await, before, "and nothing audited");

        // Out of the group: the provider's membership goes, the admin's stays even when it no longer maps.
        let r = sync(&["support"]).await;
        assert_eq!((r.added, r.removed), (vec!["support".to_string()], vec!["engineering".to_string()]));
        assert_eq!(teams_of().await, ["everyone", "ops", "support"]);
        let listed = teams(&db, t).await.unwrap();
        let support = listed.iter().find(|x| x.name == "support").unwrap();
        assert_eq!(support.members[0].2.as_deref(), Some("acme-google"));
        let ops = listed.iter().find(|x| x.name == "ops").unwrap();
        assert_eq!(ops.members[0].2, None);
        // Another provider's memberships are its own business.
        teams_sync(&db, t, &sam, "okta", &["engineering".into()], "x").await.unwrap();
        sync(&[]).await;
        assert_eq!(teams_of().await, ["engineering", "everyone", "ops"]);
        // Not a member: refused.
        assert!(teams_sync(&db, t, "p_nobody", "okta", &[], "x").await.is_err());
    }

    #[tokio::test]
    async fn upgrading_admits_nodes_no_tenant_admitted() {
        use sqlx::migrate::Migrator;
        let db = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        let all = sqlx::migrate!("./migrations");
        let mut early = Migrator::new(std::path::Path::new("./migrations")).await.unwrap();
        early.migrations = all.migrations.iter().filter(|m| m.version < 14).cloned().collect::<Vec<_>>().into();
        early.run(&db).await.unwrap();
        for (name, owner) in [("web", None), ("shared", Some("team:default/everyone")), ("laptop", None)] {
            sqlx::query("INSERT INTO nodes (tenant, name, token_hash, created_at, owner) VALUES ('default', ?, 'h-' || ?, 0, ?)")
                .bind(name)
                .bind(name)
                .bind(owner)
                .execute(&db)
                .await
                .unwrap();
        }
        agent_admit(&db, "default", "work@laptop", "admin").await.unwrap();
        all.run(&db).await.unwrap();
        let patterns: Vec<String> =
            sqlx::query_scalar("SELECT pattern FROM member_agents ORDER BY pattern").fetch_all(&db).await.unwrap();
        assert_eq!(
            patterns,
            ["*@web", "work@laptop"],
            "a team's node needs no admitting, an admitted one keeps its rows"
        );
        let logged: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_outbox WHERE subject = 'migration'")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(logged, 1);
    }

    async fn join(db: &Db, tenant: &str, handle: &str, role: &str) -> String {
        let code = invite(db, tenant, NewInvite { handle, role, teams: &[], person: None }, "admin").await.unwrap();
        invite_redeem(db, &code, Redeemer::New { display_name: handle }).await.unwrap().person
    }

    #[tokio::test]
    async fn a_team_node_stays_when_the_person_who_enrolled_it_leaves() {
        let db = open_memory().await.unwrap();
        let acme = tenant_add(&db, "acme", "Acme", "admin").await.unwrap().id;
        join(&db, &acme, "a", "admin").await;
        let c = join(&db, &acme, "c", "admin").await;
        team_add(&db, &acme, "engineering", None, "admin").await.unwrap();
        crate::db::enrol(&db, "default", "build-vm").await.unwrap();
        crate::db::enrol(&db, "default", "c-laptop").await.unwrap();
        let team = resolve_owner(&db, &acme, "team:engineering").await.unwrap();
        node_owner(&db, "build-vm", Some(&team), "person:c", &acme).await.unwrap();
        node_enrolled(&db, "build-vm", "person:c").await.unwrap();
        node_owner(&db, "c-laptop", Some(&Owner::Person(c.clone())), "admin", &acme).await.unwrap();
        agent_admit(&db, &acme, "*@c-laptop", "admin").await.unwrap();
        assert!(agent_is_member(&db, &acme, "ci@build-vm").await.unwrap(), "a team's node is its tenant's");
        assert!(!agent_is_member(&db, "default", "ci@build-vm").await.unwrap(), "and no other tenant's");
        assert!(agent_is_member(&db, &acme, "work@c-laptop").await.unwrap());
        // C leaves Acme: C's own laptop loses Acme, the team's VM doesn't.
        member_remove(&db, &acme, "c", "person:a").await.unwrap();
        assert!(!agent_is_member(&db, &acme, "work@c-laptop").await.unwrap());
        assert!(agent_is_member(&db, &acme, "ci@build-vm").await.unwrap());
        assert_eq!(owner_of_node(&db, "build-vm").await.unwrap(), Some(team));
    }

    #[tokio::test]
    async fn removing_a_person_from_the_deployment_takes_their_own_nodes_offline() {
        let db = open_memory().await.unwrap();
        join(&db, "default", "a", "admin").await;
        let b = join(&db, "default", "b", "member").await;
        let token = crate::db::enrol(&db, "default", "b-laptop").await.unwrap();
        let tenant_token = crate::db::enrol(&db, "default", "house-vm").await.unwrap();
        node_owner(&db, "b-laptop", Some(&Owner::Person(b.clone())), "admin", "default").await.unwrap();
        node_owner(&db, "house-vm", Some(&Owner::Tenant("default".into())), "admin", "default").await.unwrap();
        assert_eq!(person_remove(&db, &b, "admin").await.unwrap(), ["b-laptop"]);
        assert!(crate::db::node_for_token(&db, &token).await.unwrap().is_none(), "b's token no longer works");
        assert!(crate::db::node_for_token(&db, &tenant_token).await.unwrap().is_some());
        assert!(members(&db, "default").await.unwrap().iter().all(|m| m.handle != "b"));
    }

    #[tokio::test]
    async fn every_change_is_queued_for_the_audit_log() {
        let db = open_memory().await.unwrap();
        team_add(&db, "default", "support", None, "admin").await.unwrap();
        tenant_rename(&db, "default", "home", None, "admin").await.unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_outbox WHERE tenant = 'default'")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 2);
    }
}
