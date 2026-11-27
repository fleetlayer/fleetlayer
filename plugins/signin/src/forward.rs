//! Forward-auth (`docs/sso.md`, "Forward-auth", step 5): sign-in for services that can't speak OpenID Connect. The
//! reverse proxy in front of such a service asks `GET /forward-auth` about every request; the answer is 200 with who
//! the person is (headers the service trusts), a redirect to sign in, or 403.
//!
//! The browser carries a forward-auth cookie set on a parent domain the services share (`cookie_domain`). It holds
//! only an opaque ID, kept hashed on the sign-in session it came from, so signing out, signing out everywhere and the
//! `signout` command end it at once. Access (`sign_in` on the host's `service:`) is the control plane's decision,
//! cached for a minute per person and service, so removing someone takes up to a minute to reach forward-auth.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::store::{now_ms, sha256_hex};
use crate::web::{App, cookie, esc, page};

/// How long a decision is reused.
const CACHE: Duration = Duration::from_secs(60);
/// `X-Fleetlayer-Teams` is cut, at a team boundary, to this many bytes.
pub const TEAMS_MAX: usize = 2048;

/// One host behind forward-auth.
#[derive(Debug, Clone)]
pub struct Host {
    /// `host` or `host:port`, lower-case, matched exactly.
    pub host: String,
    /// The `service:` name the policy knows it by.
    pub service: String,
    /// The tenant it belongs to (slug or ID).
    pub tenant: String,
}

#[derive(Debug, Clone)]
pub struct Settings {
    /// The parent domain the forward-auth cookie is set on: the sign-in service and every host are under it.
    pub cookie_domain: String,
    pub hosts: Vec<Host>,
}

/// Who someone is at a host, once allowed: what the headers say.
#[derive(Debug, Clone)]
struct Allowed {
    tenant: String,
    handle: String,
    teams: Vec<String>,
}

/// A decision: who they are there, or why not.
type Decision = Result<Allowed, String>;

/// Decisions by (person, host), each with when it was made.
#[derive(Default)]
pub struct Cache {
    entries: Mutex<HashMap<(String, String), (Decision, Instant)>>,
}

impl Cache {
    /// Forget a person's decisions (they signed out, or were signed out).
    pub async fn forget(&self, person: &str) {
        self.entries.lock().await.retain(|(p, _), _| p != person);
    }

    #[cfg(test)]
    pub async fn len(&self) -> usize {
        self.entries.lock().await.len()
    }
}

fn cookie_name(app: &App) -> &'static str {
    if app.settings.issuer.starts_with("https://") { "__Secure-fleetlayer-fwd" } else { "fleetlayer-fwd" }
}

/// The forward-auth cookie, or (with an empty value) one that clears it.
pub fn fwd_cookie(app: &App, f: &Settings, value: &str, max_age: i64) -> HeaderValue {
    let secure = if app.settings.issuer.starts_with("https://") { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{}={value}; Domain={}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}",
        cookie_name(app),
        f.cookie_domain
    ))
    .expect("a cookie of safe characters")
}

fn host_port(u: &url::Url) -> Option<String> {
    let h = u.host_str()?.to_ascii_lowercase();
    Some(match u.port() {
        Some(p) => format!("{h}:{p}"),
        None => h,
    })
}

/// Where to come back to after signing in, if it's a configured host: parsed, exact host match, no user info, no
/// backslashes, https (plain http only on this machine).
pub fn safe_rd<'a>(app: &App, f: &'a Settings, rd: &str) -> Option<(url::Url, &'a Host)> {
    if rd.contains('\\') || rd.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let u = url::Url::parse(rd).ok()?;
    let http_ok = u.scheme() == "http" && crate::domain::local(&u) && !app.settings.issuer.starts_with("https://");
    if !(u.scheme() == "https" || http_ok) || !u.username().is_empty() || u.password().is_some() {
        return None;
    }
    // The text must start with exactly the scheme and host it parsed to, so nothing odd hides before the path.
    let host = host_port(&u)?;
    if !rd.to_ascii_lowercase().starts_with(&format!("{}://{host}", u.scheme())) {
        return None;
    }
    let h = f.hosts.iter().find(|h| h.host == host)?;
    Some((u, h))
}

/// The original request, as the proxy describes it: nginx's `X-Original-URL`, or `X-Forwarded-Proto`, `-Host` and
/// `-Uri` (Caddy, Traefik). Also whether it's nginx, which can't follow a redirect from `auth_request`.
fn original(headers: &HeaderMap) -> Option<(String, bool)> {
    let get = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
    if let Some(u) = get("x-original-url") {
        return Some((u, true));
    }
    let host = get("x-forwarded-host")?;
    let proto = get("x-forwarded-proto").unwrap_or_else(|| "https".into());
    let uri = get("x-forwarded-uri").unwrap_or_else(|| "/".into());
    let uri = if uri.starts_with('/') { uri } else { format!("/{uri}") };
    Some((format!("{proto}://{host}{uri}"), false))
}

fn refused(service: &str) -> String {
    format!("You're not allowed to use {service}.")
}

impl App {
    /// Whether `person` may use `h`, and who they are there: the control plane's decision, reused for a minute.
    async fn forward_decide(&self, person: &str, via_tenant: Option<&str>, h: &Host) -> anyhow::Result<Decision> {
        let key = (person.to_string(), h.host.clone());
        if let Some((d, at)) = self.forward_cache.entries.lock().await.get(&key)
            && at.elapsed() < CACHE
        {
            return Ok(d.clone());
        }
        let p = self.control.person(person).await?;
        let m = p.as_ref().and_then(|p| p.tenants.iter().find(|m| m.slug == h.tenant || m.tenant == h.tenant));
        let d = match m {
            // Signed in through a provider for another tenant: not for this one.
            Some(m) if via_tenant.is_some_and(|v| v != m.tenant) => Err(refused(&h.service)),
            Some(m) => {
                let to = format!("service:{}", h.service);
                let d = self.control.check(&m.tenant, person, "sign_in", &to).await?;
                if d.allowed {
                    Ok(Allowed { tenant: m.slug.clone(), handle: m.handle.clone(), teams: m.teams.clone() })
                } else {
                    Err(refused(&h.service))
                }
            }
            None => Err(refused(&h.service)),
        };
        self.forward_cache.entries.lock().await.insert(key, (d.clone(), Instant::now()));
        Ok(d)
    }

    /// The sign-in session a forward-auth cookie belongs to: its person and `via_tenant`.
    async fn forward_session(&self, headers: &HeaderMap) -> Option<(String, Option<String>)> {
        let value = cookie(headers, cookie_name(self))?;
        let hash = sha256_hex(&value);
        let now = now_ms();
        let sessions = self.store.sessions.lock().await;
        sessions
            .sessions
            .values()
            .find(|s| s.expires_at > now && s.forward.contains(&hash))
            .map(|s| (s.person.clone(), s.via_tenant.clone()))
    }
}

fn teams_header(teams: &[String]) -> String {
    let mut out = String::new();
    for t in teams {
        let add = if out.is_empty() { t.len() } else { t.len() + 1 };
        if out.len() + add > TEAMS_MAX {
            break;
        }
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str(t);
    }
    out
}

/// `GET /forward-auth`, called by the proxy for every request to a host behind it.
pub async fn check(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let Some(f) = &app.settings.forward else { return StatusCode::NOT_FOUND.into_response() };
    let Some((url, nginx)) = original(&headers) else {
        return (StatusCode::BAD_REQUEST, "the proxy sent no X-Forwarded-Host or X-Original-URL\n").into_response();
    };
    let host = url::Url::parse(&url).ok().as_ref().and_then(host_port);
    let Some(h) = host.and_then(|host| f.hosts.iter().find(|h| h.host == host)) else {
        return (StatusCode::FORBIDDEN, "This address isn't set up for Fleetlayer sign-in.\n").into_response();
    };
    let Some((person, via)) = app.forward_session(&headers).await else {
        let rd: String = form_urlencoded::byte_serialize(url.as_bytes()).collect();
        let to = format!("{}/forward/start?rd={rd}", app.settings.issuer);
        if nginx {
            // nginx's auth_request can't pass a redirect on: its config sends 401s to /forward/start.
            return (StatusCode::UNAUTHORIZED, [(header::LOCATION, to)]).into_response();
        }
        return Redirect::to(&to).into_response();
    };
    match app.forward_decide(&person, via.as_deref(), h).await {
        Ok(Ok(a)) => {
            let mut r = StatusCode::OK.into_response();
            let hs = r.headers_mut();
            let mut set = |k: &'static str, v: &str| {
                if let Ok(v) = HeaderValue::from_str(v) {
                    hs.insert(k, v);
                }
            };
            set("x-fleetlayer-tenant", &a.tenant);
            set("x-fleetlayer-person", &a.handle);
            set("x-fleetlayer-teams", &teams_header(&a.teams));
            set("x-forwarded-user", &a.handle);
            r
        }
        Ok(Err(why)) => (StatusCode::FORBIDDEN, format!("{why}\n")).into_response(),
        Err(e) => {
            tracing::warn!("a forward-auth check for {person} at {}: {e:#}", h.host);
            (StatusCode::SERVICE_UNAVAILABLE, "Sign-in can't check that right now. Try again soon.\n").into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct StartQuery {
    rd: Option<String>,
}

/// `/forward/start?rd=<url>`: sign in if needed, check access, set the forward-auth cookie and go back to `rd`.
pub async fn start(State(app): State<Arc<App>>, headers: HeaderMap, Query(q): Query<StartQuery>) -> Response {
    let Some(f) = &app.settings.forward else { return StatusCode::NOT_FOUND.into_response() };
    let Some((rd, h)) = q.rd.as_deref().and_then(|rd| safe_rd(&app, f, rd)) else {
        return page(
            StatusCode::BAD_REQUEST,
            "Wrong address",
            "<p>That address isn't set up for Fleetlayer sign-in.</p>",
        );
    };
    let Some((session, cookie_value)) = app.session(&headers).await else {
        let here = format!(
            "/forward/start?rd={}",
            form_urlencoded::byte_serialize(rd.as_str().as_bytes()).collect::<String>()
        );
        let ret: String = form_urlencoded::byte_serialize(here.as_bytes()).collect();
        return Redirect::to(&format!("/login?return={ret}")).into_response();
    };
    match app.forward_decide(&session.person, session.via_tenant.as_deref(), h).await {
        Ok(Ok(_)) => {}
        Ok(Err(why)) => return page(StatusCode::FORBIDDEN, "No access", &format!("<p>{}</p>", esc(&why))),
        Err(e) => {
            tracing::warn!("a forward-auth check for {} at {}: {e:#}", session.person, h.host);
            return page(
                StatusCode::SERVICE_UNAVAILABLE,
                "Try again soon",
                "<p>Sign-in can't reach the rest of Fleetlayer right now.</p>",
            );
        }
    }
    let id = fleetlayer_oidc::new_id();
    {
        let mut sessions = app.store.sessions.lock().await;
        let Some(s) = sessions.sessions.get_mut(&sha256_hex(&cookie_value)) else {
            return Redirect::to("/login").into_response();
        };
        s.forward.insert(sha256_hex(&id));
        let max_age = (s.expires_at - now_ms()) / 1000;
        if let Err(e) = app.store.save_sessions(&sessions) {
            tracing::error!("saving the session: {e:#}");
            return page(StatusCode::INTERNAL_SERVER_ERROR, "Try again", "<p>Sign-in couldn't save that.</p>");
        }
        drop(sessions);
        if let Some(t) =
            app.control.person(&session.person).await.ok().flatten().and_then(|p| {
                p.tenants.into_iter().find(|m| m.slug == h.tenant || m.tenant == h.tenant).map(|m| m.tenant)
            })
        {
            app.control
                .event(&t, &session.person, "signed_in", Some(&format!("service:{}", h.service)), "forward-auth")
                .await;
        }
        let mut r =
            Html(fleetlayer_oidc::continue_page(rd.as_str(), "Signed in · Fleetlayer", "Continue")).into_response();
        r.headers_mut().append(header::SET_COOKIE, fwd_cookie(&app, f, &id, max_age));
        r
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_teams_header_is_cut_at_a_team() {
        let teams: Vec<String> = (0..1000).map(|i| format!("team-{i}")).collect();
        let h = super::teams_header(&teams);
        assert!(h.len() <= super::TEAMS_MAX);
        assert!(h.split(',').all(|t| t.starts_with("team-")));
        assert_eq!(super::teams_header(&["a".into(), "b".into()]), "a,b");
    }
}
