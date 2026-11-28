//! The control plane, as the dashboard reaches it: its HTTP API with the dashboard's own token (`docs/dashboard.md`).
//! Nothing here reads the control plane's database or a node directly, so a hosted control plane and a self-hosted
//! one look the same.

use std::time::Duration;

use serde_json::{Value, json};

pub struct Control {
    base: String,
    token: String,
    http: reqwest::Client,
}

/// Why a call failed, in words for the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failed {
    /// The token doesn't allow it (403), or isn't accepted (401).
    Refused(String),
    /// Not there (404): an older control plane without this endpoint, or a request already decided.
    Missing(String),
    /// Couldn't reach it, or it failed.
    Down(String),
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failed::Refused(m) | Failed::Missing(m) | Failed::Down(m) => f.write_str(m),
        }
    }
}

impl Control {
    pub fn new(base: &str, token: &str) -> anyhow::Result<Control> {
        let base = base.trim_end_matches('/').to_string();
        anyhow::ensure!(
            base.starts_with("https://") || base.starts_with("http://"),
            "control.url should start with https:// (or http:// on a private network)"
        );
        Ok(Control {
            base,
            token: token.trim().to_string(),
            http: reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?,
        })
    }

    async fn send(&self, req: reqwest::RequestBuilder, path: &str) -> Result<Value, Failed> {
        let r = req
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| Failed::Down(format!("the control plane can't be reached ({})", e.without_url())))?;
        let status = r.status();
        let body = r.text().await.unwrap_or_default();
        match status.as_u16() {
            200..=299 => serde_json::from_str(&body).map_err(|e| Failed::Down(format!("{path}: {e}"))),
            401 | 403 => Err(Failed::Refused(body.trim().to_string())),
            404 => Err(Failed::Missing(if body.trim().is_empty() {
                format!("{path} isn't there")
            } else {
                body.trim().into()
            })),
            _ => Err(Failed::Down(format!("{path} answered {status}: {}", body.trim()))),
        }
    }

    pub async fn get(&self, path: &str) -> Result<Value, Failed> {
        self.send(self.http.get(format!("{}{path}", self.base)), path).await
    }

    /// What the dashboard's token is (`GET /v1/token`): its scope (`admin`, `delegate`, `read`...) and its tenant's ID
    /// and slug.
    pub async fn token(&self) -> Result<crate::web::TokenInfo, Failed> {
        let v = self.get("/v1/token").await?;
        let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
        Ok(crate::web::TokenInfo { scope: s("scope"), tenant: s("tenant"), slug: s("tenantSlug") })
    }

    /// With a delegate token, the control plane acts as this person (their ID) and records it as theirs.
    fn as_person(req: reqwest::RequestBuilder, person: Option<&str>) -> reqwest::RequestBuilder {
        match person {
            Some(p) => req.header("fleetlayer-person", p),
            None => req,
        }
    }

    /// `GET <path>` as `person` (the people and policy pages: the control plane checks they may).
    pub async fn get_for(&self, path: &str, person: Option<&str>) -> Result<Value, Failed> {
        self.send(Self::as_person(self.http.get(format!("{}{path}", self.base)), person), path).await
    }

    /// `POST <path>` with a JSON body, as `person`.
    pub async fn post_for(&self, path: &str, body: &Value, person: Option<&str>) -> Result<Value, Failed> {
        let req = self.http.post(format!("{}{path}", self.base)).json(body);
        self.send(Self::as_person(req, person), path).await
    }

    /// `POST /v1/approvals/<id>` `{approve}`, as `person` when the token is a delegate token.
    pub async fn decide(&self, id: &str, approve: bool, person: Option<&str>) -> Result<Value, Failed> {
        let path = format!("/v1/approvals/{}", enc(id));
        let req = self.http.post(format!("{}{path}", self.base)).json(&json!({"approve": approve}));
        self.send(Self::as_person(req, person), &path).await
    }

    /// `DELETE /v1/windows/<role>`, as `person` when the token is a delegate token.
    pub async fn close_window(&self, role: &str, person: Option<&str>) -> Result<Value, Failed> {
        let path = format!("/v1/windows/{}", enc(role));
        self.send(Self::as_person(self.http.delete(format!("{}{path}", self.base)), person), &path).await
    }
}

/// One path segment, percent-encoded.
fn enc(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

#[cfg(test)]
mod tests {
    #[test]
    fn path_segments_are_encoded() {
        assert_eq!(super::enc("a b/c"), "a%20b%2Fc");
        assert_eq!(super::enc("ops-admin_1"), "ops-admin_1");
    }
}
