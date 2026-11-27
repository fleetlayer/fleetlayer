//! The sign-in service's web side: an OpenID Connect provider (discovery, JWKS, authorize with PKCE, token,
//! userinfo, end-session), passkey sign-in and invite pages, a chooser for people in several places, `/api/check`
//! and `/healthz`. `docs/sso.md` is the design.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;
use url::Url;
use webauthn_rs::prelude::*;

use crate::control::{Control, Membership};
use crate::keys::Keys;
use crate::store::{Credentials, Session, Store, StoredPasskey, now_ms, sha256_hex};

/// How long things last.
const CODE_MS: i64 = 60_000;
const TOKEN_SECS: i64 = 600;
const SESSION_MS: i64 = 14 * 86_400_000;
const CEREMONY_MS: i64 = 5 * 60_000;

/// A service that signs people in here (an OpenID Connect client), named as in the policy (`service:<name>`).
#[derive(Debug, Clone)]
pub struct Client {
    pub name: String,
    pub redirect_uris: Vec<String>,
    /// None: a public client (PKCE alone). PKCE is required of every client either way.
    pub secret: Option<String>,
    pub post_logout_redirect_uris: Vec<String>,
    /// Where it takes back-channel logouts, if it does.
    pub backchannel_logout_uri: Option<String>,
}

pub struct Settings {
    /// The issuer, without a trailing slash.
    pub issuer: String,
    pub origin: Url,
    /// What passkeys are made for, and whether they'd stop working if sign-in moved (localhost, a machine name).
    pub passkey_domain: String,
    pub passkeys_fixed: bool,
    pub clients: Vec<Client>,
    /// Addresses it had before (`host[:port]`), still pointed here while services are repointed: discovery there
    /// redirects to the issuer, the JWKS is served, and everything else redirects to the same path at the issuer.
    pub previous_issuers: Vec<String>,
    /// Forward-auth, for services that can't speak OpenID Connect.
    pub forward: Option<crate::forward::Settings>,
}

struct Code {
    client: String,
    redirect_uri: String,
    person: String,
    tenant: String,
    nonce: Option<String>,
    challenge: String,
    /// The sign-in session's `sid`, for back-channel logout.
    sid: String,
    auth_time: i64,
    expires: i64,
    /// The `email` scope was asked for.
    email: bool,
}

struct Access {
    person: String,
    tenant: String,
    expires: i64,
    email: bool,
}

enum Purpose {
    /// A new account (or a recovery) from an invite.
    Invite { code: String, display_name: String },
}

struct Registering {
    state: PasskeyRegistration,
    user_id: Uuid,
    purpose: Purpose,
    expires: i64,
}

#[derive(Default)]
struct Pending {
    registering: HashMap<String, Registering>,
    authenticating: HashMap<String, (DiscoverableAuthentication, i64)>,
    codes: HashMap<String, Code>,
    access: HashMap<String, Access>,
}

impl Pending {
    fn sweep(&mut self) {
        let now = now_ms();
        self.registering.retain(|_, r| r.expires > now);
        self.authenticating.retain(|_, (_, e)| *e > now);
        self.codes.retain(|_, c| c.expires > now);
        self.access.retain(|_, a| a.expires > now);
    }
}

pub struct App {
    pub settings: Settings,
    /// Upstream providers people may sign in with.
    pub(crate) upstreams: Vec<crate::upstream::Upstream>,
    /// Invites each admin made in the last hour, for the rate limit.
    pub(crate) invites_made: Mutex<HashMap<String, Vec<i64>>>,
    pub store: Store,
    pub keys: Keys,
    /// Why the last key refresh failed, if it did (a problem until one succeeds).
    pub(crate) key_error: std::sync::Mutex<Option<String>>,
    /// Forward-auth decisions, reused for a minute.
    pub(crate) forward_cache: crate::forward::Cache,
    /// How reading upstream groups is going (step 4b).
    pub(crate) teams_tracker: crate::teams::Tracker,
    pub control: Control,
    webauthn: Webauthn,
    pending: Mutex<Pending>,
}

fn random(n: usize) -> String {
    let bytes: Vec<u8> = (0..n).map(|_| rand::random::<u8>()).collect();
    B64.encode(bytes)
}

fn bad(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({"error": msg.into()}))).into_response()
}

impl App {
    #[cfg(test)]
    pub fn new(settings: Settings, store: Store, keys: Keys, control: Control) -> anyhow::Result<App> {
        App::with_upstreams(settings, store, keys, control, vec![])
    }

    pub fn with_upstreams(
        settings: Settings,
        store: Store,
        keys: Keys,
        control: Control,
        upstreams: Vec<crate::upstream::Upstream>,
    ) -> anyhow::Result<App> {
        let webauthn =
            WebauthnBuilder::new(&settings.passkey_domain, &settings.origin)?.rp_name("Fleetlayer").build()?;
        Ok(App {
            settings,
            upstreams,
            invites_made: Mutex::new(HashMap::new()),
            store,
            keys,
            key_error: std::sync::Mutex::new(None),
            forward_cache: Default::default(),
            teams_tracker: crate::teams::Tracker::new(),
            control,
            webauthn,
            pending: Mutex::new(Pending::default()),
        })
    }

    pub(crate) fn client(&self, id: &str) -> Option<&Client> {
        self.settings.clients.iter().find(|c| c.name == id)
    }

    fn cookie_name(&self) -> &'static str {
        if self.settings.issuer.starts_with("https://") { "__Host-fleetlayer-signin" } else { "fleetlayer-signin" }
    }

    /// The session cookie's header value, for the tests.
    #[cfg(test)]
    pub fn session_cookie_header(&self, value: &str, max_age_secs: i64) -> String {
        self.session_cookie(value, max_age_secs).to_str().unwrap().to_string()
    }

    fn session_cookie(&self, value: &str, max_age_secs: i64) -> HeaderValue {
        let secure = if self.settings.issuer.starts_with("https://") { "; Secure" } else { "" };
        HeaderValue::from_str(&format!(
            "{}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}{secure}",
            self.cookie_name()
        ))
        .expect("a cookie of safe characters")
    }

    /// The person signed in on this browser, if any.
    pub(crate) async fn signed_in(&self, headers: &HeaderMap) -> Option<String> {
        self.session(headers).await.map(|(s, _)| s.person)
    }

    /// This browser's session, and its cookie's value (for the CSRF token).
    pub(crate) async fn session(&self, headers: &HeaderMap) -> Option<(Session, String)> {
        let value = cookie(headers, self.cookie_name())?;
        let sessions = self.store.sessions.lock().await;
        let s = sessions.sessions.get(&sha256_hex(&value))?;
        (s.expires_at > now_ms()).then(|| (s.clone(), value.clone()))
    }

    async fn start_session(&self, person: &str) -> anyhow::Result<HeaderValue> {
        self.start_session_via(person, None).await
    }

    /// A new session; `via_tenant` when it came through an upstream provider, which signs in for that tenant only.
    pub(crate) async fn start_session_via(
        &self,
        person: &str,
        via_tenant: Option<&str>,
    ) -> anyhow::Result<HeaderValue> {
        let value = random(32);
        let mut sessions = self.store.sessions.lock().await;
        let now = now_ms();
        sessions.sessions.retain(|_, s| s.expires_at > now);
        sessions.sessions.insert(
            sha256_hex(&value),
            Session {
                person: person.into(),
                created_at: now,
                expires_at: now + SESSION_MS,
                verified_at: now,
                via_tenant: via_tenant.map(String::from),
                clients: Default::default(),
                forward: Default::default(),
            },
        );
        self.store.save_sessions(&sessions)?;
        Ok(self.session_cookie(&value, SESSION_MS / 1000))
    }

    /// End this browser's session and send its logouts to the services it signed in to.
    async fn end_session(&self, headers: &HeaderMap) -> anyhow::Result<Option<String>> {
        let Some(value) = cookie(headers, self.cookie_name()) else { return Ok(None) };
        let key = sha256_hex(&value);
        let gone = {
            let mut sessions = self.store.sessions.lock().await;
            let gone = sessions.sessions.remove(&key);
            self.store.save_sessions(&sessions)?;
            gone
        };
        let Some(s) = gone else { return Ok(None) };
        self.forward_cache.forget(&s.person).await;
        self.queue(self.logouts_for(&key, &s, now_ms())).await?;
        Ok(Some(s.person))
    }

    /// Drop every access token a person holds (signing out everywhere).
    pub(crate) async fn drop_access_tokens(&self, person: &str) {
        self.pending.lock().await.access.retain(|_, a| a.person != person);
    }

    /// Rotate the signing key when it's due, and pick up a rotation made by the `keys rotate` command (every minute,
    /// from a blocking task: making a key is slow).
    pub fn refresh_keys(&self) {
        let result = self.keys.refresh(now_ms()).map_err(|e| format!("{e:#}"));
        if let Err(e) = &result {
            tracing::warn!("rotating the token-signing key: {e}");
        }
        *self.key_error.lock().expect("key error lock") = result.err();
    }

    /// What's wrong, for `/healthz` and the heartbeat.
    pub async fn problems(&self) -> Vec<String> {
        let mut out = vec![];
        if !self.store.writable() {
            out.push(format!("the sign-in folder {} can't be written", self.store.dir.display()));
        }
        if !self.control.reachable().await {
            out.push("the control plane doesn't answer".into());
        }
        if let Some(e) = self.key_error.lock().expect("key error lock").clone() {
            out.push(format!("rotating the token-signing key: {e}"));
        }
        if let Some(late) = self.late_logouts().await {
            out.push(late);
        }
        out.extend(self.teams_problems().await);
        out
    }

    /// A person's email for the `email` scope: the one an operator set on the control plane, else the latest one a
    /// linked provider verified. Either way it's verified; an address nobody vouched for is never given.
    pub(crate) async fn email(&self, person: &crate::control::Person) -> Option<String> {
        if let Some(e) = &person.email {
            return Some(e.clone());
        }
        let people = self.store.people.lock().await;
        let c = people.people.get(&person.id)?;
        c.links.iter().filter(|l| l.email.is_some()).max_by_key(|l| l.added_at).and_then(|l| l.email.clone())
    }

    /// The claims of an ID token (and userinfo) for a person in one of their tenants; with `email`, their verified
    /// email too, when they have one.
    async fn claims(&self, person: &crate::control::Person, m: &Membership, email: bool) -> Value {
        let mut c = self.base_claims(person, m);
        if email && let Some(e) = self.email(person).await {
            c["email"] = json!(e);
            c["email_verified"] = json!(true);
        }
        c
    }

    fn base_claims(&self, person: &crate::control::Person, m: &Membership) -> Value {
        json!({
            "sub": person.id,
            "name": person.display_name,
            "preferred_username": m.handle,
            "fleetlayer.dev/tenant": m.slug,
            "fleetlayer.dev/tenant_id": m.tenant,
            "fleetlayer.dev/principal": format!("person:{}", m.handle),
            "fleetlayer.dev/role": m.role,
            "fleetlayer.dev/teams": m.teams,
            "groups": m.groups,
        })
    }
}

/// A refusal's reason as a person reads it: without the control plane's line for agents ("report it to your person").
fn for_people(reason: &str) -> &str {
    reason.split(" If this stops you doing your job").next().unwrap_or(reason).trim_end()
}

pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|c| {
        let (k, v) = c.trim().split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

/// A path on this site to go back to after signing in: relative, and never to another site.
pub(crate) fn safe_return(r: Option<&str>) -> String {
    match r {
        Some(r) if r.starts_with('/') && !r.starts_with("//") && !r.contains('\\') => r.to_string(),
        _ => "/".to_string(),
    }
}

pub fn router(app: Arc<App>) -> axum::Router {
    axum::Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/jwks", get(jwks))
        .route("/authorize", get(authorize))
        .route("/token", post(token))
        .route("/userinfo", get(userinfo))
        .route("/logout", get(logout).post(logout))
        .route("/logout/everywhere", post(logout_everywhere))
        .route("/forward-auth", get(crate::forward::check))
        .route("/forward/start", get(crate::forward::start))
        .route("/login", get(login_page))
        .route("/invite", get(invite_page))
        .route("/invite/join", post(invite_join))
        .route("/passkey/login/start", post(login_start))
        .route("/passkey/login/finish", post(login_finish))
        .route("/passkey/register/start", post(register_start))
        .route("/passkey/register/finish", post(register_finish))
        .route("/api/check", post(api_check))
        .route("/admin", get(crate::admin::index))
        .route("/admin/{tenant}", get(crate::admin::show).post(crate::admin::change))
        .route("/healthz", get(healthz))
        .route("/upstream/{name}/start", get(crate::upstream::start))
        .route("/upstream/{name}/callback", get(crate::upstream::callback))
        .route("/", get(home))
        .layer(axum::middleware::from_fn_with_state(app.clone(), previous_issuer))
        .with_state(app)
}

/// A request to one of the `previous_issuers` (by its `Host`): the JWKS as it is, so tokens issued there still
/// verify; anything else is sent to the same path at the issuer. Only ever to the configured issuer, never to an
/// address from the request. Services must still be repointed: a client won't follow a redirect on a POST (the
/// token endpoint), and tokens from now on name the new issuer.
async fn previous_issuer(
    State(app): State<Arc<App>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).map(str::to_ascii_lowercase);
    let old = host.is_some_and(|h| app.settings.previous_issuers.contains(&h));
    if !old || req.uri().path() == "/jwks" {
        return next.run(req).await;
    }
    let path = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let to = format!("{}{path}", app.settings.issuer);
    (StatusCode::PERMANENT_REDIRECT, [(header::LOCATION, to)]).into_response()
}

async fn discovery(State(app): State<Arc<App>>) -> Json<Value> {
    let i = &app.settings.issuer;
    Json(json!({
        "issuer": i,
        "authorization_endpoint": format!("{i}/authorize"),
        "token_endpoint": format!("{i}/token"),
        "userinfo_endpoint": format!("{i}/userinfo"),
        "jwks_uri": format!("{i}/jwks"),
        "end_session_endpoint": format!("{i}/logout"),
        "backchannel_logout_supported": true,
        "backchannel_logout_session_supported": true,
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post", "none"],
        "scopes_supported": ["openid", "profile", "email"],
        "claims_supported": ["sub", "name", "preferred_username", "email", "email_verified", "groups", "fleetlayer.dev/tenant",
            "fleetlayer.dev/principal", "fleetlayer.dev/role", "fleetlayer.dev/teams"],
    }))
}

async fn jwks(State(app): State<Arc<App>>) -> Json<Value> {
    Json(app.keys.jwks())
}

#[derive(Deserialize)]
pub struct AuthorizeQuery {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    scope: Option<String>,
    state: Option<String>,
    nonce: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    /// Which tenant (its slug or ID): `tenant` or `fleetlayer.dev/tenant`.
    tenant: Option<String>,
    #[serde(rename = "fleetlayer.dev/tenant")]
    fleetlayer_tenant: Option<String>,
}

/// Back to the client with an error, once its redirect URI is known to be its own.
fn to_client_error(redirect_uri: &str, error: &str, state: Option<&str>) -> Response {
    let mut u = match Url::parse(redirect_uri) {
        Ok(u) => u,
        Err(_) => return bad(StatusCode::BAD_REQUEST, error),
    };
    u.query_pairs_mut().append_pair("error", error);
    if let Some(s) = state {
        u.query_pairs_mut().append_pair("state", s);
    }
    Redirect::to(u.as_str()).into_response()
}

async fn authorize(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    Query(q): Query<AuthorizeQuery>,
) -> Response {
    // Until the client and its redirect URI check out, errors are shown here, never sent anywhere.
    let Some(client) = q.client_id.as_deref().and_then(|c| app.client(c)) else {
        return page(
            StatusCode::BAD_REQUEST,
            "Unknown service",
            "<p>This service isn't registered for sign-in here.</p>",
        );
    };
    let Some(redirect_uri) = q.redirect_uri.as_deref().filter(|r| client.redirect_uris.iter().any(|x| x == r)) else {
        return page(
            StatusCode::BAD_REQUEST,
            "Wrong return address",
            "<p>That return address isn't registered for this service.</p>",
        );
    };
    let state = q.state.as_deref();
    if q.response_type.as_deref() != Some("code") {
        return to_client_error(redirect_uri, "unsupported_response_type", state);
    }
    if !q.scope.as_deref().unwrap_or("").split(' ').any(|s| s == "openid") {
        return to_client_error(redirect_uri, "invalid_scope", state);
    }
    let challenge = match (&q.code_challenge, q.code_challenge_method.as_deref()) {
        (Some(c), Some("S256")) if c.len() >= 43 => c.clone(),
        _ => return to_client_error(redirect_uri, "invalid_request", state),
    };
    let here = uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/authorize".into());
    let Some((session, cookie_value)) = app.session(&headers).await else {
        let ret: String = form_urlencoded::byte_serialize(here.as_bytes()).collect();
        return Redirect::to(&format!("/login?return={ret}")).into_response();
    };
    let session_key = sha256_hex(&cookie_value);
    let person_id = session.person.clone();
    let person = match app.control.person(&person_id).await {
        Ok(Some(p)) => p,
        Ok(None) => return page(StatusCode::FORBIDDEN, "No account", "<p>This account no longer exists.</p>"),
        Err(e) => {
            tracing::error!("asking the control plane about {person_id}: {e}");
            return page(
                StatusCode::SERVICE_UNAVAILABLE,
                "Try again soon",
                "<p>Sign-in can't reach the rest of Fleetlayer right now.</p>",
            );
        }
    };
    // Signed in through an upstream provider: tokens for its tenant only.
    let mut person = person;
    if let Some(via) = &session.via_tenant {
        let wanted = q.tenant.as_deref().or(q.fleetlayer_tenant.as_deref());
        let elsewhere =
            wanted.is_some_and(|w| person.tenants.iter().any(|m| (m.slug == w || m.tenant == w) && m.tenant != *via));
        person.tenants.retain(|m| m.tenant == *via);
        if elsewhere {
            let ret: String = form_urlencoded::byte_serialize(here.as_bytes()).collect();
            return page(
                StatusCode::FORBIDDEN,
                "Sign in with your passkey",
                &format!(
                    "<p>The way you signed in works for one place only. <a href=\"/login?return={ret}\">Sign in \
                     with your passkey</a> for this one.</p>"
                ),
            );
        }
    }
    let wanted = q.tenant.as_deref().or(q.fleetlayer_tenant.as_deref());
    let membership = match wanted {
        Some(t) => person.tenants.iter().find(|m| m.slug == t || m.tenant == t),
        None if person.tenants.len() == 1 => person.tenants.first(),
        None => None,
    };
    let Some(m) = membership else {
        if person.tenants.is_empty() || wanted.is_some() {
            return page(
                StatusCode::FORBIDDEN,
                "No access",
                &format!(
                    "<p>{} isn't a member here, so it can't sign in to {}.</p>",
                    esc(&person.display_name),
                    esc(&client.name)
                ),
            );
        }
        // Several: they choose, and the choice comes back as `tenant`.
        let links: String = person
            .tenants
            .iter()
            .map(|t| {
                let sep = if here.contains('?') { '&' } else { '?' };
                let href = format!(
                    "{here}{sep}tenant={}",
                    form_urlencoded::byte_serialize(t.slug.as_bytes()).collect::<String>()
                );
                format!("<li><a class=\"choice\" href=\"{}\">{}</a></li>", esc(&href), esc(&t.name))
            })
            .collect();
        return page(
            StatusCode::OK,
            "Choose",
            &format!("<p>Sign in to <b>{}</b> for:</p><ul class=\"choices\">{links}</ul>", esc(&client.name)),
        );
    };
    // The key rule: no code without `sign_in` on this service, decided by the tenant's policy.
    let to = format!("service:{}", client.name);
    match app.control.check(&m.tenant, &person.id, "sign_in", &to).await {
        Ok(d) if d.allowed => {}
        Ok(d) => {
            app.control.event(&m.tenant, &person.id, "refused", Some(&to), &d.reason).await;
            return page(
                StatusCode::FORBIDDEN,
                "No access",
                &format!(
                    "<p>You don't have access to <b>{}</b> in {}. If you need it, ask one of its admins.</p>\
                     <p class=\"why\">{}</p>",
                    esc(&client.name),
                    esc(&m.name),
                    esc(for_people(&d.reason))
                ),
            );
        }
        Err(e) => {
            tracing::error!("checking sign_in for {} on {to}: {e}", person.id);
            return page(
                StatusCode::SERVICE_UNAVAILABLE,
                "Try again soon",
                "<p>Sign-in can't reach the rest of Fleetlayer right now.</p>",
            );
        }
    }
    // Remember the service, so signing out reaches it.
    {
        let mut sessions = app.store.sessions.lock().await;
        if let Some(s) = sessions.sessions.get_mut(&session_key)
            && s.clients.insert(client.name.clone())
            && let Err(e) = app.store.save_sessions(&sessions)
        {
            tracing::error!("saving the session: {e:#}");
            return page(StatusCode::INTERNAL_SERVER_ERROR, "Try again", "<p>Sign-in couldn't save that.</p>");
        }
    }
    let code = random(32);
    {
        let mut p = app.pending.lock().await;
        p.sweep();
        p.codes.insert(
            code.clone(),
            Code {
                client: client.name.clone(),
                redirect_uri: redirect_uri.to_string(),
                person: person.id.clone(),
                tenant: m.tenant.clone(),
                nonce: q.nonce.clone(),
                challenge,
                sid: crate::logout::sid(&session_key),
                auth_time: now_ms() / 1000,
                expires: now_ms() + CODE_MS,
                email: q.scope.as_deref().unwrap_or("").split(' ').any(|s| s == "email"),
            },
        );
    }
    app.control.event(&m.tenant, &person.id, "signed_in", Some(&to), "passkey").await;
    let mut u = match Url::parse(redirect_uri) {
        Ok(u) => u,
        Err(_) => return bad(StatusCode::BAD_REQUEST, "invalid redirect_uri"),
    };
    u.query_pairs_mut().append_pair("code", &code);
    if let Some(s) = state {
        u.query_pairs_mut().append_pair("state", s);
    }
    Redirect::to(u.as_str()).into_response()
}

#[derive(Deserialize)]
pub struct TokenForm {
    grant_type: Option<String>,
    code: Option<String>,
    redirect_uri: Option<String>,
    code_verifier: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
}

fn token_error(status: StatusCode, error: &str) -> Response {
    (status, [(header::CACHE_CONTROL, "no-store")], Json(json!({"error": error}))).into_response()
}

/// The client a token request is from: HTTP Basic (`client_secret_basic`), the form's secret, or none (public).
fn client_auth(app: &App, headers: &HeaderMap, f: &TokenForm) -> Option<String> {
    let basic = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.split_once(':').map(|(i, p)| (i.to_string(), p.to_string())));
    let decode = |s: &str| form_urlencoded::parse(format!("x={s}").as_bytes()).next().map(|(_, v)| v.into_owned());
    let (id, secret) = match basic {
        Some((i, p)) => (decode(&i)?, Some(decode(&p)?)),
        None => (f.client_id.clone()?, f.client_secret.clone()),
    };
    let client = app.client(&id)?;
    match (&client.secret, secret) {
        (None, None) => Some(id),
        (Some(want), Some(got)) if bool::from(want.as_bytes().ct_eq(got.as_bytes())) => Some(id),
        _ => None,
    }
}

async fn token(State(app): State<Arc<App>>, headers: HeaderMap, Form(f): Form<TokenForm>) -> Response {
    if f.grant_type.as_deref() != Some("authorization_code") {
        return token_error(StatusCode::BAD_REQUEST, "unsupported_grant_type");
    }
    let Some(client) = client_auth(&app, &headers, &f) else {
        return token_error(StatusCode::UNAUTHORIZED, "invalid_client");
    };
    // A code is used once, whatever happens next.
    let code = match f.code.as_deref() {
        Some(c) => app.pending.lock().await.codes.remove(c),
        None => None,
    };
    let Some(code) = code.filter(|c| c.expires > now_ms()) else {
        return token_error(StatusCode::BAD_REQUEST, "invalid_grant");
    };
    let verifier_ok = f.code_verifier.as_deref().is_some_and(|v| {
        let got = B64.encode(Sha256::digest(v.as_bytes()));
        bool::from(got.as_bytes().ct_eq(code.challenge.as_bytes()))
    });
    if code.client != client || f.redirect_uri.as_deref() != Some(code.redirect_uri.as_str()) || !verifier_ok {
        return token_error(StatusCode::BAD_REQUEST, "invalid_grant");
    }
    // Who they are now, in case they left the tenant since the code was issued.
    let person = match app.control.person(&code.person).await {
        Ok(Some(p)) => p,
        _ => return token_error(StatusCode::BAD_REQUEST, "invalid_grant"),
    };
    let Some(m) = person.tenants.iter().find(|m| m.tenant == code.tenant) else {
        return token_error(StatusCode::BAD_REQUEST, "invalid_grant");
    };
    let now = now_ms() / 1000;
    let mut claims = app.claims(&person, m, code.email).await;
    claims["iss"] = json!(app.settings.issuer);
    claims["aud"] = json!(client);
    claims["iat"] = json!(now);
    claims["exp"] = json!(now + TOKEN_SECS);
    claims["auth_time"] = json!(code.auth_time);
    claims["sid"] = json!(code.sid);
    if let Some(n) = &code.nonce {
        claims["nonce"] = json!(n);
    }
    let id_token = match app.keys.sign(&claims) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("signing a token: {e}");
            return token_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error");
        }
    };
    let access = random(32);
    app.pending.lock().await.access.insert(
        access.clone(),
        Access {
            person: person.id.clone(),
            tenant: m.tenant.clone(),
            expires: now_ms() + TOKEN_SECS * 1000,
            email: code.email,
        },
    );
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"access_token": access, "token_type": "Bearer", "expires_in": TOKEN_SECS, "id_token": id_token})),
    )
        .into_response()
}

async fn userinfo(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let token =
        headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    let access = match token {
        Some(t) => app
            .pending
            .lock()
            .await
            .access
            .get(t)
            .filter(|a| a.expires > now_ms())
            .map(|a| (a.person.clone(), a.tenant.clone(), a.email)),
        None => None,
    };
    let Some((person, tenant, email)) = access else {
        return token_error(StatusCode::UNAUTHORIZED, "invalid_token");
    };
    match app.control.person(&person).await {
        Ok(Some(p)) => match p.tenants.iter().find(|m| m.tenant == tenant) {
            Some(m) => Json(app.claims(&p, m, email).await).into_response(),
            None => token_error(StatusCode::UNAUTHORIZED, "invalid_token"),
        },
        _ => token_error(StatusCode::UNAUTHORIZED, "invalid_token"),
    }
}

#[derive(Deserialize)]
pub struct LogoutQuery {
    post_logout_redirect_uri: Option<String>,
    client_id: Option<String>,
    state: Option<String>,
}

async fn logout(State(app): State<Arc<App>>, headers: HeaderMap, Query(q): Query<LogoutQuery>) -> Response {
    let person = app.end_session(&headers).await.ok().flatten();
    if let Some(p) = &person
        && let Ok(Some(view)) = app.control.person(p).await
    {
        for m in &view.tenants {
            app.control.event(&m.tenant, p, "signed_out", None, "").await;
        }
    }
    let clear = app.session_cookie("", 0);
    // Back to a service only if that address is registered for it.
    let back = q.post_logout_redirect_uri.as_deref().filter(|u| {
        app.settings
            .clients
            .iter()
            .filter(|c| q.client_id.as_deref().is_none_or(|id| c.name == id))
            .any(|c| c.post_logout_redirect_uris.iter().any(|x| x == u))
    });
    let mut resp = match back {
        Some(u) => {
            let mut url = Url::parse(u).expect("a registered address parses");
            if let Some(s) = &q.state {
                url.query_pairs_mut().append_pair("state", s);
            }
            Redirect::to(url.as_str()).into_response()
        }
        None => page(StatusCode::OK, "Signed out", "<p>You're signed out.</p>"),
    };
    resp.headers_mut().append(header::SET_COOKIE, clear);
    if let Some(f) = &app.settings.forward {
        resp.headers_mut().append(header::SET_COOKIE, crate::forward::fwd_cookie(&app, f, "", 0));
    }
    resp
}

/// "Sign out everywhere": every session this person has, on every browser, and the services each signed in to.
async fn logout_everywhere(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let Some(person) = app.signed_in(&headers).await else { return Redirect::to("/login").into_response() };
    if let Err(e) = app.sign_out_everywhere(&person, now_ms()).await {
        tracing::error!("signing {person} out everywhere: {e:#}");
        return page(StatusCode::INTERNAL_SERVER_ERROR, "Try again", "<p>Signing out everywhere didn't work.</p>");
    }
    if let Ok(Some(view)) = app.control.person(&person).await {
        for m in &view.tenants {
            app.control.event(&m.tenant, &person, "signed_out", None, "everywhere").await;
        }
    }
    let mut resp = page(
        StatusCode::OK,
        "Signed out everywhere",
        "<p>You're signed out on every browser and device, and of every service.</p>",
    );
    resp.headers_mut().append(header::SET_COOKIE, app.session_cookie("", 0));
    if let Some(f) = &app.settings.forward {
        resp.headers_mut().append(header::SET_COOKIE, crate::forward::fwd_cookie(&app, f, "", 0));
    }
    resp
}

#[derive(Deserialize)]
pub struct ReturnQuery {
    #[serde(rename = "return")]
    ret: Option<String>,
}

async fn login_page(State(app): State<Arc<App>>, Query(q): Query<ReturnQuery>) -> Response {
    let ret = safe_return(q.ret.as_deref());
    page(
        StatusCode::OK,
        "Sign in",
        &format!(
            "<p>Sign in with your passkey.</p>\
         <button id=\"go\" data-return=\"{}\">Sign in with a passkey</button><p id=\"msg\" class=\"why\"></p>\
         {}<p class=\"hint\">New here? Open the invite link you were sent.</p><script>{PASSKEY_JS}\nsignIn();</script>",
            esc(&ret),
            crate::upstream::buttons(&app, &ret, None)
        ),
    )
}

#[derive(Deserialize)]
pub struct InviteQuery {
    code: Option<String>,
}

async fn invite_page(State(app): State<Arc<App>>, headers: HeaderMap, Query(q): Query<InviteQuery>) -> Response {
    let Some(code) = q.code else {
        return page(StatusCode::BAD_REQUEST, "No invite", "<p>This link has no invite in it.</p>");
    };
    let warning = if app.settings.passkeys_fixed {
        "<p class=\"warn\">Passkeys made now stop working if sign-in moves to another address; use a domain you own to avoid that.</p>"
    } else {
        ""
    };
    let join = match app.signed_in(&headers).await {
        Some(p) => match app.control.person(&p).await {
            Ok(Some(view)) => format!(
                "<form method=\"post\" action=\"/invite/join\"><input type=\"hidden\" name=\"code\" value=\"{}\">\
                 <button>Join as {}</button></form><p class=\"hint\">Or make a separate account:</p>",
                esc(&code),
                esc(&view.display_name)
            ),
            _ => String::new(),
        },
        None => String::new(),
    };
    page(
        StatusCode::OK,
        "You're invited",
        &format!(
            "{join}<p>Choose the name people will see, then make a passkey on this device.</p>{warning}\
         <label>Your name <input id=\"name\" autocomplete=\"name\" maxlength=\"100\"></label>\
         <button id=\"go\" data-code=\"{}\">Make my passkey</button><p id=\"msg\" class=\"why\"></p>\
         {}<script>{PASSKEY_JS}\nregister();</script>",
            esc(&code),
            crate::upstream::buttons(&app, "/", Some(&code))
        ),
    )
}

#[derive(Deserialize)]
pub struct JoinForm {
    code: String,
}

/// Someone signed in already accepts an invite with their account.
async fn invite_join(State(app): State<Arc<App>>, headers: HeaderMap, Form(f): Form<JoinForm>) -> Response {
    let Some(person) = app.signed_in(&headers).await else { return Redirect::to("/login").into_response() };
    match app.control.redeem(&f.code, None, Some(&person)).await {
        Ok(r) => {
            app.control.event(&r.tenant, &person, "signed_in", None, "joined with an invite").await;
            page(StatusCode::OK, "Done", "<p>You've joined. You can close this page.</p>")
        }
        Err(e) => page(StatusCode::BAD_REQUEST, "That didn't work", &format!("<p>{}</p>", esc(&e.to_string()))),
    }
}

async fn login_start(State(app): State<Arc<App>>) -> Response {
    match app.webauthn.start_discoverable_authentication() {
        Ok((mut options, state)) => {
            // A button, not autofill: the browser asks at once.
            options.mediation = None;
            let id = random(16);
            let mut p = app.pending.lock().await;
            p.sweep();
            p.authenticating.insert(id.clone(), (state, now_ms() + CEREMONY_MS));
            Json(json!({"id": id, "options": options})).into_response()
        }
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct LoginFinish {
    id: String,
    credential: PublicKeyCredential,
    #[serde(rename = "return")]
    ret: Option<String>,
}

async fn login_finish(State(app): State<Arc<App>>, Json(f): Json<LoginFinish>) -> Response {
    let Some((state, _)) = app.pending.lock().await.authenticating.remove(&f.id) else {
        return bad(StatusCode::BAD_REQUEST, "that sign-in took too long; try again");
    };
    let Ok((user_id, cred_id)) = app.webauthn.identify_discoverable_authentication(&f.credential) else {
        return bad(StatusCode::BAD_REQUEST, "that passkey didn't say whose it is");
    };
    let mut people = app.store.people.lock().await;
    let found = people.people.iter_mut().find_map(|(pid, c)| {
        c.passkeys
            .iter_mut()
            .find(|k| k.user_id == user_id && AsRef::<[u8]>::as_ref(k.passkey.cred_id()) == cred_id)
            .map(|k| (pid.clone(), k))
    });
    let Some((person, stored)) = found else {
        return bad(StatusCode::UNAUTHORIZED, "this passkey isn't registered here");
    };
    match app.webauthn.finish_discoverable_authentication_v2(&f.credential, state, &stored.passkey) {
        Ok(result) => {
            stored.passkey.update_credential(&result);
            stored.last_used_at = Some(now_ms());
        }
        Err(e) => return bad(StatusCode::UNAUTHORIZED, format!("the passkey didn't check out: {e}")),
    }
    if let Err(e) = app.store.save_people(&people) {
        tracing::error!("saving people.json: {e}");
    }
    drop(people);
    match app.start_session(&person).await {
        Ok(cookie) => {
            let mut resp = Json(json!({"redirect": safe_return(f.ret.as_deref())})).into_response();
            resp.headers_mut().append(header::SET_COOKIE, cookie);
            resp
        }
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct RegisterStart {
    code: String,
    display_name: String,
}

async fn register_start(State(app): State<Arc<App>>, Json(r): Json<RegisterStart>) -> Response {
    let name = r.display_name.trim().to_string();
    if name.is_empty() || name.chars().count() > 100 {
        return bad(StatusCode::BAD_REQUEST, "a name is 1 to 100 characters");
    }
    let user_id = Uuid::new_v4();
    match app.webauthn.start_passkey_registration(user_id, &name, &name, None) {
        Ok((mut options, state)) => {
            // A passkey that names its person, so signing in needs no username.
            if let Some(sel) = options.public_key.authenticator_selection.as_mut() {
                sel.require_resident_key = true;
                sel.resident_key = Some(webauthn_rs_proto::ResidentKeyRequirement::Required);
            }
            let id = random(16);
            let mut p = app.pending.lock().await;
            p.sweep();
            p.registering.insert(
                id.clone(),
                Registering {
                    state,
                    user_id,
                    purpose: Purpose::Invite { code: r.code, display_name: name },
                    expires: now_ms() + CEREMONY_MS,
                },
            );
            Json(json!({"id": id, "options": options})).into_response()
        }
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct RegisterFinish {
    id: String,
    credential: RegisterPublicKeyCredential,
}

async fn register_finish(State(app): State<Arc<App>>, Json(f): Json<RegisterFinish>) -> Response {
    let Some(reg) = app.pending.lock().await.registering.remove(&f.id) else {
        return bad(StatusCode::BAD_REQUEST, "that took too long; try again");
    };
    let passkey = match app.webauthn.finish_passkey_registration(&f.credential, &reg.state) {
        Ok(p) => p,
        Err(e) => return bad(StatusCode::BAD_REQUEST, format!("the passkey didn't check out: {e}")),
    };
    // The passkey is good: now the invite. A recovery invite gives back the person it's for.
    let Purpose::Invite { code, display_name } = reg.purpose;
    let redeemed = match app.control.redeem(&code, Some(&display_name), None).await {
        Ok(r) => r,
        Err(e) => return bad(StatusCode::BAD_REQUEST, format!("the invite didn't work: {e}")),
    };
    {
        let mut people = app.store.people.lock().await;
        let creds = people.people.entry(redeemed.person.clone()).or_insert_with(|| Credentials {
            user_id: reg.user_id,
            passkeys: vec![],
            links: vec![],
        });
        creds.passkeys.push(StoredPasskey { passkey, added_at: now_ms(), last_used_at: None, user_id: reg.user_id });
        if let Err(e) = app.store.save_people(&people) {
            return bad(StatusCode::INTERNAL_SERVER_ERROR, format!("saving the passkey: {e}"));
        }
    }
    let detail = if redeemed.recovery { "recovered with an invite" } else { "joined with an invite" };
    app.control.event(&redeemed.tenant, &redeemed.person, "passkey_added", None, detail).await;
    match app.start_session(&redeemed.person).await {
        Ok(cookie) => {
            let mut resp = Json(json!({"redirect": "/"})).into_response();
            resp.headers_mut().append(header::SET_COOKIE, cookie);
            resp
        }
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct CheckRequest {
    tenant: String,
    person: String,
    can: String,
    to: String,
}

/// For services that aren't Fleetlayer plugins: may this person do this, in this tenant? Authenticated as a client
/// with a secret, and only about that client's own service (`service:<name>` and below).
async fn api_check(State(app): State<Arc<App>>, headers: HeaderMap, Json(r): Json<CheckRequest>) -> Response {
    let f = TokenForm {
        grant_type: None,
        code: None,
        redirect_uri: None,
        code_verifier: None,
        client_id: None,
        client_secret: None,
    };
    let Some(client) = client_auth(&app, &headers, &f).filter(|c| app.client(c).is_some_and(|c| c.secret.is_some()))
    else {
        return bad(StatusCode::UNAUTHORIZED, "a client ID and secret (HTTP Basic) are needed");
    };
    let own = format!("service:{client}");
    if r.to != own && !r.to.starts_with(&format!("{own}/")) {
        return bad(StatusCode::FORBIDDEN, format!("{client} may ask only about {own} and what's under it"));
    }
    match app.control.check(&r.tenant, &r.person, &r.can, &r.to).await {
        Ok(d) => Json(json!({"allowed": d.allowed, "rules": d.rules, "reason": d.reason})).into_response(),
        Err(e) => bad(StatusCode::BAD_GATEWAY, e.to_string()),
    }
}

async fn healthz(State(app): State<Arc<App>>) -> Response {
    let problems = app.problems().await;
    if problems.is_empty() {
        (StatusCode::OK, "ok\n").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, format!("{}\n", problems.join("\n"))).into_response()
    }
}

async fn home(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    match app.signed_in(&headers).await {
        Some(p) => {
            let name = match app.control.person(&p).await {
                Ok(Some(v)) => v.display_name,
                _ => p,
            };
            page(
                StatusCode::OK,
                "Signed in",
                &format!(
                    "<p>You're signed in as <b>{}</b>.</p><form method=\"post\" action=\"/logout\"><button>Sign \
                     out</button></form><form method=\"post\" action=\"/logout/everywhere\"><button>Sign out \
                     everywhere</button></form><p class=\"hint\">Everywhere: every browser and device signed in as \
                     you, and every service they signed in to.</p>",
                    esc(&name)
                ),
            )
        }
        None => Redirect::to("/login").into_response(),
    }
}

pub(crate) fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

pub(crate) fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{title} · Fleetlayer</title><style>{CSS}</style></head><body><main><h1>{title}</h1>{body}</main></body></html>",
        title = esc(title)
    );
    let mut resp = (status, Html(html)).into_response();
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert(
        "content-security-policy",
        HeaderValue::from_static("default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'"),
    );
    resp
}

const CSS: &str = ":root{--bg:#fafaf9;--fg:#1c1917;--muted:#78716c;--accent:#2563eb;--warn:#b45309}\
@media (prefers-color-scheme:dark){:root{--bg:#1c1917;--fg:#f5f5f4;--muted:#a8a29e;--accent:#60a5fa;--warn:#fbbf24}}\
body{margin:0;background:var(--bg);color:var(--fg);font:16px/1.5 system-ui,sans-serif}\
main{max-width:40rem;margin:12vh auto;padding:0 16px}h1{font-size:1.5rem}\
button{font:inherit;padding:.6rem 1rem;border:0;border-radius:.5rem;background:var(--accent);color:#fff;cursor:pointer}\
input{font:inherit;display:block;width:100%;box-sizing:border-box;margin:.3rem 0 1rem;padding:.5rem;border-radius:.4rem;border:1px solid var(--muted);background:transparent;color:inherit}\
.why,.hint{color:var(--muted)}a.button{display:inline-block;padding:.6rem 1rem;border-radius:.5rem;border:1px solid var(--accent);color:var(--accent);text-decoration:none}table{border-collapse:collapse;width:100%}td,th{text-align:left;padding:.3rem .4rem;border-bottom:1px solid var(--muted)}td form{display:inline}select{font:inherit}details{margin-top:2rem}summary{cursor:pointer;color:var(--muted)}.ok{color:var(--accent)}.warn{color:var(--warn)}.choices{list-style:none;padding:0}.choice{display:block;padding:.6rem 0}";

/// The browser half of the passkey ceremonies: options from the server, base64url to bytes and back.
pub(crate) const PASSKEY_JS: &str = r#"
const b2u = b => btoa(String.fromCharCode(...new Uint8Array(b))).replace(/\+/g,'-').replace(/\//g,'_').replace(/=+$/,'');
const u2b = s => Uint8Array.from(atob(s.replace(/-/g,'+').replace(/_/g,'/') + '==='.slice((s.length+3)%4)), c => c.charCodeAt(0));
const msg = t => { document.getElementById('msg').textContent = t; };
async function post(path, body) {
  const r = await fetch(path, {method:'POST', headers:{'content-type':'application/json'}, body: JSON.stringify(body)});
  const j = await r.json(); if (!r.ok) throw new Error(j.error || r.statusText); return j;
}
function signIn() {
  const go = document.getElementById('go');
  go.onclick = async () => {
    try {
      const {id, options} = await post('/passkey/login/start', {});
      const pk = options.publicKey; pk.challenge = u2b(pk.challenge);
      (pk.allowCredentials||[]).forEach(c => c.id = u2b(c.id));
      const c = await navigator.credentials.get({publicKey: pk});
      const credential = {id: c.id, rawId: b2u(c.rawId), type: c.type, extensions: {}, response: {
        authenticatorData: b2u(c.response.authenticatorData), clientDataJSON: b2u(c.response.clientDataJSON),
        signature: b2u(c.response.signature), userHandle: c.response.userHandle ? b2u(c.response.userHandle) : null}};
      const done = await post('/passkey/login/finish', {id, credential, return: go.dataset.return});
      location.href = done.redirect;
    } catch (e) { msg(e.message); }
  };
}
function register() {
  const go = document.getElementById('go');
  go.onclick = async () => {
    try {
      const display_name = document.getElementById('name').value;
      const {id, options} = await post('/passkey/register/start', {code: go.dataset.code, display_name});
      const pk = options.publicKey; pk.challenge = u2b(pk.challenge); pk.user.id = u2b(pk.user.id);
      (pk.excludeCredentials||[]).forEach(c => c.id = u2b(c.id));
      const c = await navigator.credentials.create({publicKey: pk});
      const credential = {id: c.id, rawId: b2u(c.rawId), type: c.type, extensions: {}, response: {
        attestationObject: b2u(c.response.attestationObject), clientDataJSON: b2u(c.response.clientDataJSON),
        transports: c.response.getTransports ? c.response.getTransports() : []}};
      const done = await post('/passkey/register/finish', {id, credential});
      location.href = done.redirect;
    } catch (e) { msg(e.message); }
  };
}
"#;
