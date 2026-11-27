//! Passkey step-up for an approval (`docs/plugins/signin.md`, "Confirming a request with a passkey"). Some requests
//! need a fresh passkey from the person approving them, not only a signed-in browser: opening an admin window, for
//! one. The control plane asks for it; this page shows the request, runs a new passkey check (user verification
//! required, a fresh challenge, a passkey of the signed-in person), and tells the control plane over the sign-in
//! token that this person just confirmed this approval. The control plane holds the record (five minutes, single use)
//! and accepts the approval only from that person.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;
use webauthn_rs::prelude::*;

use crate::store::now_ms;
use crate::web::{App, PASSKEY_JS, bad, esc, page, random};

const CEREMONY_MS: i64 = 5 * 60_000;

/// A step-up under way: whose, for which approval, and where to go after.
pub(crate) struct Stepping {
    state: DiscoverableAuthentication,
    person: String,
    tenant: String,
    approval: String,
    ret: Option<String>,
    pub(crate) expires: i64,
}

/// The `stepup_return_origins` setting, checked: each an `https://` origin (or `http://localhost`), nothing more.
pub fn origins(list: &[String]) -> anyhow::Result<Vec<String>> {
    list.iter()
        .map(|o| {
            let u = Url::parse(o).map_err(|e| anyhow::anyhow!("stepup_return_origins: {o}: {e}"))?;
            anyhow::ensure!(
                u.scheme() == "https" || (u.scheme() == "http" && u.host_str() == Some("localhost")),
                "stepup_return_origins: {o} isn't https"
            );
            anyhow::ensure!(
                u.path() == "/" && u.query().is_none() && u.fragment().is_none(),
                "stepup_return_origins: {o} must be only an origin, such as https://deck.example.com"
            );
            Ok(u.origin().ascii_serialization())
        })
        .collect()
}

/// Where a step-up may send the browser afterwards: a path here, or an address at the issuer's origin, a client's
/// redirect origin or a `stepup_return_origins` entry. Anything else gets the "you can close this tab" page.
pub(crate) fn allowed_return(app: &App, ret: Option<&str>) -> Option<String> {
    let r = ret?.trim();
    if r.starts_with('/') && !r.starts_with("//") && !r.contains('\\') {
        return Some(r.to_string());
    }
    let u = Url::parse(r).ok()?;
    if !matches!(u.scheme(), "https" | "http") {
        return None;
    }
    let origin = u.origin().ascii_serialization();
    let of = |s: &str| Url::parse(s).ok().map(|u| u.origin().ascii_serialization());
    let ok = of(&app.settings.issuer).as_deref() == Some(origin.as_str())
        || app
            .settings
            .clients
            .iter()
            .flat_map(|c| &c.redirect_uris)
            .any(|r| of(r).as_deref() == Some(origin.as_str()))
        || app.settings.stepup_return_origins.contains(&origin);
    ok.then(|| u.to_string())
}

#[derive(Deserialize)]
pub struct StepupQuery {
    tenant: Option<String>,
    approval: Option<String>,
    #[serde(rename = "return")]
    ret: Option<String>,
}

fn enc(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// The request's lines, for the page.
fn describe(v: &Value) -> String {
    let field = |k: &str| v[k].as_str().map(esc).unwrap_or_default();
    let detail = v["detail"].as_str().map(|d| format!("<p class=\"why\">{}</p>", esc(d))).unwrap_or_default();
    format!(
        "<table><tr><th>From</th><td>{}</td></tr><tr><th>Asks to</th><td>{}</td></tr><tr><th>On</th><td>{}</td></tr>\
         </table>{detail}",
        field("from"),
        field("verb"),
        field("to")
    )
}

pub async fn page_for(State(app): State<Arc<App>>, headers: HeaderMap, Query(q): Query<StepupQuery>) -> Response {
    let (Some(tenant), Some(approval)) = (q.tenant.as_deref(), q.approval.as_deref()) else {
        return page(
            StatusCode::BAD_REQUEST,
            "Incomplete link",
            "<p>This link doesn't say which request it's for.</p>",
        );
    };
    if app.signed_in(&headers).await.is_none() {
        let mut here = format!("/stepup?tenant={}&approval={}", enc(tenant), enc(approval));
        if let Some(r) = &q.ret {
            here.push_str(&format!("&return={}", enc(r)));
        }
        return Redirect::to(&format!("/login?return={}", enc(&here))).into_response();
    }
    let view = match app.control.approval_view(tenant, approval).await {
        Ok(Some(v)) => v,
        Ok(None) => {
            return page(StatusCode::NOT_FOUND, "No such request", "<p>There's no such request waiting here.</p>");
        }
        Err(e) => {
            tracing::error!("reading approval {approval} in {tenant}: {e:#}");
            return page(
                StatusCode::SERVICE_UNAVAILABLE,
                "Try again soon",
                "<p>Sign-in can't reach the rest of Fleetlayer right now.</p>",
            );
        }
    };
    if view["status"].as_str() != Some("pending") {
        let status = view["status"].as_str().unwrap_or("closed");
        return page(
            StatusCode::CONFLICT,
            "Already decided",
            &format!("<p>This request was already {}.</p>{}", esc(status), describe(&view)),
        );
    }
    let ret = q.ret.as_deref().and_then(|r| allowed_return(&app, Some(r))).unwrap_or_default();
    let extra = json!({"tenant": tenant, "approval": approval, "return": ret});
    page(
        StatusCode::OK,
        "Confirm this request",
        &format!(
            "<p>This request needs your passkey, even though you're signed in.</p>{}\
             <p><button id=\"go\" data-return=\"{}\">Confirm with your passkey</button></p><p id=\"msg\" class=\"why\"></p>\
             <p class=\"hint\">Not something you expected? Close this page: nothing happens without your passkey.</p>\
             <script>{PASSKEY_JS}\nsignIn('/stepup/start', '/stepup/finish', {});</script>",
            describe(&view),
            esc(&ret),
            serde_json::to_string(&extra).unwrap_or_default().replace('<', "\\u003c")
        ),
    )
}

#[derive(Deserialize)]
pub struct StartBody {
    tenant: String,
    approval: String,
    #[serde(rename = "return")]
    ret: Option<String>,
}

pub async fn start(State(app): State<Arc<App>>, headers: HeaderMap, Json(b): Json<StartBody>) -> Response {
    let Some(person) = app.signed_in(&headers).await else {
        return bad(StatusCode::UNAUTHORIZED, "sign in first");
    };
    match app.webauthn.start_discoverable_authentication() {
        Ok((mut options, state)) => {
            options.mediation = None;
            let id = random(16);
            let mut p = app.pending.lock().await;
            p.sweep();
            p.stepping.insert(
                id.clone(),
                Stepping {
                    state,
                    person,
                    tenant: b.tenant,
                    approval: b.approval,
                    ret: b.ret.filter(|r| !r.is_empty()),
                    expires: now_ms() + CEREMONY_MS,
                },
            );
            Json(json!({"id": id, "options": options})).into_response()
        }
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct FinishBody {
    id: String,
    credential: PublicKeyCredential,
}

pub async fn finish(State(app): State<Arc<App>>, headers: HeaderMap, Json(f): Json<FinishBody>) -> Response {
    let Some(st) = app.pending.lock().await.stepping.remove(&f.id) else {
        return bad(StatusCode::BAD_REQUEST, "that took too long; reload the page and try again");
    };
    // The same browser session that started it.
    if app.signed_in(&headers).await.as_deref() != Some(st.person.as_str()) {
        return bad(StatusCode::UNAUTHORIZED, "you're no longer signed in as the person who started this");
    }
    let Ok((user_id, cred_id)) = app.webauthn.identify_discoverable_authentication(&f.credential) else {
        return bad(StatusCode::BAD_REQUEST, "that passkey didn't say whose it is");
    };
    {
        let mut people = app.store.people.lock().await;
        let Some(creds) = people.people.get_mut(&st.person) else {
            return bad(StatusCode::UNAUTHORIZED, "this passkey isn't yours");
        };
        let Some(stored) = creds
            .passkeys
            .iter_mut()
            .find(|k| k.user_id == user_id && AsRef::<[u8]>::as_ref(k.passkey.cred_id()) == cred_id)
        else {
            return bad(StatusCode::UNAUTHORIZED, "this passkey isn't yours: use your own");
        };
        match app.webauthn.finish_discoverable_authentication_v2(&f.credential, st.state, &stored.passkey) {
            Ok(result) => {
                stored.passkey.update_credential(&result);
                stored.last_used_at = Some(now_ms());
            }
            Err(e) => return bad(StatusCode::UNAUTHORIZED, format!("the passkey didn't check out: {e}")),
        }
        if let Err(e) = app.store.save_people(&people) {
            tracing::error!("saving people.json: {e}");
        }
    }
    match app.control.stepup(&st.tenant, &st.person, &st.approval).await {
        Ok(_) => {
            app.control.event(&st.tenant, &st.person, "stepup", None, &format!("approval {}", st.approval)).await;
            let to = allowed_return(&app, st.ret.as_deref()).unwrap_or_else(|| "/stepup/done".into());
            Json(json!({"redirect": to})).into_response()
        }
        Err(why) => bad(StatusCode::FORBIDDEN, why),
    }
}

pub async fn done() -> Response {
    page(StatusCode::OK, "Confirmed", "<p>Confirmed with your passkey. You can close this tab.</p>")
}
