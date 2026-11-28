//! The control plane, as the sign-in service reaches it: `/v1/signin/*` with the sign-in token (`docs/sso.md`).

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Membership {
    pub tenant: String,
    pub slug: String,
    pub name: String,
    pub handle: String,
    pub role: String,
    #[serde(default)]
    pub teams: Vec<String>,
    #[serde(default)]
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Person {
    pub id: String,
    pub display_name: String,
    /// Set by an operator on the control plane: verified, as far as services are concerned.
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub tenants: Vec<Membership>,
}

/// A request that didn't get an answer, in words for the person: nothing happened, so they can try again.
fn unreachable(e: &reqwest::Error) -> String {
    tracing::warn!("the control plane didn't answer: {e}");
    "sign-in can't reach the rest of Fleetlayer right now, so nothing was saved. Try again in a minute.".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Redeemed {
    pub tenant: String,
    pub person: String,
    pub handle: String,
    pub recovery: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Decision {
    pub allowed: bool,
    #[serde(default)]
    pub rules: Vec<String>,
    pub reason: String,
}

pub struct Control {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl Control {
    pub fn new(base: &str, token: &str) -> Control {
        Control {
            base: base.trim_end_matches('/').to_string(),
            token: token.trim().to_string(),
            http: reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap_or_default(),
        }
    }

    async fn post(&self, path: &str, body: Value) -> anyhow::Result<Option<Value>> {
        let r = self.http.post(format!("{}{path}", self.base)).bearer_auth(&self.token).json(&body).send().await?;
        let status = r.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let text = r.text().await?;
        anyhow::ensure!(status.is_success(), "the control plane answered {status}: {text}");
        Ok(Some(serde_json::from_str(&text)?))
    }

    pub async fn person(&self, id: &str) -> anyhow::Result<Option<Person>> {
        match self.post("/v1/signin/person", json!({"person": id})).await? {
            Some(v) => Ok(Some(serde_json::from_value(v)?)),
            None => Ok(None),
        }
    }

    /// Redeem an invite: a new account (`display_name`) or someone signed in already (`person`).
    pub async fn redeem(
        &self,
        code: &str,
        display_name: Option<&str>,
        person: Option<&str>,
    ) -> anyhow::Result<Redeemed> {
        let body = match (display_name, person) {
            (_, Some(p)) => json!({"code": code, "person": p}),
            (n, None) => json!({"code": code, "display_name": n.unwrap_or("")}),
        };
        // Tried again only when the request never reached the control plane (it couldn't connect): once it has,
        // the invite may be spent, and a second try would fail or, worse, do it twice.
        let mut wait = Duration::from_millis(500);
        let mut tries = 0;
        let resp = loop {
            tries += 1;
            match self
                .http
                .post(format!("{}/v1/signin/invite_redeem", self.base))
                .bearer_auth(&self.token)
                .json(&body)
                .send()
                .await
            {
                Ok(r) => break r,
                Err(e) if e.is_connect() && tries < 4 => {
                    tracing::warn!("redeeming an invite: can't connect to the control plane (try {tries}): {e}");
                    tokio::time::sleep(wait).await;
                    wait *= 3;
                }
                Err(e) => anyhow::bail!("{}", unreachable(&e)),
            }
        };
        let status = resp.status();
        let text = resp.text().await?;
        anyhow::ensure!(status.is_success(), "{}", text.trim());
        Ok(serde_json::from_str(&text)?)
    }

    /// Whether an invite can still be used (`{tenant, tenant_name, handle, role, for_person}`), before anyone makes a
    /// passkey for it. `Err` is the reason, in words for the person.
    pub async fn invite_check(&self, code: &str) -> Result<Value, String> {
        let r = self
            .http
            .post(format!("{}/v1/signin/invite_check", self.base))
            .bearer_auth(&self.token)
            .json(&json!({"code": code}))
            .send()
            .await
            .map_err(|e| unreachable(&e))?;
        let status = r.status();
        let text = r.text().await.unwrap_or_default();
        if status.is_success() {
            return serde_json::from_str(&text).map_err(|e| format!("the control plane's answer: {e}"));
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            // A control plane from before invite_check: let the redeem decide.
            return Ok(json!({}));
        }
        Err(if text.trim().is_empty() {
            format!("the control plane answered {status}")
        } else {
            text.trim().to_string()
        })
    }

    /// Someone an upstream provider vouches for joins its tenant as a member.
    pub async fn join(&self, tenant: &str, display_name: &str, hint: &str, via: &str) -> anyhow::Result<Redeemed> {
        let body = json!({"tenant": tenant, "display_name": display_name, "hint": hint, "via": via});
        let v = self.post("/v1/signin/join", body).await?.ok_or_else(|| anyhow::anyhow!("not found"))?;
        Ok(serde_json::from_value(v)?)
    }

    /// `person`'s teams from `upstream`'s groups (it replaces that upstream's, and keeps an admin's).
    pub async fn teams_sync(
        &self,
        tenant: &str,
        person: &str,
        upstream: &str,
        teams: &[String],
    ) -> anyhow::Result<Value> {
        let body = json!({"tenant": tenant, "person": person, "upstream": upstream, "teams": teams});
        self.post("/v1/signin/teams_sync", body).await?.ok_or_else(|| anyhow::anyhow!("not found"))
    }

    pub async fn check(&self, tenant: &str, who: &str, can: &str, to: &str) -> anyhow::Result<Decision> {
        let v = self
            .post("/v1/signin/check", json!({"tenant": tenant, "who": who, "can": can, "to": to}))
            .await?
            .ok_or_else(|| anyhow::anyhow!("not found"))?;
        Ok(serde_json::from_value(v)?)
    }

    /// Record a sign-in event in the tenant's audit log. Best effort: a sign-in doesn't fail for want of its record.
    pub async fn event(&self, tenant: &str, person: &str, kind: &str, client: Option<&str>, detail: &str) {
        let body = json!({"tenant": tenant, "person": person, "kind": kind, "client": client, "detail": detail});
        if let Err(e) = self.post("/v1/signin/event", body).await {
            tracing::warn!("recording {kind} for {person} in {tenant}: {e}");
        }
    }

    /// The person just used their passkey (signing in, or making one with an invite): the control plane keeps the time,
    /// for changes to people that need a recent one.
    pub async fn passkey_used(&self, person: &str) {
        if let Err(e) = self.post("/v1/signin/passkey", json!({"person": person})).await {
            tracing::warn!("recording a passkey use for {person}: {e}");
        }
    }

    /// What an approval asks (`{id, from, verb, to, detail, status, holdMs, createdAt}`); none if there's no such
    /// approval in that tenant.
    pub async fn approval_view(&self, tenant: &str, approval: &str) -> anyhow::Result<Option<Value>> {
        self.post("/v1/signin/approval_view", json!({"tenant": tenant, "approval": approval})).await
    }

    /// Tell the control plane `person` just passed a fresh passkey check for `approval`. `Err` carries the control
    /// plane's own reason when it refuses (not an admin, not pending...).
    pub async fn stepup(&self, tenant: &str, person: &str, approval: &str) -> Result<Value, String> {
        let body = json!({"tenant": tenant, "person": person, "approval": approval});
        let r = self
            .http
            .post(format!("{}/v1/signin/stepup", self.base))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("the control plane didn't answer: {e}"))?;
        let status = r.status();
        let text = r.text().await.unwrap_or_default();
        if status.is_success() {
            return serde_json::from_str(&text).map_err(|e| format!("the control plane's answer: {e}"));
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err("there's no such request".into());
        }
        Err(if text.trim().is_empty() {
            format!("the control plane answered {status}")
        } else {
            text.trim().to_string()
        })
    }

    /// Whether the control plane answers (`/healthz`).
    pub async fn reachable(&self) -> bool {
        matches!(self.http.get(format!("{}/healthz", self.base)).send().await, Ok(r) if r.status().is_success())
    }
}
