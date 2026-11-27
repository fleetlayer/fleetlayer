//! Signing out of every service (`docs/sso.md`, "Sessions and signing out", step 2c): OpenID Connect Back-Channel
//! Logout 1.0. Each sign-in session remembers which services it signed in to; when it ends (the person signs out, or
//! is signed out everywhere), each of those with a `backchannel_logout_uri` is sent a logout token naming the
//! session (`sid`) and the person (`sub`), and ends its own session for them.
//!
//! A service that's down when that happens would otherwise carry on honouring the session when it comes back, so
//! the logouts wait in `logouts.json` and are retried, backing off to once an hour, until each is delivered or the
//! session would have expired anyway. One undelivered for more than an hour is a problem in `/healthz`: a sign-out
//! that doesn't arrive is a hole, not noise.
//!
//! **Signing out everywhere** ends every session a person has, sends their logouts, and drops their access tokens.
//! The person does it from the sign-in page; an operator, for an account that may be in someone else's hands, with
//! `fleetlayer-signin signout --dir <folder> --person <id>`, which writes `revocations.json` for the running service
//! to act on within seconds.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::store::{Session, now_ms, sha256_hex};
use crate::web::App;

/// The `events` member a logout token must carry.
pub const EVENT: &str = "http://schemas.openid.net/event/backchannel-logout";
/// Retries back off from this to [`MAX_BACKOFF_MS`].
const FIRST_BACKOFF_MS: i64 = 30_000;
const MAX_BACKOFF_MS: i64 = 3_600_000;
/// Undelivered for longer than this, it's a problem.
pub const LATE_MS: i64 = 3_600_000;

/// A session's ID as services see it (the `sid` claim): derived from the key it's kept under, itself the SHA-256 of
/// the cookie, so it says nothing about the cookie and needs no field of its own.
pub fn sid(key: &str) -> String {
    sha256_hex(&format!("{key}:sid"))[..32].to_string()
}

/// One logout to send.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Logout {
    pub client: String,
    pub sub: String,
    pub sid: String,
    pub queued_at: i64,
    /// When the session would have ended anyway: after this there's nothing left to sign out of.
    pub give_up_at: i64,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub next_at: i64,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Outbox {
    #[serde(default)]
    pub logouts: Vec<Logout>,
}

/// People signed out everywhere by the operator: person ID to when. Sessions started before then end.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Revocations {
    #[serde(default)]
    pub people: BTreeMap<String, i64>,
}

/// `fleetlayer-signin signout --dir <folder> --person <id>`: sign someone out everywhere from the command line.
pub fn revoke(dir: &Path, person: &str, now: i64) -> anyhow::Result<()> {
    let path = dir.join("revocations.json");
    let mut r: Revocations = match std::fs::read(&path) {
        Ok(b) => serde_json::from_slice(&b)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Revocations::default(),
        Err(e) => anyhow::bail!("{}: {e}", path.display()),
    };
    r.people.insert(person.to_string(), now);
    crate::store::write_private(&path, &serde_json::to_vec_pretty(&r)?)
}

fn backoff(attempts: u32) -> i64 {
    FIRST_BACKOFF_MS.saturating_mul(1i64 << attempts.min(20)).min(MAX_BACKOFF_MS)
}

impl App {
    /// The logouts a session that just ended owes: one per service it signed in to that takes them.
    pub(crate) fn logouts_for(&self, key: &str, s: &Session, now: i64) -> Vec<Logout> {
        s.clients
            .iter()
            .filter(|c| self.client(c).is_some_and(|c| c.backchannel_logout_uri.is_some()))
            .map(|c| Logout {
                client: c.clone(),
                sub: s.person.clone(),
                sid: sid(key),
                queued_at: now,
                give_up_at: s.expires_at,
                attempts: 0,
                next_at: now,
                last_error: None,
            })
            .collect()
    }

    pub(crate) async fn queue(&self, logouts: Vec<Logout>) -> anyhow::Result<()> {
        if logouts.is_empty() {
            return Ok(());
        }
        let mut out = self.store.logouts.lock().await;
        out.logouts.extend(logouts);
        self.store.save_logouts(&out)
    }

    /// End every session `person` started at or before `before`, send their logouts, and drop their access tokens.
    /// Returns how many sessions ended.
    pub async fn sign_out_everywhere(&self, person: &str, before: i64) -> anyhow::Result<usize> {
        let now = now_ms();
        let ended: Vec<(String, Session)> = {
            let mut sessions = self.store.sessions.lock().await;
            let keys: Vec<String> = sessions
                .sessions
                .iter()
                .filter(|(_, s)| s.person == person && s.created_at <= before)
                .map(|(k, _)| k.clone())
                .collect();
            let ended = keys.into_iter().filter_map(|k| sessions.sessions.remove(&k).map(|s| (k, s))).collect();
            self.store.save_sessions(&sessions)?;
            ended
        };
        self.drop_access_tokens(person).await;
        let logouts = ended.iter().flat_map(|(k, s)| self.logouts_for(k, s, now)).collect();
        self.queue(logouts).await?;
        if !ended.is_empty() {
            tracing::info!(person, sessions = ended.len(), "signed out everywhere");
        }
        Ok(ended.len())
    }

    /// Act on `revocations.json` (the `signout` command): sign out everyone named, for sessions from before then.
    pub async fn apply_revocations(&self) -> anyhow::Result<()> {
        let path = self.store.dir.join("revocations.json");
        let r: Revocations = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => anyhow::bail!("{}: {e}", path.display()),
        };
        for (person, at) in &r.people {
            let has =
                self.store.sessions.lock().await.sessions.values().any(|s| &s.person == person && s.created_at <= *at);
            if has {
                self.sign_out_everywhere(person, *at).await?;
            }
        }
        Ok(())
    }

    /// Send the logouts that are due: delivered ones leave the outbox, failed ones wait longer each time.
    pub async fn deliver_logouts(&self) {
        let now = now_ms();
        let due: Vec<Logout> = {
            let mut out = self.store.logouts.lock().await;
            let before = out.logouts.len();
            out.logouts.retain(|l| {
                let keep = l.give_up_at > now;
                if !keep {
                    tracing::warn!(client = %l.client, sid = %l.sid, "gave up on a sign-out: its session has expired anyway");
                }
                keep
            });
            if out.logouts.len() != before
                && let Err(e) = self.store.save_logouts(&out)
            {
                tracing::warn!("saving the sign-out outbox: {e:#}");
            }
            out.logouts.iter().filter(|l| l.next_at <= now).cloned().collect()
        };
        for l in due {
            let result = self.send_logout(&l).await;
            let mut out = self.store.logouts.lock().await;
            let Some(i) = out.logouts.iter().position(|x| x.client == l.client && x.sid == l.sid) else { continue };
            match result {
                Ok(()) => {
                    out.logouts.remove(i);
                }
                Err(e) => {
                    let x = &mut out.logouts[i];
                    x.attempts += 1;
                    x.next_at = now_ms() + backoff(x.attempts - 1);
                    x.last_error = Some(format!("{e:#}"));
                    tracing::warn!(client = %x.client, attempts = x.attempts, "a sign-out wasn't delivered: {e:#}");
                }
            }
            if let Err(e) = self.store.save_logouts(&out) {
                tracing::warn!("saving the sign-out outbox: {e:#}");
            }
        }
    }

    async fn send_logout(&self, l: &Logout) -> anyhow::Result<()> {
        let uri = self
            .client(&l.client)
            .and_then(|c| c.backchannel_logout_uri.clone())
            .ok_or_else(|| anyhow::anyhow!("{} no longer takes sign-outs", l.client))?;
        let now = now_ms() / 1000;
        let token = self.keys.sign_typed(
            "logout+jwt",
            &json!({
                "iss": self.settings.issuer, "aud": l.client, "iat": now, "exp": now + 120,
                "jti": fleetlayer_oidc::new_id(), "sub": l.sub, "sid": l.sid, "events": { EVENT: {} },
            }),
        )?;
        let body = form_urlencoded::Serializer::new(String::new()).append_pair("logout_token", &token).finish();
        let http = fleetlayer_oidc::client_for(&uri)?;
        let resp = http
            .post(&uri)
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        anyhow::ensure!(resp.status().is_success(), "{uri} answered {}", resp.status());
        Ok(())
    }

    /// Logouts waiting more than an hour, for `/healthz`.
    pub(crate) async fn late_logouts(&self) -> Option<String> {
        let now = now_ms();
        let out = self.store.logouts.lock().await;
        let late: Vec<&Logout> = out.logouts.iter().filter(|l| now - l.queued_at > LATE_MS).collect();
        let first = late.first()?;
        Some(format!(
            "{} sign-out(s) not delivered for more than an hour (to {}: {})",
            late.len(),
            first.client,
            first.last_error.as_deref().unwrap_or("not tried yet")
        ))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn retries_back_off_to_an_hour() {
        assert_eq!(super::backoff(0), 30_000);
        assert_eq!(super::backoff(1), 60_000);
        assert_eq!(super::backoff(7), 3_600_000);
        assert_eq!(super::backoff(1000), 3_600_000);
    }
}
