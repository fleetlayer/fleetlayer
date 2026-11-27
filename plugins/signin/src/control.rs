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
    #[serde(default)]
    pub tenants: Vec<Membership>,
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
        let v = self.post("/v1/signin/invite_redeem", body).await?.ok_or_else(|| anyhow::anyhow!("not found"))?;
        Ok(serde_json::from_value(v)?)
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

    /// Whether the control plane answers (`/healthz`).
    pub async fn reachable(&self) -> bool {
        matches!(self.http.get(format!("{}/healthz", self.base)).send().await, Ok(r) if r.status().is_success())
    }
}
