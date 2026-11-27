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
            "/v1/signin/check",
            post(|Json(b): Json<Value>| async move {
                let ok =
                    b["tenant"] == "t_acme" && b["who"] == "p_a" && b["can"] == "sign_in" && b["to"] == "service:hub";
                Json(json!({"allowed": ok, "rules": if ok { vec!["allow[0]"] } else { vec![] },
                    "reason": if ok { "allow[0]" } else { "no rule allows it" }}))
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
    let store = crate::store::Store::open(dir).unwrap();
    let keys = crate::keys::Keys::load_or_make(dir).unwrap();
    let client = |name: &str| web::Client {
        name: name.into(),
        redirect_uris: vec![format!("https://{name}.example.com/cb")],
        secret: Some("s3cret".into()),
        post_logout_redirect_uris: vec![],
    };
    let settings = web::Settings {
        issuer: ORIGIN.into(),
        origin: Url::parse(ORIGIN).unwrap(),
        passkey_domain: "example.com".into(),
        passkeys_fixed: false,
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
    for f in ["people.json", "sessions.json", "keys/signing.pem"] {
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
