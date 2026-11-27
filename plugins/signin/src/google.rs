//! Reading a person's Google Workspace groups, and whether their account is still active, from the Admin SDK
//! Directory API (`docs/sso.md`, "Upstream providers", step 4b), with a service account's key: the OAuth 2.0 JWT
//! bearer grant, signed RS256 with the key from its JSON file. The key never leaves the sign-in machine.
//!
//! The service account needs read access to groups and users: an admin role with "Groups: Read" and "Users: Read"
//! assigned to it, or domain-wide delegation with `subject` set to an admin to act as.
//!
//! Only direct memberships count: `groups.list?userKey=` doesn't return the groups a person is in through another
//! group, and nothing here follows nesting.

use std::path::Path;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::signature::{SignatureEncoding, Signer};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::sync::Mutex;

const SCOPES: &str = "https://www.googleapis.com/auth/admin.directory.group.readonly \
                      https://www.googleapis.com/auth/admin.directory.user.readonly";
pub const DIRECTORY: &str = "https://admin.googleapis.com/admin/directory/v1";

#[derive(Deserialize)]
struct KeyFile {
    client_email: String,
    private_key: String,
    #[serde(default = "default_token_uri")]
    token_uri: String,
}

fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".into()
}

/// A Directory API client for one Workspace.
pub struct Directory {
    email: String,
    key: rsa::PrivateKey,
    token_uri: String,
    /// An admin to act as, with domain-wide delegation; none when the service account holds an admin role itself.
    subject: Option<String>,
    base: String,
    http: reqwest::Client,
    token: Mutex<Option<(String, i64)>>,
}

/// What the Directory says about someone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Account {
    /// Active, in these groups (their email addresses, lower-case).
    Active(Vec<String>),
    /// Suspended or archived: no longer allowed in.
    Suspended,
    /// The Directory has no such user (deleted).
    Gone,
}

impl Directory {
    pub fn from_file(path: &Path, subject: Option<String>) -> anyhow::Result<Directory> {
        let text = std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let k: KeyFile = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("{}: not a service account key: {e}", path.display()))?;
        Directory::new(&k.client_email, &k.private_key, &k.token_uri, subject, DIRECTORY)
    }

    pub fn new(
        email: &str,
        pem: &str,
        token_uri: &str,
        subject: Option<String>,
        base: &str,
    ) -> anyhow::Result<Directory> {
        let key =
            rsa::PrivateKey::from_pkcs8_pem(pem).map_err(|e| anyhow::anyhow!("the service account's key: {e}"))?;
        Ok(Directory {
            email: email.into(),
            key,
            token_uri: token_uri.into(),
            subject,
            base: base.trim_end_matches('/').into(),
            http: reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?,
            token: Mutex::default(),
        })
    }

    async fn access_token(&self) -> anyhow::Result<String> {
        let now = crate::store::now_ms() / 1000;
        let mut cur = self.token.lock().await;
        if let Some((t, exp)) = &*cur
            && *exp - 60 > now
        {
            return Ok(t.clone());
        }
        let mut claims =
            json!({"iss": self.email, "scope": SCOPES, "aud": self.token_uri, "iat": now, "exp": now + 3600});
        if let Some(s) = &self.subject {
            claims["sub"] = json!(s);
        }
        let header = json!({"alg": "RS256", "typ": "JWT"});
        let input =
            format!("{}.{}", B64.encode(serde_json::to_vec(&header)?), B64.encode(serde_json::to_vec(&claims)?));
        let sig = SigningKey::<Sha256>::new(self.key.clone()).sign(input.as_bytes());
        let assertion = format!("{input}.{}", B64.encode(sig.to_bytes()));
        let body = form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer")
            .append_pair("assertion", &assertion)
            .finish();
        let resp = self
            .http
            .post(&self.token_uri)
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await?;
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or_default();
        anyhow::ensure!(
            status.is_success(),
            "Google refused the service account's token request ({status}): {}",
            v["error_description"].as_str().or(v["error"].as_str()).unwrap_or("")
        );
        let token =
            v["access_token"].as_str().ok_or_else(|| anyhow::anyhow!("no access_token from Google"))?.to_string();
        let exp = now + v["expires_in"].as_i64().unwrap_or(3600);
        *cur = Some((token.clone(), exp));
        Ok(token)
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> anyhow::Result<Option<Value>> {
        let token = self.access_token().await?;
        let mut url = url::Url::parse(&format!("{}{path}", self.base))?;
        url.query_pairs_mut().extend_pairs(query);
        let resp = self.http.get(url).bearer_auth(token).send().await?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let v: Value = resp.json().await.unwrap_or_default();
        anyhow::ensure!(
            status.is_success(),
            "the Directory API answered {status}: {}",
            v["error"]["message"].as_str().unwrap_or("")
        );
        Ok(Some(v))
    }

    /// The account with Google ID `user` (an ID token's `sub`): suspended, gone, or active with its direct groups.
    pub async fn account(&self, user: &str) -> anyhow::Result<Account> {
        let path = format!("/users/{}", form_urlencoded::byte_serialize(user.as_bytes()).collect::<String>());
        let Some(u) = self.get(&path, &[("fields", "suspended,archived")]).await? else { return Ok(Account::Gone) };
        if u["suspended"].as_bool() == Some(true) || u["archived"].as_bool() == Some(true) {
            return Ok(Account::Suspended);
        }
        let mut groups = vec![];
        let mut page: Option<String> = None;
        loop {
            let mut q = vec![("userKey", user), ("maxResults", "200")];
            if let Some(p) = &page {
                q.push(("pageToken", p));
            }
            let Some(v) = self.get("/groups", &q).await? else { return Ok(Account::Gone) };
            for g in v["groups"].as_array().into_iter().flatten() {
                if let Some(e) = g["email"].as_str() {
                    groups.push(e.to_ascii_lowercase());
                }
            }
            match v["nextPageToken"].as_str() {
                Some(p) if groups.len() < 10_000 => page = Some(p.to_string()),
                _ => break,
            }
        }
        Ok(Account::Active(groups))
    }
}
