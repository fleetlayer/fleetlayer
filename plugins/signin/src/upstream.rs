//! Signing in through another provider (`docs/sso.md`, "Upstream providers", step 4): a company's Google Workspace,
//! or any OpenID Connect provider, as a way for its people to prove who they are. The sign-in service is that
//! provider's client (through `fleetlayer-oidc`), and the identity it vouches for (its issuer and `sub`) links to one
//! Fleetlayer person:
//!
//! - the first time by an invite opened with "Join with <provider>", or, when the provider's settings say `join`, by
//!   joining its tenant as a member;
//! - after that, by the issuer and `sub`. Never by email: an email can change hands.
//!
//! An upstream signs in for its own tenant only: a browser signed in through Acme's Google gets tokens for Acme, not
//! for the person's other tenants (they sign in with a passkey for those).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::store::{Credentials, Link, now_ms};
use crate::web::{App, cookie, esc, page};

/// One upstream provider, as configured.
pub struct Upstream {
    /// Its name in the settings and in addresses (`/upstream/<name>/callback`).
    pub name: String,
    /// What the button says ("Continue with Google").
    pub label: String,
    /// The domain its people's verified email must be at (and, for Google Workspace, its `hd` claim name).
    pub domain: Option<String>,
    /// Google Workspace: the `hd` claim must name the domain too (an OIDC provider has no such claim).
    pub hd: bool,
    /// The tenant it signs people in for (slug or ID).
    pub tenant: String,
    /// Anyone it vouches for may join the tenant as a member.
    pub join: bool,
    pub oidc: fleetlayer_oidc::Oidc,
    /// Its groups, mapped to the tenant's teams (step 4b).
    pub teams: Option<crate::teams::Teams>,
}

const COOKIE: &str = "fleetlayer-upstream";

fn state_cookie(app: &App, value: &str, max_age: u32) -> HeaderValue {
    let secure = if app.settings.issuer.starts_with("https://") { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{COOKIE}={value}; Path=/upstream; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}"
    ))
    .expect("a cookie of safe characters")
}

/// The buttons for the sign-in page (`return` is where to go after) and the invite page (`invite` is its code).
pub fn buttons(app: &App, ret: &str, invite: Option<&str>) -> String {
    let enc = |s: &str| form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    app.upstreams
        .iter()
        .map(|u| {
            let (verb, extra) = match invite {
                Some(code) => ("Join with", format!("&invite={}", enc(code))),
                None => ("Continue with", String::new()),
            };
            format!(
                "<p><a class=\"button\" href=\"/upstream/{}/start?return={}{extra}\">{verb} {}</a></p>",
                esc(&enc(&u.name)),
                esc(&enc(ret)),
                esc(&u.label)
            )
        })
        .collect()
}

#[derive(Deserialize)]
pub struct StartQuery {
    #[serde(rename = "return")]
    ret: Option<String>,
    invite: Option<String>,
}

pub async fn start(State(app): State<Arc<App>>, Path(name): Path<String>, Query(q): Query<StartQuery>) -> Response {
    let Some(u) = app.upstreams.iter().find(|u| u.name == name) else {
        return page(StatusCode::NOT_FOUND, "Unknown provider", "<p>There's no such way to sign in here.</p>");
    };
    // Where to go after, and an invite to use, travel with the sign-in (kept here, never sent to the provider).
    let next = json!({"return": crate::web::safe_return(q.ret.as_deref()), "invite": q.invite}).to_string();
    match u.oidc.start(&next).await {
        Ok((url, state)) => {
            let mut resp = Redirect::to(&url).into_response();
            resp.headers_mut().append(header::SET_COOKIE, state_cookie(&app, &state, 600));
            resp
        }
        Err(e) => {
            tracing::warn!("starting a sign-in with {name}: {e:#}");
            page(
                StatusCode::BAD_GATEWAY,
                "Try again soon",
                &format!("<p>{} can't be reached right now.</p>", esc(&u.label)),
            )
        }
    }
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

fn claim<'a>(c: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    c.get(k).and_then(Value::as_str)
}

/// Whether the provider vouches for someone the settings accept: for a Workspace domain, its `hd` and a verified
/// email at it.
fn accepted(u: &Upstream, c: &Map<String, Value>) -> Result<(), String> {
    let Some(domain) = &u.domain else { return Ok(()) };
    if u.hd && claim(c, "hd") != Some(domain.as_str()) {
        return Err(format!("this account isn't in {domain}"));
    }
    let verified = c.get("email_verified").and_then(Value::as_bool) == Some(true);
    let at_domain = claim(c, "email").is_some_and(|e| e.to_ascii_lowercase().ends_with(&format!("@{domain}")));
    if !verified || !at_domain {
        return Err(format!("this account's email isn't a verified address at {domain}"));
    }
    Ok(())
}

pub async fn callback(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let Some(u) = app.upstreams.iter().find(|u| u.name == name) else {
        return page(StatusCode::NOT_FOUND, "Unknown provider", "<p>There's no such way to sign in here.</p>");
    };
    let fail = |status: StatusCode, msg: &str| {
        let mut r = page(
            status,
            "That didn't work",
            &format!("<p>{}</p><p><a href=\"/login\">Back to sign-in</a></p>", esc(msg)),
        );
        r.headers_mut().append(header::SET_COOKIE, state_cookie(&app, "", 0));
        r
    };
    if q.error.is_some() {
        return fail(StatusCode::FORBIDDEN, &format!("{} didn't sign you in.", u.label));
    }
    let (Some(code), Some(state)) = (q.code.as_deref(), q.state.as_deref()) else {
        return fail(StatusCode::BAD_REQUEST, "That sign-in link isn't complete. Sign in again.");
    };
    let bound = cookie(&headers, COOKIE);
    let (claims, next) = match u.oidc.finish_claims(code, state, bound.as_deref()).await {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!("a sign-in with {name} failed: {e:#}");
            return fail(StatusCode::FORBIDDEN, "Signing in didn't work. Sign in again.");
        }
    };
    if let Err(why) = accepted(u, &claims) {
        return fail(
            StatusCode::FORBIDDEN,
            &format!("{} vouched for someone this sign-in doesn't accept: {why}.", u.label),
        );
    }
    let next: Value = serde_json::from_str(&next).unwrap_or_default();
    let ret = next["return"].as_str().unwrap_or("/").to_string();
    let (iss, sub) =
        (claim(&claims, "iss").unwrap_or_default().to_string(), claim(&claims, "sub").unwrap_or_default().to_string());
    let display_name = claim(&claims, "name").or(claim(&claims, "email")).unwrap_or("New member").to_string();
    let email = claim(&claims, "email").map(str::to_string);

    // Linked already: by issuer and subject, never by email.
    let linked = {
        let people = app.store.people.lock().await;
        people.people.iter().find(|(_, c)| c.links.iter().any(|l| l.iss == iss && l.sub == sub)).map(|(p, _)| p.clone())
    };
    let person = match linked {
        Some(p) => p,
        None => {
            let redeemed = if let Some(code) = next["invite"].as_str() {
                // An invite opened with "Join with …": as the account signed in on this browser, if any.
                let existing = app.signed_in(&headers).await;
                app.control.redeem(code, Some(&display_name), existing.as_deref()).await
            } else if u.join {
                let hint = email.as_deref().and_then(|e| e.split('@').next()).unwrap_or(&display_name).to_string();
                app.control.join(&u.tenant, &display_name, &hint, &u.name).await
            } else {
                return fail(
                    StatusCode::FORBIDDEN,
                    &format!(
                        "No one here has linked this {} account yet. Ask an admin for an invite, open it, and choose \
                         \"Join with {}\".",
                        u.label, u.label
                    ),
                );
            };
            let r = match redeemed {
                Ok(r) => r,
                Err(e) => return fail(StatusCode::BAD_REQUEST, &format!("That didn't work: {e}")),
            };
            let mut people = app.store.people.lock().await;
            let creds = people.people.entry(r.person.clone()).or_insert_with(|| Credentials {
                user_id: webauthn_rs::prelude::Uuid::new_v4(),
                passkeys: vec![],
                links: vec![],
            });
            creds.links.push(Link {
                upstream: u.name.clone(),
                iss: iss.clone(),
                sub: sub.clone(),
                added_at: now_ms(),
                teams_read_at: None,
            });
            if let Err(e) = app.store.save_people(&people) {
                return fail(StatusCode::INTERNAL_SERVER_ERROR, &format!("saving the link: {e}"));
            }
            drop(people);
            app.control.event(&r.tenant, &r.person, "upstream_linked", None, &u.name).await;
            r.person
        }
    };
    let tenant_id = match app.control.person(&person).await {
        Ok(Some(v)) => v.tenants.iter().find(|m| m.slug == u.tenant || m.tenant == u.tenant).map(|m| m.tenant.clone()),
        _ => None,
    };
    let Some(tenant_id) = tenant_id else {
        return fail(
            StatusCode::FORBIDDEN,
            &format!("This {} account's person isn't a member there any more.", u.label),
        );
    };
    app.teams_at_sign_in(u, &person, &tenant_id, &sub, &claims).await;
    app.control.event(&tenant_id, &person, "signed_in", None, &u.name).await;
    match app.start_session_via(&person, Some(&tenant_id)).await {
        Ok(c) => {
            // A page, so the session cookie set here comes with the next request (fleetlayer_oidc::continue_page).
            let mut r = (
                StatusCode::OK,
                axum::response::Html(fleetlayer_oidc::continue_page(&ret, "Signed in · Fleetlayer", "Continue")),
            )
                .into_response();
            r.headers_mut().append(header::SET_COOKIE, c);
            r.headers_mut().append(header::SET_COOKIE, state_cookie(&app, "", 0));
            r
        }
        Err(e) => fail(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
