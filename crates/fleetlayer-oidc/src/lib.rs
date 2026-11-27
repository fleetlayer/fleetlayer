//! Signing people in to a Fleetlayer service through the sign-in service (`docs/plugins/signin.md`), or any OpenID
//! Connect provider: the authorization code flow with PKCE (S256), the state and nonce kept here and used once, and the
//! ID token checked with `jsonwebtoken`, pinned to RS256 and to keys from the issuer's own JWKS. Used by pages and the
//! Claude sign-in hub; first written for pages.
//!
//! A service's addresses follow one convention: it comes back to `<base>/_/auth/callback` and, after signing out at the
//! sign-in service, to `<base>/_/signed-out`. Register both with the sign-in service.
//!
//! The token only says who someone is. What they may do is asked per request (`_fleetlayer.dev/access/check`).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

/// How long a sign-in may take, from leaving for the issuer to coming back.
const PENDING: Duration = Duration::from_secs(10 * 60);
/// Clock skew allowed on `exp` and `nbf`.
const LEEWAY: u64 = 60;
/// A key the JWKS doesn't have is looked for again at most this often.
const JWKS_REFETCH: Duration = Duration::from_secs(60);
/// The discovery document is fetched again after this long.
const META_TTL: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Deserialize)]
struct Meta {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    #[serde(default)]
    end_session_endpoint: Option<String>,
}

struct Pending {
    verifier: String,
    nonce: String,
    next: String,
    at: Instant,
}

#[derive(Default)]
struct Keys {
    by_kid: HashMap<String, DecodingKey>,
    fetched: Option<Instant>,
}

pub struct Oidc {
    issuer: String,
    client: String,
    secret: String,
    /// `<base_url>/_/auth/callback`, registered exactly with the issuer.
    redirect_uri: String,
    /// `<base_url>/_/signed-out`, where signing out comes back to (registered as a `post_logout_redirect_uris`).
    after_logout: String,
    http: reqwest::Client,
    meta: Mutex<Option<(Meta, Instant)>>,
    keys: Mutex<Keys>,
    pending: Mutex<HashMap<String, Pending>>,
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: String,
}

/// A person signed in through Fleetlayer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    /// The person's ID (the token's `sub`), never reused.
    pub sub: String,
    /// The tenant they signed in to (`fleetlayer.dev/tenant`).
    pub tenant: String,
    pub handle: Option<String>,
    pub name: Option<String>,
}

/// 32 random bytes from the kernel (`/dev/urandom` on Linux and macOS), hex.
pub fn new_id() -> String {
    use std::io::Read;
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).expect("reading /dev/urandom");
    hex::encode(b)
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// A page that moves the browser on to `url`, for two places a redirect doesn't work: after the callback (a
/// `SameSite=Strict` session cookie isn't sent with a redirect that's still the issuer's cross-site navigation) and
/// when signing out (a form's `form-action 'self'` stops a redirect to the sign-in service). `title` and `link` are
/// what the page says while it moves on.
pub fn continue_page(url: &str, title: &str, link: &str) -> String {
    format!(
        r#"<!doctype html><meta charset="utf-8"><meta http-equiv="refresh" content="0;url={0}"><title>{1}</title><p><a href="{0}">{2}</a></p>"#,
        esc(url),
        esc(title),
        esc(link)
    )
}

/// An issuer address: https, or plain http only on this machine.
pub fn valid_issuer(issuer: &str) -> bool {
    if issuer.starts_with("https://") {
        return true;
    }
    let Some(rest) = issuer.strip_prefix("http://") else { return false };
    let host = rest.split(['/', ':']).next().unwrap_or_default();
    host == "localhost" || host.ends_with(".localhost") || host == "127.0.0.1"
}

fn b64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len() * 4 / 3 + 2);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(A[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

/// The HTTP client for the issuer. A `*.localhost` issuer (a laptop's `signin.localhost:7680`) is this machine
/// (RFC 6761), as browsers know, but the system's resolver often doesn't: resolve it to loopback here.
pub fn client_for(issuer: &str) -> anyhow::Result<reqwest::Client> {
    let mut b = reqwest::Client::builder().timeout(Duration::from_secs(15));
    let host = issuer.split("://").nth(1).unwrap_or_default().split(['/', ':']).next().unwrap_or_default();
    if host.ends_with(".localhost") {
        b = b.resolve(host, std::net::SocketAddr::from(([127, 0, 0, 1], 0)));
    }
    Ok(b.build()?)
}

fn enc(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

impl Oidc {
    /// A Fleetlayer service's client: it comes back to `<base>/_/auth/callback`, and to `<base>/_/signed-out` after
    /// signing out.
    pub fn new(issuer: &str, client: &str, secret: &str, base_url: &str) -> anyhow::Result<Oidc> {
        let base = base_url.trim_end_matches('/');
        Oidc::with_redirect(issuer, client, secret, &format!("{base}/_/auth/callback"), &format!("{base}/_/signed-out"))
    }

    /// A client with its own return addresses: the sign-in service as the client of an upstream provider (Google, an
    /// OIDC provider), coming back to `<issuer>/upstream/<name>/callback`.
    pub fn with_redirect(
        issuer: &str,
        client: &str,
        secret: &str,
        redirect_uri: &str,
        after_logout: &str,
    ) -> anyhow::Result<Oidc> {
        let issuer = issuer.trim_end_matches('/').to_string();
        anyhow::ensure!(valid_issuer(&issuer), "signin issuer {issuer:?}: use https (plain http only on localhost)");
        anyhow::ensure!(!secret.trim().is_empty(), "the sign-in client secret is empty");
        let http = client_for(&issuer)?;
        Ok(Oidc {
            issuer,
            client: client.to_string(),
            secret: secret.trim().to_string(),
            redirect_uri: redirect_uri.to_string(),
            after_logout: after_logout.to_string(),
            http,
            meta: Mutex::default(),
            keys: Mutex::default(),
            pending: Mutex::default(),
        })
    }

    async fn meta(&self) -> anyhow::Result<Meta> {
        let mut cur = self.meta.lock().await;
        if let Some((m, at)) = &*cur
            && at.elapsed() < META_TTL
        {
            return Ok(m.clone());
        }
        let url = format!("{}/.well-known/openid-configuration", self.issuer);
        let m: Meta = self.http.get(&url).send().await?.error_for_status()?.json().await?;
        // The document must be the issuer's own (OpenID Connect Discovery, section 4.3).
        anyhow::ensure!(m.issuer.trim_end_matches('/') == self.issuer, "the issuer's discovery names {:?}", m.issuer);
        *cur = Some((m.clone(), Instant::now()));
        Ok(m)
    }

    /// Where to send the browser to sign in, and the state to bind to it (a cookie), for coming back to `next`.
    pub async fn start(&self, next: &str) -> anyhow::Result<(String, String)> {
        let meta = self.meta().await?;
        let state = new_id();
        let nonce = new_id();
        let verifier = format!("{}{}", new_id(), &new_id()[..32]);
        let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
        {
            let mut p = self.pending.lock().await;
            p.retain(|_, v| v.at.elapsed() < PENDING);
            p.insert(
                state.clone(),
                Pending { verifier, nonce: nonce.clone(), next: next.to_string(), at: Instant::now() },
            );
        }
        let sep = if meta.authorization_endpoint.contains('?') { '&' } else { '?' };
        let url = format!(
            "{}{sep}response_type=code&client_id={}&redirect_uri={}&scope=openid%20profile&state={state}&nonce={nonce}\
             &code_challenge={challenge}&code_challenge_method=S256",
            meta.authorization_endpoint,
            enc(&self.client),
            enc(&self.redirect_uri),
        );
        Ok((url, state))
    }

    /// The person behind a code the issuer sent back, and where they were going. `bound` is the state from the
    /// browser's cookie: it must match, so a sign-in someone else started can't be finished in this browser.
    pub async fn finish(&self, code: &str, state: &str, bound: Option<&str>) -> anyhow::Result<(Person, String)> {
        let (claims, next) = self.finish_claims(code, state, bound).await?;
        let get = |k: &str| claims.get(k).and_then(|v| v.as_str()).map(str::to_string);
        let tenant =
            get("fleetlayer.dev/tenant").ok_or_else(|| anyhow::anyhow!("the ID token doesn't say which tenant"))?;
        let sub = get("sub").unwrap_or_default();
        Ok((Person { sub, tenant, handle: get("preferred_username"), name: get("name") }, next))
    }

    /// [`finish`](Self::finish) for any provider: the ID token's claims once checked (signature by a key from the
    /// issuer's JWKS, RS256, issuer, audience, expiry, nonce, a subject), and where the browser was going.
    pub async fn finish_claims(
        &self,
        code: &str,
        state: &str,
        bound: Option<&str>,
    ) -> anyhow::Result<(serde_json::Map<String, serde_json::Value>, String)> {
        anyhow::ensure!(bound == Some(state), "this sign-in wasn't started in this browser");
        let pending = self.pending.lock().await.remove(state);
        let p = pending.ok_or_else(|| anyhow::anyhow!("this sign-in link was already used or has expired"))?;
        anyhow::ensure!(p.at.elapsed() < PENDING, "this sign-in took too long");
        let meta = self.meta().await?;
        let body = format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&code_verifier={}&client_id={}",
            enc(code),
            enc(&self.redirect_uri),
            enc(&p.verifier),
            enc(&self.client)
        );
        let res = self
            .http
            .post(&meta.token_endpoint)
            .basic_auth(enc(&self.client), Some(enc(&self.secret)))
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await?;
        let status = res.status();
        if !status.is_success() {
            let text = res.text().await.unwrap_or_default();
            anyhow::bail!(
                "the sign-in service refused the code ({status}): {}",
                text.chars().take(200).collect::<String>()
            );
        }
        let t: TokenResponse = res.json().await?;
        let claims = self.verify(&meta, &t.id_token, &p.nonce).await?;
        Ok((claims, p.next))
    }

    async fn key(&self, meta: &Meta, kid: &str) -> anyhow::Result<DecodingKey> {
        let mut keys = self.keys.lock().await;
        if let Some(k) = keys.by_kid.get(kid) {
            return Ok(k.clone());
        }
        if keys.fetched.is_some_and(|t| t.elapsed() < JWKS_REFETCH) {
            anyhow::bail!("the ID token's key {kid:?} isn't in the issuer's JWKS");
        }
        let jwks: serde_json::Value = self.http.get(&meta.jwks_uri).send().await?.error_for_status()?.json().await?;
        keys.by_kid.clear();
        keys.fetched = Some(Instant::now());
        for k in jwks["keys"].as_array().into_iter().flatten() {
            let usable = k["kty"] == "RSA"
                && k["alg"].as_str().is_none_or(|a| a == "RS256")
                && k["use"].as_str().is_none_or(|u| u == "sig");
            let (Some(id), Some(n), Some(e)) = (k["kid"].as_str(), k["n"].as_str(), k["e"].as_str()) else { continue };
            if usable && let Ok(key) = DecodingKey::from_rsa_components(n, e) {
                keys.by_kid.insert(id.to_string(), key);
            }
        }
        keys.by_kid
            .get(kid)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("the ID token's key {kid:?} isn't in the issuer's JWKS"))
    }

    async fn verify(
        &self,
        meta: &Meta,
        token: &str,
        nonce: &str,
    ) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
        let header = jsonwebtoken::decode_header(token)?;
        anyhow::ensure!(header.alg == Algorithm::RS256, "the ID token isn't signed with RS256");
        let kid = header.kid.ok_or_else(|| anyhow::anyhow!("the ID token names no key"))?;
        let key = self.key(meta, &kid).await?;
        let mut v = Validation::new(Algorithm::RS256);
        v.set_issuer(&[&self.issuer, &meta.issuer]);
        v.set_audience(&[&self.client]);
        v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        v.validate_nbf = true;
        v.leeway = LEEWAY;
        let data = jsonwebtoken::decode::<serde_json::Map<String, serde_json::Value>>(token, &key, &v)?;
        let c = data.claims;
        anyhow::ensure!(
            c.get("nonce").and_then(|n| n.as_str()) == Some(nonce),
            "the ID token's nonce doesn't match this sign-in"
        );
        anyhow::ensure!(
            c.get("sub").and_then(|s| s.as_str()).is_some_and(|s| !s.is_empty()),
            "the ID token has no subject"
        );
        Ok(c)
    }

    /// Where to send the browser after signing out here, so the sign-in service ends its session too.
    pub async fn logout_url(&self) -> Option<String> {
        let end = self.meta().await.ok()?.end_session_endpoint?;
        let sep = if end.contains('?') { '&' } else { '?' };
        Some(format!("{end}{sep}client_id={}&post_logout_redirect_uri={}", enc(&self.client), enc(&self.after_logout)))
    }
}

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    //! A fake issuer for tests (with the `testing` feature for other crates).
    use super::*;

    use std::sync::Arc;

    use axum::extract::{Form, State};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use rsa::pkcs1v15::SigningKey;
    use rsa::signature::{SignatureEncoding, Signer};
    use rsa::traits::PublicKeyParts;
    use serde_json::{Value, json};

    /// An issuer for the tests: discovery, a JWKS and a token endpoint that hands out whatever ID token the test set.
    pub struct FakeIssuer {
        pub url: String,
        key: rsa::PrivateKey,
        /// The client ID tokens are for (`aud`): `pages` unless started with [`FakeIssuer::start_for`].
        pub client: String,
        pub token: std::sync::Mutex<Option<String>>,
        pub seen: std::sync::Mutex<Vec<HashMap<String, String>>>,
    }

    impl FakeIssuer {
        pub async fn start() -> Arc<FakeIssuer> {
            FakeIssuer::start_for("pages").await
        }

        pub async fn start_for(client: &str) -> Arc<FakeIssuer> {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
            let key = rsa::PrivateKey::new(&mut crypto_common::getrandom::SysRng, 2048).unwrap();
            let me = Arc::new(FakeIssuer {
                url,
                key,
                client: client.to_string(),
                token: Default::default(),
                seen: Default::default(),
            });
            let router = Router::new()
                .route("/.well-known/openid-configuration", get(discovery))
                .route("/jwks", get(jwks))
                .route("/token", post(token))
                .with_state(me.clone());
            tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            me
        }

        pub fn sign(&self, header: Value, claims: Value) -> String {
            let b = |v: &Value| b64url(v.to_string().as_bytes());
            let input = format!("{}.{}", b(&header), b(&claims));
            let sig = SigningKey::<Sha256>::new(self.key.clone()).sign(input.as_bytes());
            format!("{input}.{}", b64url(&sig.to_vec()))
        }

        pub fn claims(&self, nonce: &str) -> Value {
            let now = jiff::Timestamp::now().as_second();
            json!({
                "iss": self.url, "aud": self.client, "sub": "p_123", "exp": now + 600, "iat": now, "nonce": nonce,
                "preferred_username": "alice", "name": "Alice", "fleetlayer.dev/tenant": "home",
            })
        }
    }

    async fn discovery(State(f): State<Arc<FakeIssuer>>) -> Json<Value> {
        Json(json!({
            "issuer": f.url, "authorization_endpoint": format!("{}/authorize", f.url),
            "token_endpoint": format!("{}/token", f.url), "jwks_uri": format!("{}/jwks", f.url),
            "end_session_endpoint": format!("{}/logout", f.url),
        }))
    }

    async fn jwks(State(f): State<Arc<FakeIssuer>>) -> Json<Value> {
        let p = f.key.to_public_key();
        Json(json!({"keys": [{"kty": "RSA", "alg": "RS256", "use": "sig", "kid": "k1",
            "n": b64url(&p.n_bytes()), "e": b64url(&p.e_bytes())}]}))
    }

    async fn token(State(f): State<Arc<FakeIssuer>>, Form(form): Form<HashMap<String, String>>) -> Json<Value> {
        f.seen.lock().unwrap().push(form);
        Json(json!({"id_token": f.token.lock().unwrap().clone().unwrap_or_default(), "token_type": "Bearer"}))
    }

    pub fn param(url: &str, name: &str) -> String {
        let q = url.split_once('?').unwrap().1;
        form_urlencoded::parse(q.as_bytes()).find(|(k, _)| k == name).map(|(_, v)| v.into_owned()).unwrap()
    }

    pub fn good(f: &FakeIssuer, c: Value) -> String {
        f.sign(json!({"alg": "RS256", "typ": "JWT", "kid": "k1"}), c)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::testing::*;
    use super::*;
    use serde_json::{Value, json};

    /// Starts a sign-in and has the issuer answer with the token `make` builds from the expected claims.
    async fn sign_in(
        make: impl Fn(&FakeIssuer, Value) -> String,
    ) -> (Arc<FakeIssuer>, anyhow::Result<(Person, String)>) {
        let f = FakeIssuer::start().await;
        let o = Oidc::new(&f.url, "pages", "s3cret", "https://pages.example").unwrap();
        let (url, state) = o.start("/personal/").await.unwrap();
        assert_eq!(param(&url, "code_challenge_method"), "S256");
        assert_eq!(param(&url, "redirect_uri"), "https://pages.example/_/auth/callback");
        let claims = f.claims(&param(&url, "nonce"));
        *f.token.lock().unwrap() = Some(make(&f, claims));
        let r = o.finish("the-code", &state, Some(&state)).await;
        (f, r)
    }

    #[tokio::test]
    async fn a_good_token_signs_the_person_in_with_pkce() {
        let (f, r) = sign_in(good).await;
        let (p, next) = r.unwrap();
        assert_eq!((p.sub.as_str(), p.tenant.as_str(), next.as_str()), ("p_123", "home", "/personal/"));
        assert_eq!(p.handle.as_deref(), Some("alice"));
        let seen = f.seen.lock().unwrap()[0].clone();
        assert_eq!(seen["grant_type"], "authorization_code");
        assert_eq!(seen["code"], "the-code");
        // The verifier sent is the one whose S256 was the challenge (checked by a real issuer); here: present, long.
        assert!(seen["code_verifier"].len() >= 43);
    }

    #[tokio::test]
    async fn bad_tokens_are_refused() {
        type Make = Box<dyn Fn(&FakeIssuer, Value) -> String>;
        let cases: Vec<(&str, Make)> = vec![
            (
                "signature",
                Box::new(|f: &FakeIssuer, c: Value| {
                    let t = good(f, c);
                    let (rest, sig) = t.rsplit_once('.').unwrap();
                    let mut s = sig.as_bytes().to_vec();
                    s[10] = if s[10] == b'A' { b'B' } else { b'A' };
                    format!("{rest}.{}", String::from_utf8(s).unwrap())
                }),
            ),
            (
                "none",
                Box::new(|_f: &FakeIssuer, c: Value| {
                    format!("{}.{}.", b64url(br#"{"alg":"none","kid":"k1"}"#), b64url(c.to_string().as_bytes()))
                }),
            ),
            (
                "HS256",
                Box::new(|_f: &FakeIssuer, c: Value| {
                    format!("{}.{}.AAAA", b64url(br#"{"alg":"HS256","kid":"k1"}"#), b64url(c.to_string().as_bytes()))
                }),
            ),
            ("unknown key", Box::new(|f: &FakeIssuer, c: Value| f.sign(json!({"alg": "RS256", "kid": "k2"}), c))),
            (
                "audience",
                Box::new(|f: &FakeIssuer, mut c: Value| {
                    c["aud"] = json!("hub");
                    good(f, c)
                }),
            ),
            (
                "issuer",
                Box::new(|f: &FakeIssuer, mut c: Value| {
                    c["iss"] = json!("https://evil.example");
                    good(f, c)
                }),
            ),
            (
                "expired",
                Box::new(|f: &FakeIssuer, mut c: Value| {
                    c["exp"] = json!(jiff::Timestamp::now().as_second() - 3600);
                    good(f, c)
                }),
            ),
            (
                "not yet",
                Box::new(|f: &FakeIssuer, mut c: Value| {
                    c["nbf"] = json!(jiff::Timestamp::now().as_second() + 3600);
                    good(f, c)
                }),
            ),
            (
                "nonce",
                Box::new(|f: &FakeIssuer, mut c: Value| {
                    c["nonce"] = json!("other");
                    good(f, c)
                }),
            ),
            (
                "no tenant",
                Box::new(|f: &FakeIssuer, mut c: Value| {
                    c.as_object_mut().unwrap().remove("fleetlayer.dev/tenant");
                    good(f, c)
                }),
            ),
        ];
        for (what, make) in cases {
            let (_f, r) = sign_in(make).await;
            assert!(r.is_err(), "a token with a bad {what} was accepted");
        }
    }

    #[tokio::test]
    async fn a_state_is_used_once_and_only_in_the_browser_that_started_it() {
        let f = FakeIssuer::start().await;
        let o = Oidc::new(&f.url, "pages", "s3cret", "https://pages.example").unwrap();
        let (url, state) = o.start("/").await.unwrap();
        *f.token.lock().unwrap() = Some(good(&f, f.claims(&param(&url, "nonce"))));
        assert!(o.finish("c", &state, None).await.unwrap_err().to_string().contains("browser"));
        assert!(o.finish("c", &state, Some("other")).await.is_err());
        o.finish("c", &state, Some(&state)).await.unwrap();
        let e = o.finish("c", &state, Some(&state)).await.unwrap_err();
        assert!(e.to_string().contains("already used"), "{e}");
    }

    #[test]
    fn issuers_must_be_https_except_on_this_machine() {
        assert!(valid_issuer("https://signin.example"));
        assert!(valid_issuer("http://signin.localhost:7680"));
        assert!(!valid_issuer("http://signin.example"));
        assert_eq!(b64url(b"\xfb\xff"), "-_8");
    }
}
