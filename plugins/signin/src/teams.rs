//! Teams from an upstream provider's groups (`docs/sso.md`, "Upstream providers", step 4b). Each upstream maps some
//! of its groups to the tenant's teams; the control plane replaces that upstream's memberships with the mapped teams
//! and keeps the ones an admin set. Nothing unmapped becomes a team, and no team is ever created.
//!
//! - **At each sign-in** through the upstream: from the ID token's groups claim (`oidc`), or the Directory API
//!   (`google`).
//! - **Every day**, for Google Workspace: every person linked to that upstream is read again, so leaving a group
//!   reaches their teams without a sign-in; a suspended or deleted account loses its provider teams and is signed out
//!   everywhere. An `oidc` provider can't be asked between sign-ins, so its teams lapse after `teams_expire_days`
//!   without one.
//! - **When Google can't be read**, people keep the teams from the last successful read for `teams_grace_hours`,
//!   then lose that upstream's teams until a read works. Failing for an hour, or no successful daily read for a day,
//!   is a `/healthz` problem.

use std::collections::{BTreeMap, HashMap};

use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::google::{Account, Directory};
use crate::store::now_ms;
use crate::upstream::Upstream;
use crate::web::App;

const HOUR_MS: i64 = 3_600_000;
/// The daily read runs this often, and again hourly while it fails.
const RESYNC_EVERY_MS: i64 = 24 * HOUR_MS;

/// Where an upstream's groups come from.
pub enum Source {
    /// A claim in the ID token (an OIDC provider's `groups`).
    Claim(String),
    /// Google Workspace's Directory API.
    Google(Box<Directory>),
}

pub struct Teams {
    pub source: Source,
    /// Group (lower-case) to the teams it maps to.
    pub map: BTreeMap<String, Vec<String>>,
    pub grace_ms: i64,
    pub expire_ms: i64,
}

/// How an upstream's reads are going, for `/healthz`.
#[derive(Debug, Clone, Default)]
pub struct State {
    /// The last daily read where every person was read.
    pub last_ok: Option<i64>,
    pub last_try: Option<i64>,
    /// Since when reads have been failing, and the latest error.
    pub failing_since: Option<i64>,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct Tracker {
    pub started: i64,
    pub states: Mutex<HashMap<String, State>>,
}

impl Tracker {
    pub fn new() -> Tracker {
        Tracker { started: now_ms(), states: Mutex::default() }
    }
}

/// The teams `groups` map to, each once.
pub fn mapped(map: &BTreeMap<String, Vec<String>>, groups: &[String]) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for g in groups {
        for t in map.get(&g.to_ascii_lowercase()).into_iter().flatten() {
            if !out.contains(t) {
                out.push(t.clone());
            }
        }
    }
    out
}

/// The groups an ID token's claim names: a list of strings, or one string.
pub fn from_claim(claims: &Map<String, Value>, claim: &str) -> Vec<String> {
    match claims.get(claim) {
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => vec![],
    }
}

impl App {
    /// Tell the control plane `person`'s teams from `u`; on success, remember when, for the grace period.
    async fn put_teams(
        &self,
        u: &Upstream,
        person: &str,
        tenant: &str,
        teams: &[String],
        read: bool,
    ) -> anyhow::Result<()> {
        let out = self.control.teams_sync(tenant, person, &u.name, teams).await?;
        if let Some(missing) = out["missing"].as_array().filter(|m| !m.is_empty()) {
            tracing::warn!(upstream = %u.name, ?missing, "groups map to teams the tenant doesn't have");
        }
        if read {
            let mut people = self.store.people.lock().await;
            if let Some(c) = people.people.get_mut(person) {
                for l in c.links.iter_mut().filter(|l| l.upstream == u.name) {
                    l.teams_read_at = Some(now_ms());
                }
                self.store.save_people(&people)?;
            }
        }
        Ok(())
    }

    /// Reading failed: past the grace period since the last good read, that upstream's teams go.
    async fn after_failed_read(&self, u: &Upstream, t: &Teams, person: &str, tenant: &str, read_at: Option<i64>) {
        if read_at.is_none_or(|at| now_ms() - at > t.grace_ms)
            && let Err(e) = self.put_teams(u, person, tenant, &[], false).await
        {
            tracing::warn!(upstream = %u.name, person, "dropping teams after the grace period: {e:#}");
        }
    }

    async fn note(&self, u: &Upstream, result: Result<(), String>, daily: bool) {
        let mut states = self.teams_tracker.states.lock().await;
        let s = states.entry(u.name.clone()).or_default();
        let now = now_ms();
        if daily {
            s.last_try = Some(now);
        }
        match result {
            Ok(()) => {
                if daily {
                    s.last_ok = Some(now);
                }
                s.failing_since = None;
                s.error = None;
            }
            Err(e) => {
                s.failing_since.get_or_insert(now);
                s.error = Some(e);
            }
        }
    }

    fn read_at(people: &crate::store::People, person: &str, upstream: &str) -> Option<i64> {
        people.people.get(person)?.links.iter().filter(|l| l.upstream == upstream).filter_map(|l| l.teams_read_at).max()
    }

    /// At a sign-in through `u`: its groups (from `claims`, or the Directory for `sub`) become teams.
    pub async fn teams_at_sign_in(
        &self,
        u: &Upstream,
        person: &str,
        tenant: &str,
        sub: &str,
        claims: &Map<String, Value>,
    ) {
        let Some(t) = &u.teams else { return };
        let groups = match &t.source {
            Source::Claim(c) => Ok(from_claim(claims, c)),
            Source::Google(d) => match d.account(sub).await {
                Ok(Account::Active(g)) => Ok(g),
                Ok(_) => Ok(vec![]),
                Err(e) => Err(format!("{e:#}")),
            },
        };
        match groups {
            Ok(g) => {
                if let Err(e) = self.put_teams(u, person, tenant, &mapped(&t.map, &g), true).await {
                    tracing::warn!(upstream = %u.name, person, "setting teams at sign-in: {e:#}");
                }
                if matches!(t.source, Source::Google(_)) {
                    self.note(u, Ok(()), false).await;
                }
            }
            Err(e) => {
                tracing::warn!(upstream = %u.name, person, "reading groups at sign-in: {e}");
                let read_at = Self::read_at(&*self.store.people.lock().await, person, &u.name);
                self.after_failed_read(u, t, person, tenant, read_at).await;
                self.note(u, Err(e), false).await;
            }
        }
    }

    /// The daily pass: read every linked person's groups again (Google), or let teams lapse (OIDC).
    pub async fn resync_teams(&self, force: bool) {
        for u in &self.upstreams {
            let Some(t) = &u.teams else { continue };
            if !force && !self.resync_due(u).await {
                continue;
            }
            let linked: Vec<(String, String, Option<i64>)> = {
                let people = self.store.people.lock().await;
                people
                    .people
                    .iter()
                    .flat_map(|(p, c)| {
                        c.links
                            .iter()
                            .filter(|l| l.upstream == u.name)
                            .map(move |l| (p.clone(), l.sub.clone(), l.teams_read_at))
                    })
                    .collect()
            };
            let mut failure: Option<String> = None;
            for (person, sub, read_at) in linked {
                let tenant = match self.control.person(&person).await {
                    Ok(Some(v)) => {
                        v.tenants.into_iter().find(|m| m.slug == u.tenant || m.tenant == u.tenant).map(|m| m.tenant)
                    }
                    Ok(None) => None,
                    Err(e) => {
                        failure = Some(format!("the control plane: {e:#}"));
                        continue;
                    }
                };
                let Some(tenant) = tenant else { continue };
                match &t.source {
                    Source::Claim(_) => {
                        if read_at.is_none_or(|at| now_ms() - at > t.expire_ms) {
                            let _ = self.put_teams(u, &person, &tenant, &[], false).await;
                        }
                    }
                    Source::Google(d) => match d.account(&sub).await {
                        Ok(Account::Active(g)) => {
                            if let Err(e) = self.put_teams(u, &person, &tenant, &mapped(&t.map, &g), true).await {
                                failure = Some(format!("{e:#}"));
                            }
                        }
                        Ok(gone) => {
                            tracing::info!(upstream = %u.name, person, ?gone, "the account is no longer active: its teams go, and it's signed out everywhere");
                            if let Err(e) = self.put_teams(u, &person, &tenant, &[], true).await {
                                failure = Some(format!("{e:#}"));
                            }
                            if let Err(e) = self.sign_out_everywhere(&person, now_ms()).await {
                                failure = Some(format!("signing {person} out: {e:#}"));
                            }
                            self.control
                                .event(&tenant, &person, "signed_out", None, &format!("{} account {gone:?}", u.name))
                                .await;
                        }
                        Err(e) => {
                            failure = Some(format!("{e:#}"));
                            self.after_failed_read(u, t, &person, &tenant, read_at).await;
                        }
                    },
                }
            }
            if let Some(e) = &failure {
                tracing::warn!(upstream = %u.name, "the daily read of groups: {e}");
            }
            self.note(u, failure.map_or(Ok(()), Err), true).await;
        }
    }

    async fn resync_due(&self, u: &Upstream) -> bool {
        let states = self.teams_tracker.states.lock().await;
        let Some(s) = states.get(&u.name) else { return true };
        let since_try = s.last_try.map_or(i64::MAX, |t| now_ms() - t);
        since_try >= RESYNC_EVERY_MS || (s.failing_since.is_some() && since_try >= HOUR_MS)
    }

    /// Problems for `/healthz`: Google groups failing to read for an hour, or no good daily read for over a day.
    pub(crate) async fn teams_problems(&self) -> Vec<String> {
        let now = now_ms();
        let states = self.teams_tracker.states.lock().await;
        let mut out = vec![];
        for u in &self.upstreams {
            let Some(Teams { source: Source::Google(_), .. }) = &u.teams else { continue };
            let s = states.get(&u.name).cloned().unwrap_or_default();
            if let Some(since) = s.failing_since
                && now - since > HOUR_MS
            {
                out.push(format!(
                    "reading {}'s groups has failed for over an hour: {}",
                    u.name,
                    s.error.as_deref().unwrap_or("")
                ));
            } else if now - s.last_ok.unwrap_or(self.teams_tracker.started) > RESYNC_EVERY_MS + 2 * HOUR_MS {
                out.push(format!("{}'s groups haven't been read again for over a day", u.name));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_map_to_teams_once_and_case_doesnt_matter() {
        let map: BTreeMap<String, Vec<String>> = [
            ("eng@acme.example".to_string(), vec!["engineering".to_string()]),
            ("leads@acme.example".to_string(), vec!["engineering".to_string(), "leads".to_string()]),
        ]
        .into();
        let groups =
            vec!["Eng@Acme.example".to_string(), "leads@acme.example".to_string(), "other@acme.example".to_string()];
        assert_eq!(mapped(&map, &groups), ["engineering", "leads"]);
        let mut c = Map::new();
        c.insert("groups".into(), serde_json::json!(["a", "b"]));
        c.insert("role".into(), serde_json::json!("x"));
        assert_eq!(from_claim(&c, "groups"), ["a", "b"]);
        assert_eq!(from_claim(&c, "role"), ["x"]);
        assert!(from_claim(&c, "none").is_empty());
    }
}
