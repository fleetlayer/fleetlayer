//! A whole sign-in, against a stand-in control plane and a software passkey: an invite makes an account and its
//! passkey, a service gets a code only where the policy allows `sign_in`, the code becomes a signed ID token, and the
//! folder copied to a second instance keeps the passkey and the key (`docs/sso.md`, "Moving it").

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use webauthn_authenticator_rs::WebauthnAuthenticator;
use webauthn_authenticator_rs::softpasskey::SoftPasskey;
use webauthn_rs::prelude::Url;

use crate::web;

const ORIGIN: &str = "https://signin.example.com";

/// What the stand-in control plane was told.
#[derive(Default)]
struct Seen {
    events: Vec<String>,
}

/// The control plane's `/v1/signin/*`, for one person (A) in one tenant (Acme), where `sign_in` is allowed on
/// `service:hub` only.
async fn control_plane(seen: Arc<Mutex<Seen>>) -> String {
    use axum::Json;
    use axum::routing::{get, post};
    let person = json!({"id": "p_a", "display_name": "A", "tenants": [{
        "tenant": "t_acme", "slug": "acme", "name": "Acme", "handle": "a", "role": "admin",
        "teams": ["engineering"], "groups": ["acme", "acme/engineering", "acme/role:admin"]}]});
    let s1 = seen.clone();
    let s2 = seen.clone();
    let router = axum::Router::new()
        .route(
            "/v1/signin/admin/view",
            post(|Json(b): Json<Value>| async move {
                if b["as"] != "p_a" {
                    return (StatusCode::FORBIDDEN, Json(json!({})));
                }
                (
                    StatusCode::OK,
                    Json(json!({"tenant": "t_acme", "slug": "acme", "name": "Acme",
                    "members": [{"person": "p_a", "display_name": "A", "handle": "a", "role": "admin", "teams": []},
                                {"person": "p_b", "display_name": "B", "handle": "b", "role": "member", "teams": []}],
                    "invites": [], "teams": [{"name": "everyone", "parent": null, "members": []}]})),
                )
            }),
        )
        .route(
            "/v1/signin/admin/{action}",
            post(move |axum::extract::Path(action): axum::extract::Path<String>, Json(b): Json<Value>| {
                let s = s2.clone();
                async move {
                    s.lock().unwrap().events.push(format!("admin {action} {}", b["handle"].as_str().unwrap_or("-")));
                    if action == "member_remove" && b["handle"] == "a" {
                        return (
                            StatusCode::BAD_REQUEST,
                            "a is the tenant's last admin: make someone else an admin first".to_string(),
                        );
                    }
                    let out = if action == "invite" { json!({"code": "new-code"}) } else { json!({}) };
                    (StatusCode::OK, out.to_string())
                }
            }),
        )
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/v1/signin/person",
            post(move |Json(b): Json<Value>| {
                let p = person.clone();
                async move {
                    if b["person"] == "p_a" {
                        (StatusCode::OK, Json(p))
                    } else if b["person"] == "p_two" {
                        // In Acme and in Home.
                        let mut two = p.clone();
                        two["id"] = json!("p_two");
                        two["tenants"].as_array_mut().unwrap().push(json!({"tenant": "t_home", "slug": "home",
                            "name": "Home", "handle": "two", "role": "admin", "teams": [], "groups": ["home"]}));
                        (StatusCode::OK, Json(two))
                    } else {
                        (StatusCode::NOT_FOUND, Json(json!({})))
                    }
                }
            }),
        )
        .route(
            "/v1/signin/invite_redeem",
            post(|Json(b): Json<Value>| async move {
                if b["code"] == "good-invite" {
                    (
                        StatusCode::OK,
                        Json(json!({"tenant": "t_acme", "person": "p_a", "handle": "a", "recovery": false})),
                    )
                } else {
                    (StatusCode::BAD_REQUEST, Json(json!({"error": "no such invite"})))
                }
            }),
        )
        .route(
            "/v1/signin/teams_sync",
            post({
                let s = seen.clone();
                move |Json(b): Json<Value>| {
                    let s = s.clone();
                    async move {
                        let teams: Vec<&str> =
                            b["teams"].as_array().unwrap().iter().map(|t| t.as_str().unwrap()).collect();
                        s.lock().unwrap().events.push(format!(
                            "teams_sync {} {} {}",
                            b["person"].as_str().unwrap(),
                            b["upstream"].as_str().unwrap(),
                            teams.join(",")
                        ));
                        Json(json!({"added": [], "removed": [], "missing": []}))
                    }
                }
            }),
        )
        .route(
            "/v1/signin/join",
            post(|Json(b): Json<Value>| async move {
                assert_eq!(b["tenant"], "acme");
                let person = if b["hint"] == "two" { "p_two" } else { "p_a" };
                Json(json!({"tenant": "t_acme", "person": person, "handle": b["hint"], "recovery": false}))
            }),
        )
        .route(
            "/v1/signin/check",
            post(|Json(b): Json<Value>| async move {
                let ok = (b["tenant"] == "t_acme" || b["tenant"] == "t_home")
                    && (b["who"] == "p_a" || b["who"] == "p_two")
                    && b["can"] == "sign_in"
                    && b["to"] == "service:hub";
                Json(json!({"allowed": ok, "rules": if ok { vec!["allow[0]"] } else { vec![] },
                "reason": if ok { "allow[0]".to_string() } else {
                    format!("person:a may not sign_in {} (access control: no rule allows it). If this stops you \
                             doing your job, report it to your person.", b["to"].as_str().unwrap_or(""))
                }}))
            }),
        )
        .route(
            "/v1/signin/event",
            post(move |Json(b): Json<Value>| {
                let s = s1.clone();
                async move {
                    s.lock().unwrap().events.push(format!(
                        "{} {}",
                        b["kind"].as_str().unwrap_or(""),
                        b["client"].as_str().unwrap_or("-")
                    ));
                    Json(json!({}))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

fn app(dir: &std::path::Path, control: &str) -> Arc<web::App> {
    app_with(dir, control, None)
}

/// The same, with the hub taking back-channel logouts at `backchannel`.
fn app_with(dir: &std::path::Path, control: &str, backchannel: Option<String>) -> Arc<web::App> {
    let store = crate::store::Store::open(dir).unwrap();
    let keys = crate::keys::Keys::load_or_make(dir).unwrap();
    let client = |name: &str| web::Client {
        name: name.into(),
        redirect_uris: vec![format!("https://{name}.example.com/cb")],
        secret: Some("s3cret".into()),
        post_logout_redirect_uris: vec![],
        backchannel_logout_uri: if name == "hub" { backchannel.clone() } else { None },
    };
    let settings = web::Settings {
        issuer: ORIGIN.into(),
        origin: Url::parse(ORIGIN).unwrap(),
        passkey_domain: "example.com".into(),
        passkeys_fixed: false,
        previous_issuers: vec![],
        forward: None,
        clients: vec![client("hub"), client("vault")],
    };
    Arc::new(web::App::new(settings, store, keys, crate::control::Control::new(control, "fls_test")).unwrap())
}

struct Resp {
    status: StatusCode,
    location: Option<String>,
    cookie: Option<String>,
    body: Value,
    text: String,
}

async fn call(
    app: &Arc<web::App>,
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    body: Option<Body>,
    ctype: &str,
) -> Resp {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, c);
    }
    if body.is_some() {
        req = req.header(header::CONTENT_TYPE, ctype);
    }
    if method == "POST" && uri == "/token" {
        req = req.header(
            header::AUTHORIZATION,
            format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("hub:s3cret")),
        );
    }
    let resp = web::router(app.clone()).oneshot(req.body(body.unwrap_or_else(Body::empty)).unwrap()).await.unwrap();
    let status = resp.status();
    let location = resp.headers().get(header::LOCATION).map(|v| v.to_str().unwrap().to_string());
    let cookie =
        resp.headers().get(header::SET_COOKIE).map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string());
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let text = String::from_utf8_lossy(&bytes).to_string();
    Resp { status, location, cookie, body: serde_json::from_slice(&bytes).unwrap_or(Value::Null), text }
}

async fn post_json(app: &Arc<web::App>, uri: &str, body: Value) -> Resp {
    call(app, "POST", uri, None, Some(Body::from(body.to_string())), "application/json").await
}

/// Make an account from the invite with a software passkey; returns the session cookie.
async fn register(app: &Arc<web::App>, key: &mut SoftPasskey) -> String {
    let start = post_json(app, "/passkey/register/start", json!({"code": "good-invite", "display_name": "A"})).await;
    assert_eq!(start.status, StatusCode::OK, "{}", start.text);
    let mut options = start.body["options"].clone();
    assert_eq!(
        options["publicKey"]["authenticatorSelection"]["residentKey"], "required",
        "a passkey that names its person"
    );
    // The software key can't keep a resident key; the user handle is supplied at sign-in below instead.
    options["publicKey"]["authenticatorSelection"]["residentKey"] = json!("discouraged");
    options["publicKey"]["authenticatorSelection"]["requireResidentKey"] = json!(false);
    let cred = key.do_registration(Url::parse(ORIGIN).unwrap(), serde_json::from_value(options).unwrap()).unwrap();
    let done = post_json(app, "/passkey/register/finish", json!({"id": start.body["id"], "credential": cred})).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.text);
    done.cookie.expect("signed in")
}

/// Sign in with the software passkey, as an authenticator holding a discoverable passkey would.
async fn sign_in(app: &Arc<web::App>, key: &mut SoftPasskey) -> Resp {
    let (user_id, cred_id) = {
        let people = app.store.people.lock().await;
        let k = &people.people["p_a"].passkeys[0];
        (k.user_id, k.passkey.cred_id().clone())
    };
    let start = post_json(app, "/passkey/login/start", json!({})).await;
    assert_eq!(start.status, StatusCode::OK, "{}", start.text);
    let mut options = start.body["options"].clone();
    assert_eq!(options["publicKey"]["allowCredentials"], json!([]), "no username: the passkey says whose it is");
    options["publicKey"]["allowCredentials"] =
        json!([{"type": "public-key", "id": B64.encode(AsRef::<[u8]>::as_ref(&cred_id))}]);
    let mut cred =
        key.do_authentication(Url::parse(ORIGIN).unwrap(), serde_json::from_value(options).unwrap()).unwrap();
    cred.response.user_handle = Some(user_id.as_bytes().to_vec());
    post_json(app, "/passkey/login/finish", json!({"id": start.body["id"], "credential": cred, "return": "/"})).await
}

fn pkce() -> (String, String) {
    let verifier = "a-verifier-of-at-least-forty-three-characters-long-0123456789".to_string();
    let challenge = B64.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn authorize_uri(client: &str, challenge: &str) -> String {
    format!(
        "/authorize?response_type=code&client_id={client}&redirect_uri=https%3A%2F%2F{client}.example.com%2Fcb&scope=openid&state=xyz&nonce=n1&code_challenge={challenge}&code_challenge_method=S256"
    )
}

/// An authorize URI that also asks for the `email` scope.
fn with_email(uri: &str) -> String {
    uri.replace("scope=openid", "scope=openid%20email")
}

fn code_of(location: &str) -> String {
    let u = Url::parse(location).unwrap();
    assert_eq!(u.query_pairs().find(|(k, _)| k == "state").unwrap().1, "xyz");
    u.query_pairs().find(|(k, _)| k == "code").expect("a code").1.into_owned()
}

async fn exchange(app: &Arc<web::App>, code: &str, verifier: &str) -> Resp {
    let form = format!(
        "grant_type=authorization_code&code={code}&redirect_uri=https%3A%2F%2Fhub.example.com%2Fcb&code_verifier={verifier}"
    );
    call(app, "POST", "/token", None, Some(Body::from(form)), "application/x-www-form-urlencoded").await
}

#[tokio::test]
async fn an_invite_a_passkey_and_a_token_for_a_service_the_policy_allows() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let control = control_plane(seen.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let a = app(dir.path(), &control);
    let mut key = SoftPasskey::new(true);
    let cookie = register(&a, &mut key).await;

    // Signed in: the hub gets a code, then an ID token.
    let (verifier, challenge) = pkce();
    let r = call(&a, "GET", &authorize_uri("hub", &challenge), Some(&cookie), None, "").await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.text);
    let code = code_of(r.location.as_deref().unwrap());
    let t = exchange(&a, &code, &verifier).await;
    assert_eq!(t.status, StatusCode::OK, "{}", t.text);
    let claims = crate::keys::verify(&a.keys.jwks(), t.body["id_token"].as_str().unwrap()).unwrap();
    assert_eq!(claims["sub"], "p_a");
    assert_eq!(claims["aud"], "hub");
    assert_eq!(claims["iss"], ORIGIN);
    assert_eq!(claims["nonce"], "n1");
    assert_eq!(claims["fleetlayer.dev/tenant"], "acme");
    assert_eq!(claims["fleetlayer.dev/principal"], "person:a");
    assert_eq!(claims["groups"], json!(["acme", "acme/engineering", "acme/role:admin"]));
    // A code works once.
    assert_eq!(exchange(&a, &code, &verifier).await.status, StatusCode::BAD_REQUEST);
    // Userinfo with the access token.
    let token = t.body["access_token"].as_str().unwrap().to_string();
    let req = Request::builder()
        .uri("/userinfo")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let u = web::router(a.clone()).oneshot(req).await.unwrap();
    assert_eq!(u.status(), StatusCode::OK);

    // The policy doesn't allow `sign_in` on the vault: no code at all.
    let r = call(&a, "GET", &authorize_uri("vault", &challenge), Some(&cookie), None, "").await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.text);
    assert!(r.text.contains("don&#39;t have access") || r.text.contains("don't have access"), "{}", r.text);
    assert!(r.text.contains("no rule allows it") && r.text.contains("ask one of its admins"), "{}", r.text);
    assert!(!r.text.contains("your person"), "the line for agents isn't shown to people: {}", r.text);

    // A wrong verifier, and no PKCE at all, get nothing.
    let r = call(&a, "GET", &authorize_uri("hub", &challenge), Some(&cookie), None, "").await;
    let code = code_of(r.location.as_deref().unwrap());
    assert_eq!(
        exchange(&a, &code, "the-wrong-verifier-but-long-enough-to-look-plausible-0000").await.status,
        StatusCode::BAD_REQUEST
    );
    let no_pkce = "/authorize?response_type=code&client_id=hub&redirect_uri=https%3A%2F%2Fhub.example.com%2Fcb&scope=openid&state=xyz";
    let r = call(&a, "GET", no_pkce, Some(&cookie), None, "").await;
    assert!(r.location.as_deref().unwrap().contains("error=invalid_request"), "{:?}", r.location);
    // An unregistered return address is never redirected to.
    let evil = authorize_uri("hub", &challenge).replace("hub.example.com", "evil.example.com");
    let r = call(&a, "GET", &evil, Some(&cookie), None, "").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.location.is_none());

    // Not signed in: sent to the sign-in page, which comes back here.
    let r = call(&a, "GET", &authorize_uri("hub", &challenge), None, None, "").await;
    assert!(r.location.as_deref().unwrap().starts_with("/login?return=%2Fauthorize"), "{:?}", r.location);
    // And signing in with the passkey works without a username.
    let s = sign_in(&a, &mut key).await;
    assert_eq!(s.status, StatusCode::OK, "{}", s.text);
    assert!(s.cookie.is_some());

    let events = seen.lock().unwrap().events.clone();
    assert!(events.contains(&"passkey_added -".to_string()), "{events:?}");
    assert!(events.contains(&"signed_in service:hub".to_string()), "{events:?}");
    assert!(events.contains(&"refused service:vault".to_string()), "{events:?}");
}

#[tokio::test]
async fn moving_the_folder_keeps_passkeys_tokens_and_sessions() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let control = control_plane(seen).await;
    let first = tempfile::tempdir().unwrap();
    let a = app(first.path(), &control);
    let mut key = SoftPasskey::new(true);
    let cookie = register(&a, &mut key).await;
    let (verifier, challenge) = pkce();
    let r = call(&a, "GET", &authorize_uri("hub", &challenge), Some(&cookie), None, "").await;
    let t = exchange(&a, &code_of(r.location.as_deref().unwrap()), &verifier).await;
    let id_token = t.body["id_token"].as_str().unwrap().to_string();

    // Stop, copy the folder, start elsewhere.
    let second = tempfile::tempdir().unwrap();
    for f in ["people.json", "sessions.json", "keys/signing.pem", "keys/state.json"] {
        std::fs::create_dir_all(second.path().join("keys")).unwrap();
        std::fs::copy(first.path().join(f), second.path().join(f)).unwrap();
    }
    drop(a);
    let b = app(second.path(), &control);
    // A token issued before the move still verifies against the new instance's keys.
    assert!(crate::keys::verify(&b.keys.jwks(), &id_token).is_ok());
    // The browser stays signed in, and the passkey still signs in.
    let r = call(&b, "GET", &authorize_uri("hub", &challenge), Some(&cookie), None, "").await;
    assert!(r.status.is_redirection(), "{}", r.text);
    let s = sign_in(&b, &mut key).await;
    assert_eq!(s.status, StatusCode::OK, "{}", s.text);
}

#[tokio::test]
async fn discovery_names_the_endpoints_and_requires_pkce() {
    let control = control_plane(Arc::new(Mutex::new(Seen::default()))).await;
    let dir = tempfile::tempdir().unwrap();
    let a = app(dir.path(), &control);
    let d = call(&a, "GET", "/.well-known/openid-configuration", None, None, "").await;
    assert_eq!(d.body["issuer"], ORIGIN);
    assert_eq!(d.body["code_challenge_methods_supported"], json!(["S256"]));
    assert_eq!(d.body["jwks_uri"], format!("{ORIGIN}/jwks"));
    let h = call(&a, "GET", "/healthz", None, None, "").await;
    assert_eq!(h.status, StatusCode::OK, "{}", h.text);
}

#[tokio::test]
async fn admins_invite_and_are_asked_again_before_removing_someone() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let control = control_plane(seen.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let a = app(dir.path(), &control);
    let mut key = SoftPasskey::new(true);
    let cookie = register(&a, &mut key).await;
    let value = cookie.split_once('=').unwrap().1.to_string();
    let csrf = crate::store::sha256_hex(&format!("{value}:csrf"));

    // One place to manage: straight to it, with its people.
    let r = call(&a, "GET", "/admin", Some(&cookie), None, "").await;
    assert_eq!(r.location.as_deref(), Some("/admin/acme"));
    let r = call(&a, "GET", "/admin/acme", Some(&cookie), None, "").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert!(r.text.contains("Invite someone") && r.text.contains("<td>B</td>"), "{}", r.text);
    assert!(r.text.contains("<details><summary>Advanced</summary>"), "teams sit under Advanced");

    let post = |body: String| {
        let (a, cookie) = (a.clone(), cookie.clone());
        async move {
            call(&a, "POST", "/admin/acme", Some(&cookie), Some(Body::from(body)), "application/x-www-form-urlencoded")
                .await
        }
    };
    // An invite gives a link to send.
    let r = post(format!("csrf={csrf}&action=invite&handle=c&role=member")).await;
    assert!(r.location.as_deref().unwrap().contains("invite=new-code"), "{:?}", r.location);
    // Without the CSRF token, nothing happens.
    let r = post("csrf=wrong&action=invite&handle=d&role=member".into()).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // The control plane's refusal is shown with its reason.
    let r = post(format!("csrf={csrf}&action=member_remove&handle=a")).await;
    assert!(
        r.location.as_deref().unwrap().contains("last%20admin")
            || r.location.as_deref().unwrap().contains("last+admin"),
        "{:?}",
        r.location
    );

    // An hour later on the same session, removing someone asks for the passkey first.
    {
        let mut sessions = a.store.sessions.lock().await;
        for s in sessions.sessions.values_mut() {
            s.verified_at -= 3_600_000;
        }
    }
    let r = post(format!("csrf={csrf}&action=member_remove&handle=b")).await;
    assert!(r.location.as_deref().unwrap().starts_with("/login?return="), "{:?}", r.location);
    // An invite doesn't.
    let r = post(format!("csrf={csrf}&action=invite&handle=e&role=member")).await;
    assert!(r.location.as_deref().unwrap().contains("invite="), "{:?}", r.location);
    // Twenty invites an hour at most.
    for _ in 0..18 {
        post(format!("csrf={csrf}&action=invite&handle=f&role=member")).await;
    }
    let r = post(format!("csrf={csrf}&action=invite&handle=g&role=member")).await;
    assert!(r.location.as_deref().unwrap().contains("limit"), "{:?}", r.location);
    let removes = seen.lock().unwrap().events.iter().filter(|e| e.as_str() == "admin member_remove b").count();
    assert_eq!(removes, 0, "nothing removed without the fresh passkey");
}

#[tokio::test]
async fn on_plain_http_localhost_the_cookie_isnt_secure_and_everywhere_else_it_is() {
    // WebKit treats http://*.localhost as a secure context but drops Secure cookies there (homeserver's Mac test).
    let control = control_plane(Arc::new(Mutex::new(Seen::default()))).await;
    let dir = tempfile::tempdir().unwrap();
    let https = app(dir.path(), &control);
    let set = |a: &Arc<web::App>| a.session_cookie_header("v", 60);
    let h = set(&https);
    assert!(h.starts_with("__Host-fleetlayer-signin=v;") && h.contains("; Secure"), "{h}");
    let local_dir = tempfile::tempdir().unwrap();
    let store = crate::store::Store::open(local_dir.path()).unwrap();
    let keys = crate::keys::Keys::load_or_make(local_dir.path()).unwrap();
    let settings = web::Settings {
        issuer: "http://signin.localhost:7680".into(),
        origin: Url::parse("http://signin.localhost:7680").unwrap(),
        passkey_domain: "signin.localhost".into(),
        passkeys_fixed: true,
        previous_issuers: vec![],
        forward: None,
        clients: vec![],
    };
    let local = Arc::new(web::App::new(settings, store, keys, crate::control::Control::new(&control, "t")).unwrap());
    let h = set(&local);
    assert!(h.starts_with("fleetlayer-signin=v;") && !h.contains("Secure"), "{h}");
    assert!(h.contains("HttpOnly") && h.contains("SameSite=Lax"));
}

/// The sign-in service with one upstream: an OIDC provider standing in for Acme's Google Workspace.
async fn with_upstream(
    dir: &std::path::Path,
    control: &str,
    join: bool,
) -> (Arc<web::App>, Arc<fleetlayer_oidc::testing::FakeIssuer>) {
    let f = fleetlayer_oidc::testing::FakeIssuer::start_for("fleetlayer").await;
    let store = crate::store::Store::open(dir).unwrap();
    let keys = crate::keys::Keys::load_or_make(dir).unwrap();
    let settings = web::Settings {
        issuer: ORIGIN.into(),
        origin: Url::parse(ORIGIN).unwrap(),
        passkey_domain: "example.com".into(),
        passkeys_fixed: false,
        previous_issuers: vec![],
        forward: None,
        clients: vec![web::Client {
            name: "hub".into(),
            redirect_uris: vec!["https://hub.example.com/cb".into()],
            secret: Some("s3cret".into()),
            post_logout_redirect_uris: vec![],
            backchannel_logout_uri: None,
        }],
    };
    let oidc = fleetlayer_oidc::Oidc::with_redirect(
        &f.url,
        "fleetlayer",
        "up-secret",
        &format!("{ORIGIN}/upstream/acme-google/callback"),
        ORIGIN,
    )
    .unwrap();
    let up = crate::upstream::Upstream {
        name: "acme-google".into(),
        label: "Google".into(),
        domain: Some("acme.example".into()),
        hd: true,
        tenant: "acme".into(),
        join,
        oidc,
        teams: None,
    };
    let control = crate::control::Control::new(control, "fls_test");
    (Arc::new(web::App::with_upstreams(settings, store, keys, control, vec![up]).unwrap()), f)
}

/// Through the upstream: start, the provider answers with `claims` (given the nonce), back to the callback.
async fn via_upstream(
    a: &Arc<web::App>,
    f: &fleetlayer_oidc::testing::FakeIssuer,
    claims: impl Fn(&str) -> Value,
) -> Resp {
    use fleetlayer_oidc::testing::{good, param};
    let r = call(a, "GET", "/upstream/acme-google/start?return=%2Fadmin", None, None, "").await;
    let to = r.location.expect("off to the provider");
    assert_eq!(param(&to, "code_challenge_method"), "S256");
    assert_eq!(param(&to, "redirect_uri"), format!("{ORIGIN}/upstream/acme-google/callback"));
    let state = param(&to, "state");
    *f.token.lock().unwrap() = Some(good(f, claims(&param(&to, "nonce"))));
    call(
        a,
        "GET",
        &format!("/upstream/acme-google/callback?code=c&state={state}"),
        Some(&format!("fleetlayer-upstream={state}")),
        None,
        "",
    )
    .await
}

fn workspace_claims(f: &fleetlayer_oidc::testing::FakeIssuer, nonce: &str) -> Value {
    let now = jiff::Timestamp::now().as_second();
    json!({"iss": f.url, "aud": "fleetlayer", "sub": "google-123", "exp": now + 600, "iat": now, "nonce": nonce,
        "hd": "acme.example", "email": "sam@acme.example", "email_verified": true, "name": "Sam"})
}

#[tokio::test]
async fn a_workspace_account_joins_once_and_signs_in_by_its_subject_after() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let control = control_plane(seen.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let (a, f) = with_upstream(dir.path(), &control, true).await;
    let page = call(&a, "GET", "/login", None, None, "").await;
    assert!(page.text.contains("Continue with Google"), "{}", page.text);

    let r = via_upstream(&a, &f, |n| workspace_claims(&f, n)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert!(r.text.contains("http-equiv=\"refresh\"") && r.text.contains("/admin"));
    let cookie = r.cookie.clone().expect("signed in");
    {
        let people = a.store.people.lock().await;
        let link = &people.people["p_a"].links[0];
        assert_eq!(
            (link.upstream.as_str(), link.sub.as_str(), link.iss.as_str()),
            ("acme-google", "google-123", f.url.as_str())
        );
    }
    // The hub signs them in, for Acme, with the email Google verified.
    let (verifier, challenge) = pkce();
    let r2 = call(&a, "GET", &with_email(&authorize_uri("hub", &challenge)), Some(&cookie), None, "").await;
    assert!(r2.status.is_redirection(), "{}", r2.text);
    let t = exchange(&a, &code_of(r2.location.as_deref().unwrap()), &verifier).await;
    let id = crate::keys::verify(&a.keys.jwks(), t.body["id_token"].as_str().unwrap()).unwrap();
    assert_eq!((&id["email"], &id["email_verified"]), (&json!("sam@acme.example"), &json!(true)));
    // Again: by the subject, without joining again.
    let r = via_upstream(&a, &f, |n| workspace_claims(&f, n)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let events = seen.lock().unwrap().events.clone();
    assert_eq!(events.iter().filter(|e| e.starts_with("upstream_linked")).count(), 1, "{events:?}");
}

#[tokio::test]
async fn a_workspace_account_outside_the_domain_or_unverified_is_refused() {
    let control = control_plane(Arc::new(Mutex::new(Seen::default()))).await;
    let dir = tempfile::tempdir().unwrap();
    let (a, f) = with_upstream(dir.path(), &control, true).await;
    let r = via_upstream(&a, &f, |n| {
        let mut c = workspace_claims(&f, n);
        c["hd"] = json!("other.example");
        c
    })
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.text.contains("isn&#39;t in acme.example") || r.text.contains("isn't in acme.example"), "{}", r.text);
    let r = via_upstream(&a, &f, |n| {
        let mut c = workspace_claims(&f, n);
        c["email_verified"] = json!(false);
        c
    })
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // A personal Gmail with no hd at all.
    let r = via_upstream(&a, &f, |n| {
        let mut c = workspace_claims(&f, n);
        c.as_object_mut().unwrap().remove("hd");
        c
    })
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(a.store.people.lock().await.people.is_empty(), "nothing linked");
}

#[tokio::test]
async fn without_join_an_unlinked_account_needs_an_invite() {
    let control = control_plane(Arc::new(Mutex::new(Seen::default()))).await;
    let dir = tempfile::tempdir().unwrap();
    let (a, f) = with_upstream(dir.path(), &control, false).await;
    let r = via_upstream(&a, &f, |n| workspace_claims(&f, n)).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.text.contains("Ask an admin for an invite"), "{}", r.text);
    // With the invite: "Join with Google" redeems it and links the account.
    let page = call(&a, "GET", "/invite?code=good-invite", None, None, "").await;
    assert!(page.text.contains("Join with Google"), "{}", page.text);
    use fleetlayer_oidc::testing::{good, param};
    let r = call(&a, "GET", "/upstream/acme-google/start?return=%2F&invite=good-invite", None, None, "").await;
    let to = r.location.unwrap();
    let state = param(&to, "state");
    *f.token.lock().unwrap() = Some(good(&f, workspace_claims(&f, &param(&to, "nonce"))));
    let r = call(
        &a,
        "GET",
        &format!("/upstream/acme-google/callback?code=c&state={state}"),
        Some(&format!("fleetlayer-upstream={state}")),
        None,
        "",
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(a.store.people.lock().await.people["p_a"].links.len(), 1);
}

#[tokio::test]
async fn an_upstream_signs_in_for_its_own_tenant_only() {
    let control = control_plane(Arc::new(Mutex::new(Seen::default()))).await;
    let dir = tempfile::tempdir().unwrap();
    let (a, f) = with_upstream(dir.path(), &control, true).await;
    // Someone in Acme and Home signs in with Acme's Google.
    let r = via_upstream(&a, &f, |n| {
        let mut c = workspace_claims(&f, n);
        c["email"] = json!("two@acme.example");
        c["sub"] = json!("google-two");
        c
    })
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let cookie = r.cookie.unwrap();
    let (_, challenge) = pkce();
    // Acme: yes, and no chooser (Home isn't offered).
    let r = call(&a, "GET", &authorize_uri("hub", &challenge), Some(&cookie), None, "").await;
    assert!(r.status.is_redirection(), "{} {}", r.status, r.text);
    // Home: asked to sign in with a passkey instead.
    let r =
        call(&a, "GET", &format!("{}&tenant=home", authorize_uri("hub", &challenge)), Some(&cookie), None, "").await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.text);
    assert!(r.text.contains("Sign in with your passkey"), "{}", r.text);
}

#[test]
fn join_without_a_domain_must_be_said_outright() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("token"), "fls_x").unwrap();
    std::fs::write(dir.path().join("up"), "secret").unwrap();
    let config = |join: &str| {
        serde_json::from_value::<crate::Config>(json!({
            "dir": dir.path().join("state"), "issuer": "https://signin.example.com",
            "control_plane": "http://127.0.0.1:1", "token_file": dir.path().join("token"), "listen": "127.0.0.1:0",
            "upstreams": [{"name": "gitlab", "kind": "oidc", "issuer": "https://gitlab.com", "tenant": "acme",
                "client_id": "x", "client_secret_file": dir.path().join("up"), "join": serde_json::from_str::<Value>(join).unwrap()}],
        }))
        .unwrap()
    };
    let e = crate::build(config(r#"{"role": "member"}"#), dir.path()).err().expect("refused").to_string();
    assert!(e.contains("anyone on the internet") && e.contains("anyone_it_vouches_for"), "{e}");
    assert!(crate::build(config(r#"{"role": "member", "anyone_it_vouches_for": true}"#), dir.path()).is_ok());
    assert!(crate::build(config(r#"{"role": "admin", "anyone_it_vouches_for": true}"#), dir.path()).is_err());
}

#[tokio::test]
async fn an_oidc_upstream_with_a_domain_checks_the_verified_email_without_hd() {
    let control = control_plane(Arc::new(Mutex::new(Seen::default()))).await;
    let dir = tempfile::tempdir().unwrap();
    let (a, f) = with_upstream(dir.path(), &control, true).await;
    // The same upstream as an OIDC provider: no hd claim to check, only the verified email's domain.
    let a = {
        let mut a = Arc::try_unwrap(a).ok().expect("one owner");
        a.upstreams[0].hd = false;
        Arc::new(a)
    };
    let without_hd = |n: &str| {
        let mut c = workspace_claims(&f, n);
        c.as_object_mut().unwrap().remove("hd");
        c
    };
    assert_eq!(via_upstream(&a, &f, without_hd).await.status, StatusCode::OK);
    let elsewhere = |n: &str| {
        let mut c = without_hd(n);
        c["email"] = json!("sam@gmail.com");
        c["sub"] = json!("other");
        c
    };
    assert_eq!(via_upstream(&a, &f, elsewhere).await.status, StatusCode::FORBIDDEN);
}

/// A service's back-channel logout endpoint: keeps each logout token, and answers 503 while `up` is false.
struct Receiver {
    url: String,
    up: Arc<std::sync::atomic::AtomicBool>,
    tokens: Arc<Mutex<Vec<String>>>,
}

async fn receiver() -> Receiver {
    use std::sync::atomic::{AtomicBool, Ordering};
    let up = Arc::new(AtomicBool::new(true));
    let tokens = Arc::new(Mutex::new(vec![]));
    let (u, t) = (up.clone(), tokens.clone());
    let router = axum::Router::new().route(
        "/backchannel",
        axum::routing::post(move |axum::Form(f): axum::Form<std::collections::HashMap<String, String>>| {
            let (u, t) = (u.clone(), t.clone());
            async move {
                if !u.load(Ordering::SeqCst) {
                    return StatusCode::SERVICE_UNAVAILABLE;
                }
                t.lock().unwrap().push(f["logout_token"].clone());
                StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Receiver { url: format!("http://{addr}/backchannel"), up, tokens }
}

/// Signed in to the hub: its ID token's claims and its access token.
async fn signed_in_to_hub(a: &Arc<web::App>, cookie: &str) -> (Value, String) {
    let (verifier, challenge) = pkce();
    let r = call(a, "GET", &authorize_uri("hub", &challenge), Some(cookie), None, "").await;
    let t = exchange(a, &code_of(r.location.as_deref().unwrap()), &verifier).await;
    assert_eq!(t.status, StatusCode::OK, "{}", t.text);
    let claims = crate::keys::verify(&a.keys.jwks(), t.body["id_token"].as_str().unwrap()).unwrap();
    (claims, t.body["access_token"].as_str().unwrap().to_string())
}

fn header_of(token: &str) -> Value {
    serde_json::from_slice(&B64.decode(token.split('.').next().unwrap()).unwrap()).unwrap()
}

#[tokio::test]
async fn signing_out_sends_a_logout_token_to_each_service_the_session_signed_in_to() {
    let control = control_plane(Arc::new(Mutex::new(Seen::default()))).await;
    let rx = receiver().await;
    let dir = tempfile::tempdir().unwrap();
    let a = app_with(dir.path(), &control, Some(rx.url.clone()));
    let cookie = register(&a, &mut SoftPasskey::new(true)).await;
    let (id, _) = signed_in_to_hub(&a, &cookie).await;
    let sid = id["sid"].as_str().expect("the ID token names the session").to_string();

    let r = call(&a, "POST", "/logout", Some(&cookie), None, "").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(a.store.logouts.lock().await.logouts.len(), 1, "one service signed in, one logout");
    a.deliver_logouts().await;
    let tokens = rx.tokens.lock().unwrap().clone();
    assert_eq!(tokens.len(), 1);
    assert_eq!(header_of(&tokens[0])["typ"], "logout+jwt");
    let c = crate::keys::verify(&a.keys.jwks(), &tokens[0]).unwrap();
    assert_eq!((c["iss"].as_str(), c["aud"].as_str(), c["sub"].as_str()), (Some(ORIGIN), Some("hub"), Some("p_a")));
    assert_eq!(c["sid"], sid);
    assert!(c["events"][crate::logout::EVENT].is_object());
    assert!(c.get("nonce").is_none() && c["jti"].is_string());
    assert!(a.store.logouts.lock().await.logouts.is_empty(), "delivered, so gone");
    assert!(a.problems().await.is_empty());
}

#[tokio::test]
async fn a_logout_a_service_misses_waits_on_disk_is_retried_and_is_a_problem_after_an_hour() {
    use std::sync::atomic::Ordering;
    let control = control_plane(Arc::new(Mutex::new(Seen::default()))).await;
    let rx = receiver().await;
    let dir = tempfile::tempdir().unwrap();
    let a = app_with(dir.path(), &control, Some(rx.url.clone()));
    let cookie = register(&a, &mut SoftPasskey::new(true)).await;
    signed_in_to_hub(&a, &cookie).await;

    rx.up.store(false, Ordering::SeqCst);
    call(&a, "POST", "/logout", Some(&cookie), None, "").await;
    a.deliver_logouts().await;
    {
        let out = a.store.logouts.lock().await;
        let l = &out.logouts[0];
        assert_eq!(l.attempts, 1);
        assert!(l.next_at > crate::store::now_ms(), "backs off");
        assert!(l.last_error.as_deref().unwrap().contains("503"), "{:?}", l.last_error);
    }
    // Not due yet: not sent again.
    a.deliver_logouts().await;
    assert_eq!(a.store.logouts.lock().await.logouts[0].attempts, 1);
    // It survives a restart.
    drop(a);
    let a = app_with(dir.path(), &control, Some(rx.url.clone()));
    assert_eq!(a.store.logouts.lock().await.logouts.len(), 1);

    // Undelivered for over an hour: a problem.
    a.store.logouts.lock().await.logouts[0].queued_at -= 2 * crate::logout::LATE_MS;
    let problems = a.problems().await;
    assert!(problems.iter().any(|p| p.contains("not delivered for more than an hour")), "{problems:?}");

    // The service is back: delivered on the next try, and the problem clears.
    rx.up.store(true, Ordering::SeqCst);
    a.store.logouts.lock().await.logouts[0].next_at = 0;
    a.deliver_logouts().await;
    assert_eq!(rx.tokens.lock().unwrap().len(), 1);
    assert!(a.problems().await.is_empty());
}

#[tokio::test]
async fn a_logout_is_dropped_once_its_session_would_have_expired() {
    use std::sync::atomic::Ordering;
    let control = control_plane(Arc::new(Mutex::new(Seen::default()))).await;
    let rx = receiver().await;
    let dir = tempfile::tempdir().unwrap();
    let a = app_with(dir.path(), &control, Some(rx.url.clone()));
    let cookie = register(&a, &mut SoftPasskey::new(true)).await;
    signed_in_to_hub(&a, &cookie).await;
    rx.up.store(false, Ordering::SeqCst);
    call(&a, "POST", "/logout", Some(&cookie), None, "").await;
    a.store.logouts.lock().await.logouts[0].give_up_at = crate::store::now_ms() - 1;
    a.deliver_logouts().await;
    assert!(a.store.logouts.lock().await.logouts.is_empty());
    assert!(rx.tokens.lock().unwrap().is_empty());
}

#[tokio::test]
async fn signing_out_everywhere_ends_every_session_its_access_tokens_and_tells_the_services() {
    let control = control_plane(Arc::new(Mutex::new(Seen::default()))).await;
    let rx = receiver().await;
    let dir = tempfile::tempdir().unwrap();
    let a = app_with(dir.path(), &control, Some(rx.url.clone()));
    let mut key = SoftPasskey::new(true);
    let laptop = register(&a, &mut key).await;
    let phone = sign_in(&a, &mut key).await.cookie.expect("a second session");
    let (_, access) = signed_in_to_hub(&a, &laptop).await;

    let r = call(&a, "POST", "/logout/everywhere", Some(&phone), None, "").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert!(a.store.sessions.lock().await.sessions.is_empty(), "both browsers signed out");
    let (_, challenge) = pkce();
    let r = call(&a, "GET", &authorize_uri("hub", &challenge), Some(&laptop), None, "").await;
    assert!(r.location.as_deref().unwrap().starts_with("/login"), "{:?}", r.location);
    let req = Request::builder()
        .uri("/userinfo")
        .header(header::AUTHORIZATION, format!("Bearer {access}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(web::router(a.clone()).oneshot(req).await.unwrap().status(), StatusCode::UNAUTHORIZED);
    a.deliver_logouts().await;
    assert_eq!(rx.tokens.lock().unwrap().len(), 1, "the hub, which the laptop signed in to");
}

#[tokio::test]
async fn the_signout_command_signs_someone_out_everywhere_from_the_command_line() {
    let control = control_plane(Arc::new(Mutex::new(Seen::default()))).await;
    let dir = tempfile::tempdir().unwrap();
    let a = app(dir.path(), &control);
    let mut key = SoftPasskey::new(true);
    register(&a, &mut key).await;
    crate::logout::revoke(dir.path(), "p_a", crate::store::now_ms()).unwrap();
    a.apply_revocations().await.unwrap();
    assert!(a.store.sessions.lock().await.sessions.is_empty());
    // Signing in again afterwards works, and the revocation doesn't end the new session.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let s = sign_in(&a, &mut key).await;
    assert_eq!(s.status, StatusCode::OK);
    a.apply_revocations().await.unwrap();
    assert_eq!(a.store.sessions.lock().await.sessions.len(), 1);
}

#[tokio::test]
async fn a_previous_address_serves_the_jwks_and_sends_everything_else_to_the_issuer_only() {
    let control = control_plane(Arc::new(Mutex::new(Seen::default()))).await;
    let dir = tempfile::tempdir().unwrap();
    let mut a = Arc::try_unwrap(app(dir.path(), &control)).ok().unwrap();
    a.settings.previous_issuers = vec!["signin.old.example".into()];
    let a = Arc::new(a);
    let at = |host: &'static str, method: &'static str, uri: &'static str| {
        let req = Request::builder().method(method).uri(uri).header(header::HOST, host).body(Body::empty()).unwrap();
        let r = web::router(a.clone());
        async move { r.oneshot(req).await.unwrap() }
    };
    let r = at("signin.old.example", "GET", "/.well-known/openid-configuration").await;
    assert_eq!(r.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(r.headers()[header::LOCATION], format!("{ORIGIN}/.well-known/openid-configuration"));
    let r = at("signin.old.example", "GET", "/jwks").await;
    assert_eq!(r.status(), StatusCode::OK, "tokens issued there still verify");
    let r = at("SIGNIN.OLD.EXAMPLE", "GET", "/authorize?client_id=hub&x=1").await;
    assert_eq!(r.headers()[header::LOCATION], format!("{ORIGIN}/authorize?client_id=hub&x=1"));
    // Never to anywhere the request names.
    let r = at("signin.old.example", "GET", "//evil.example/x").await;
    assert!(r.headers()[header::LOCATION].to_str().unwrap().starts_with(&format!("{ORIGIN}/")));
    // The issuer's own address is served as usual.
    let r = at("signin.example.com", "GET", "/.well-known/openid-configuration").await;
    assert_eq!(r.status(), StatusCode::OK);
}

/// An app with forward-auth for `grafana.example.com` (the policy's `service:hub`, allowed) and `vault.example.com`
/// (`service:vault`, refused).
fn forward_app(dir: &std::path::Path, control: &str) -> Arc<web::App> {
    let mut a = Arc::try_unwrap(app(dir, control)).ok().unwrap();
    let host = |h: &str, s: &str| crate::forward::Host { host: h.into(), service: s.into(), tenant: "acme".into() };
    a.settings.forward = Some(crate::forward::Settings {
        cookie_domain: "example.com".into(),
        hosts: vec![host("grafana.example.com", "hub"), host("vault.example.com", "vault")],
    });
    Arc::new(a)
}

async fn forward_check(a: &Arc<web::App>, headers: &[(&str, &str)]) -> axum::response::Response {
    let mut req = Request::builder().uri("/forward-auth");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    web::router(a.clone()).oneshot(req.body(Body::empty()).unwrap()).await.unwrap()
}

fn set_cookies(r: &axum::response::Response) -> Vec<String> {
    r.headers().get_all(header::SET_COOKIE).iter().map(|v| v.to_str().unwrap().to_string()).collect()
}

async fn text_of(r: axum::response::Response) -> String {
    String::from_utf8_lossy(&axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap()).to_string()
}

#[tokio::test]
async fn forward_auth_lets_in_whom_the_policy_allows_and_sends_others_to_sign_in() {
    let control = control_plane(Arc::new(Mutex::new(Seen::default()))).await;
    let dir = tempfile::tempdir().unwrap();
    let a = forward_app(dir.path(), &control);
    let grafana =
        [("x-forwarded-proto", "https"), ("x-forwarded-host", "grafana.example.com"), ("x-forwarded-uri", "/d/1?x=2")];

    // No cookie: off to sign in, and back to the page after. nginx gets a 401 its config turns into that.
    let r = forward_check(&a, &grafana).await;
    assert_eq!(r.status(), StatusCode::SEE_OTHER);
    let to = r.headers()[header::LOCATION].to_str().unwrap().to_string();
    assert_eq!(to, format!("{ORIGIN}/forward/start?rd=https%3A%2F%2Fgrafana.example.com%2Fd%2F1%3Fx%3D2"));
    let r = forward_check(&a, &[("x-original-url", "https://grafana.example.com/d/1")]).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    // A host that isn't set up: refused.
    let r = forward_check(&a, &[("x-forwarded-host", "other.example.com")]).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);

    // Not signed in at /forward/start: to the sign-in page first, which comes back.
    let start = "/forward/start?rd=https%3A%2F%2Fgrafana.example.com%2Fd%2F1%3Fx%3D2";
    let r = call(&a, "GET", start, None, None, "").await;
    assert!(r.location.as_deref().unwrap().starts_with("/login?return=%2Fforward%2Fstart"), "{:?}", r.location);
    // Signed in: the cookie, on the shared domain, and on to the page.
    let session = register(&a, &mut SoftPasskey::new(true)).await;
    let req = Request::builder().uri(start).header(header::COOKIE, &session).body(Body::empty()).unwrap();
    let r = web::router(a.clone()).oneshot(req).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let set = set_cookies(&r).into_iter().find(|c| c.starts_with("__Secure-fleetlayer-fwd=")).expect("the cookie");
    assert!(set.contains("Domain=example.com") && set.contains("HttpOnly") && set.contains("Secure"), "{set}");
    assert!(text_of(r).await.contains("https://grafana.example.com/d/1?x=2"));
    let fwd = set.split(';').next().unwrap().to_string();

    // With it: in, with who they are.
    let mut with = grafana.to_vec();
    with.push(("cookie", fwd.as_str()));
    let r = forward_check(&a, &with).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["x-fleetlayer-person"], "a");
    assert_eq!(r.headers()["x-forwarded-user"], "a");
    assert_eq!(r.headers()["x-fleetlayer-tenant"], "acme");
    assert_eq!(r.headers()["x-fleetlayer-teams"], "engineering");

    // The vault isn't theirs: 403, saying only that, with no rule names.
    let r = forward_check(&a, &[("x-forwarded-host", "vault.example.com"), ("cookie", fwd.as_str())]).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert_eq!(text_of(r).await.trim(), "You're not allowed to use vault.");
    let req = Request::builder()
        .uri("/forward/start?rd=https%3A%2F%2Fvault.example.com%2F")
        .header(header::COOKIE, &session)
        .body(Body::empty())
        .unwrap();
    assert_eq!(web::router(a.clone()).oneshot(req).await.unwrap().status(), StatusCode::FORBIDDEN);

    // Signing out ends it at once, decisions included, and clears the cookie.
    assert!(a.forward_cache.len().await > 0);
    let req =
        Request::builder().method("POST").uri("/logout").header(header::COOKIE, &session).body(Body::empty()).unwrap();
    let r = web::router(a.clone()).oneshot(req).await.unwrap();
    assert!(set_cookies(&r).iter().any(|c| c.starts_with("__Secure-fleetlayer-fwd=;") && c.contains("Max-Age=0")));
    assert_eq!(a.forward_cache.len().await, 0);
    let r = forward_check(&a, &with).await;
    assert_eq!(r.status(), StatusCode::SEE_OTHER, "the cookie no longer works");
}

#[tokio::test]
async fn forward_auth_only_returns_to_a_configured_host() {
    let control = control_plane(Arc::new(Mutex::new(Seen::default()))).await;
    let dir = tempfile::tempdir().unwrap();
    let a = forward_app(dir.path(), &control);
    let f = a.settings.forward.as_ref().unwrap();
    assert!(crate::forward::safe_rd(&a, f, "https://grafana.example.com/d/1?x=2").is_some());
    assert!(crate::forward::safe_rd(&a, f, "HTTPS://Grafana.Example.com/").is_some());
    for bad in [
        "https://evil.example.org/",
        "https://grafana.example.com.evil.org/",
        "https://evilgrafana.example.com/",
        "https://user@grafana.example.com/",
        "https://grafana.example.com@evil.org/",
        "https://evil.org\\@grafana.example.com/",
        "https://evil.org#@grafana.example.com/",
        "//grafana.example.com/",
        "/grafana.example.com/",
        "http://grafana.example.com/",
        "https://grafana.example.com:444/",
        "javascript:alert(1)",
        "https:/\\grafana.example.com/",
        "https://grafana.example.com\t/",
    ] {
        assert!(crate::forward::safe_rd(&a, f, bad).is_none(), "{bad}");
    }
    let session = register(&a, &mut SoftPasskey::new(true)).await;
    let req = Request::builder()
        .uri("/forward/start?rd=https%3A%2F%2Fevil.example.org%2F")
        .header(header::COOKIE, &session)
        .body(Body::empty())
        .unwrap();
    let r = web::router(a.clone()).oneshot(req).await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(r.headers().get(header::LOCATION).is_none() && set_cookies(&r).iter().all(|c| !c.contains("fwd")));
}

#[tokio::test]
async fn signing_out_everywhere_ends_forward_auth_too() {
    let control = control_plane(Arc::new(Mutex::new(Seen::default()))).await;
    let dir = tempfile::tempdir().unwrap();
    let a = forward_app(dir.path(), &control);
    let session = register(&a, &mut SoftPasskey::new(true)).await;
    let req = Request::builder()
        .uri("/forward/start?rd=https%3A%2F%2Fgrafana.example.com%2F")
        .header(header::COOKIE, &session)
        .body(Body::empty())
        .unwrap();
    let r = web::router(a.clone()).oneshot(req).await.unwrap();
    let fwd = set_cookies(&r).into_iter().find(|c| c.starts_with("__Secure-fleetlayer-fwd=")).unwrap();
    let fwd = fwd.split(';').next().unwrap().to_string();
    let with = [("x-forwarded-host", "grafana.example.com"), ("cookie", fwd.as_str())];
    assert_eq!(forward_check(&a, &with).await.status(), StatusCode::OK);
    crate::logout::revoke(dir.path(), "p_a", crate::store::now_ms()).unwrap();
    a.apply_revocations().await.unwrap();
    assert_eq!(a.forward_cache.len().await, 0);
    assert_eq!(forward_check(&a, &with).await.status(), StatusCode::SEE_OTHER);
}

// ---------------------------------------------------------------- upstream groups to teams (step 4b)

/// The upstream app, with `teams` on its upstream.
fn with_teams(a: Arc<web::App>, teams: crate::teams::Teams) -> Arc<web::App> {
    let mut a = Arc::try_unwrap(a).ok().unwrap();
    a.upstreams[0].teams = Some(teams);
    Arc::new(a)
}

fn synced(seen: &Arc<Mutex<Seen>>) -> Vec<String> {
    seen.lock().unwrap().events.iter().filter(|e| e.starts_with("teams_sync")).cloned().collect()
}

async fn age_reads(a: &web::App, by_ms: i64) {
    let mut people = a.store.people.lock().await;
    for c in people.people.values_mut() {
        for l in &mut c.links {
            l.teams_read_at = l.teams_read_at.map(|t| t - by_ms);
        }
    }
}

#[tokio::test]
async fn an_oidc_providers_groups_claim_maps_to_teams_and_lapses_without_a_sign_in() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let control = control_plane(seen.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let (a, f) = with_upstream(dir.path(), &control, true).await;
    let a = with_teams(
        a,
        crate::teams::Teams {
            source: crate::teams::Source::Claim("groups".into()),
            map: [("engineering-group".to_string(), vec!["engineering".to_string()])].into(),
            grace_ms: 0,
            expire_ms: 30 * 86_400_000,
        },
    );
    let r = via_upstream(&a, &f, |n| {
        let mut c = workspace_claims(&f, n);
        c["groups"] = json!(["Engineering-Group", "unmapped"]);
        c
    })
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(synced(&seen), ["teams_sync p_a acme-google engineering"], "only mapped groups, before the session");

    // The daily pass leaves them alone while the last sign-in is recent, and drops them after 30 days without one.
    a.resync_teams(true).await;
    assert_eq!(synced(&seen).len(), 1);
    age_reads(&a, 31 * 86_400_000).await;
    a.resync_teams(true).await;
    assert_eq!(synced(&seen).last().unwrap(), "teams_sync p_a acme-google ");
}

/// A stand-in for Google: the token endpoint and the Directory API, for accounts set in `accounts`
/// (`active:<group>,<group>`, `suspended`, `gone`, or `error`).
struct FakeGoogle {
    url: String,
    accounts: Arc<Mutex<std::collections::HashMap<String, String>>>,
    queries: Arc<Mutex<Vec<String>>>,
}

async fn fake_google() -> FakeGoogle {
    use axum::extract::{Path, Query, RawQuery};
    use axum::routing::{get, post};
    let accounts: Arc<Mutex<std::collections::HashMap<String, String>>> = Arc::default();
    let queries: Arc<Mutex<Vec<String>>> = Arc::default();
    let (a1, a2, q) = (accounts.clone(), accounts.clone(), queries.clone());
    let router = axum::Router::new()
        .route(
            "/token",
            post(|axum::Form(f): axum::Form<std::collections::HashMap<String, String>>| async move {
                assert_eq!(f["grant_type"], "urn:ietf:params:oauth:grant-type:jwt-bearer");
                assert_eq!(f["assertion"].split('.').count(), 3);
                axum::Json(json!({"access_token": "at", "expires_in": 3600}))
            }),
        )
        .route(
            "/users/{id}",
            get(move |Path(id): Path<String>| {
                let a = a1.clone();
                async move {
                    match a.lock().unwrap().get(&id).map(String::as_str).unwrap_or("gone") {
                        "error" => (StatusCode::FORBIDDEN, axum::Json(json!({"error": {"message": "not authorized"}}))),
                        "gone" => (StatusCode::NOT_FOUND, axum::Json(json!({}))),
                        "suspended" => (StatusCode::OK, axum::Json(json!({"suspended": true}))),
                        _ => (StatusCode::OK, axum::Json(json!({"suspended": false}))),
                    }
                }
            }),
        )
        .route(
            "/groups",
            get(move |Query(p): Query<std::collections::HashMap<String, String>>, RawQuery(raw): RawQuery| {
                let (a, q) = (a2.clone(), q.clone());
                async move {
                    q.lock().unwrap().push(raw.unwrap_or_default());
                    let state = a.lock().unwrap().get(&p["userKey"]).cloned().unwrap_or_default();
                    let groups: Vec<Value> = state
                        .strip_prefix("active:")
                        .unwrap_or("")
                        .split(',')
                        .filter(|g| !g.is_empty())
                        .map(|g| json!({"email": g}))
                        .collect();
                    axum::Json(json!({"groups": groups}))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    FakeGoogle { url: format!("http://{addr}"), accounts, queries }
}

fn google_teams(g: &FakeGoogle) -> crate::teams::Teams {
    use rsa::pkcs8::{EncodePrivateKey, LineEnding};
    let key = rsa::PrivateKey::new(&mut crypto_common::getrandom::SysRng, 2048).unwrap();
    let pem = key.to_pkcs8_pem(LineEnding::LF).unwrap();
    let dir = crate::google::Directory::new("sa@example.iam", &pem, &format!("{}/token", g.url), None, &g.url).unwrap();
    crate::teams::Teams {
        source: crate::teams::Source::Google(Box::new(dir)),
        map: [("eng@acme.example".to_string(), vec!["engineering".to_string()])].into(),
        grace_ms: 24 * 3_600_000,
        expire_ms: 0,
    }
}

#[tokio::test]
async fn workspace_groups_map_to_teams_at_sign_in_and_every_day_and_a_suspended_account_is_signed_out() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let control = control_plane(seen.clone()).await;
    let g = fake_google().await;
    let dir = tempfile::tempdir().unwrap();
    let (a, f) = with_upstream(dir.path(), &control, true).await;
    let a = with_teams(a, google_teams(&g));
    // eng@ maps (case aside); other@ maps to nothing.
    g.accounts.lock().unwrap().insert("google-123".into(), "active:Eng@acme.example,other@acme.example".into());
    let r = via_upstream(&a, &f, |n| workspace_claims(&f, n)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(synced(&seen), ["teams_sync p_a acme-google engineering"]);
    assert_eq!(
        g.queries.lock().unwrap()[0],
        "userKey=google-123&maxResults=200",
        "direct groups only: no nesting asked for"
    );
    assert!(a.problems().await.iter().all(|p| !p.contains("groups")));

    // Out of the group: the daily read drops the team without a sign-in.
    g.accounts.lock().unwrap().insert("google-123".into(), "active:other@acme.example".into());
    a.resync_teams(true).await;
    assert_eq!(synced(&seen).last().unwrap(), "teams_sync p_a acme-google ");

    // Suspended: teams gone and signed out everywhere.
    assert_eq!(a.store.sessions.lock().await.sessions.len(), 1);
    g.accounts.lock().unwrap().insert("google-123".into(), "suspended".into());
    a.resync_teams(true).await;
    assert_eq!(synced(&seen).last().unwrap(), "teams_sync p_a acme-google ");
    assert!(a.store.sessions.lock().await.sessions.is_empty(), "signed out");
    assert!(seen.lock().unwrap().events.iter().any(|e| e.starts_with("signed_out")));
}

#[tokio::test]
async fn when_google_cant_be_read_teams_last_a_grace_period_and_it_becomes_a_problem_after_an_hour() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let control = control_plane(seen.clone()).await;
    let g = fake_google().await;
    let dir = tempfile::tempdir().unwrap();
    let (a, f) = with_upstream(dir.path(), &control, true).await;
    let a = with_teams(a, google_teams(&g));
    g.accounts.lock().unwrap().insert("google-123".into(), "active:eng@acme.example".into());
    via_upstream(&a, &f, |n| workspace_claims(&f, n)).await;
    assert_eq!(synced(&seen).len(), 1);

    // Google refuses: within the grace period, nothing changes.
    g.accounts.lock().unwrap().insert("google-123".into(), "error".into());
    a.resync_teams(true).await;
    assert_eq!(synced(&seen).len(), 1, "kept for now");
    assert!(a.problems().await.iter().all(|p| !p.contains("groups")), "not yet an hour");
    // Failing for over an hour: a problem.
    a.teams_tracker.states.lock().await.get_mut("acme-google").unwrap().failing_since =
        Some(crate::store::now_ms() - 2 * 3_600_000);
    let problems = a.problems().await;
    assert!(problems.iter().any(|p| p.contains("reading acme-google's groups has failed")), "{problems:?}");
    // Past the grace period: the upstream's teams go until a read works.
    age_reads(&a, 25 * 3_600_000).await;
    a.resync_teams(true).await;
    assert_eq!(synced(&seen).last().unwrap(), "teams_sync p_a acme-google ");
    // It works again: back, and the problem clears.
    g.accounts.lock().unwrap().insert("google-123".into(), "active:eng@acme.example".into());
    a.resync_teams(true).await;
    assert_eq!(synced(&seen).last().unwrap(), "teams_sync p_a acme-google engineering");
    assert!(a.problems().await.iter().all(|p| !p.contains("groups")));
}

#[test]
fn group_settings_are_checked_at_start() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("token"), "fls_x").unwrap();
    std::fs::write(dir.path().join("up"), "secret").unwrap();
    let build = |up: Value| {
        let mut u =
            json!({"name": "up", "tenant": "acme", "client_id": "x", "client_secret_file": dir.path().join("up")});
        u.as_object_mut().unwrap().extend(up.as_object().unwrap().clone());
        let cfg = serde_json::from_value::<crate::Config>(json!({
            "dir": dir.path().join("state"), "issuer": "https://signin.example.com",
            "control_plane": "http://127.0.0.1:1", "token_file": dir.path().join("token"), "listen": "127.0.0.1:0",
            "upstreams": [u],
        }))
        .unwrap();
        crate::build(cfg, dir.path()).err().map(|e| e.to_string())
    };
    let oidc = json!({"kind": "oidc", "issuer": "https://idp.example.com"});
    let with = |base: &Value, extra: Value| {
        let mut b = base.clone();
        b.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        b
    };
    assert_eq!(
        build(with(&oidc, json!({"map": {"Engineering": "engineering", "Leads": ["engineering", "leads"]}}))),
        None
    );
    assert!(build(with(&oidc, json!({"map": {"Eng": "Not A Team"}}))).unwrap().contains("isn't a team name"));
    assert!(build(with(&oidc, json!({"teams_claim": "roles"}))).unwrap().contains("set map"));
    let google = json!({"kind": "google", "domain": "acme.example"});
    assert!(build(with(&google, json!({"map": {"eng@acme.example": "engineering"}}))).unwrap().contains("teams:"));
    let missing =
        build(with(&google, json!({"teams": {"service_account_file": dir.path().join("nope.json"), "map": {}}})));
    assert!(missing.unwrap().contains("nope.json"));
}

#[tokio::test]
async fn the_email_scope_gives_a_verified_email_only_when_asked_and_only_one_someone_vouched_for() {
    let control = control_plane(Arc::new(Mutex::new(Seen::default()))).await;
    let dir = tempfile::tempdir().unwrap();
    let a = app(dir.path(), &control);
    let cookie = register(&a, &mut SoftPasskey::new(true)).await;
    let sign_in = |scope_email: bool| {
        let (a, cookie) = (a.clone(), cookie.clone());
        async move {
            let (verifier, challenge) = pkce();
            let mut uri = authorize_uri("hub", &challenge);
            if scope_email {
                uri = with_email(&uri);
            }
            let r = call(&a, "GET", &uri, Some(&cookie), None, "").await;
            let t = exchange(&a, &code_of(r.location.as_deref().unwrap()), &verifier).await;
            assert_eq!(t.status, StatusCode::OK, "{}", t.text);
            let id = crate::keys::verify(&a.keys.jwks(), t.body["id_token"].as_str().unwrap()).unwrap();
            let req = Request::builder()
                .uri("/userinfo")
                .header(header::AUTHORIZATION, format!("Bearer {}", t.body["access_token"].as_str().unwrap()))
                .body(Body::empty())
                .unwrap();
            let info = web::router(a.clone()).oneshot(req).await.unwrap();
            let info: Value =
                serde_json::from_slice(&axum::body::to_bytes(info.into_body(), 1 << 20).await.unwrap()).unwrap();
            (id, info)
        }
    };
    let discovery = call(&a, "GET", "/.well-known/openid-configuration", None, None, "").await;
    assert!(discovery.body["scopes_supported"].as_array().unwrap().contains(&json!("email")));

    // A passkey account with no email: none, even when asked.
    let (id, info) = sign_in(true).await;
    assert!(id.get("email").is_none() && info.get("email").is_none(), "{id} {info}");

    // A provider verified one: given when asked, in the ID token and userinfo, and not otherwise.
    a.store.people.lock().await.people.get_mut("p_a").unwrap().links.push(crate::store::Link {
        upstream: "acme-google".into(),
        iss: "https://accounts.google.com".into(),
        sub: "g-1".into(),
        added_at: 1,
        teams_read_at: None,
        email: Some("a@acme.example".into()),
    });
    let (id, info) = sign_in(true).await;
    assert_eq!((&id["email"], &id["email_verified"]), (&json!("a@acme.example"), &json!(true)));
    assert_eq!((&info["email"], &info["email_verified"]), (&json!("a@acme.example"), &json!(true)));
    let (id, info) = sign_in(false).await;
    assert!(id.get("email").is_none() && info.get("email").is_none(), "not asked for: {id} {info}");

    // One an operator set on the control plane comes first.
    let mut p: crate::control::Person = serde_json::from_value(json!({"id": "p_a", "display_name": "A"})).unwrap();
    p.email = Some("a@home.example".into());
    assert_eq!(a.email(&p).await.as_deref(), Some("a@home.example"));
    p.email = None;
    assert_eq!(a.email(&p).await.as_deref(), Some("a@acme.example"));
}
