//! The web app: routes, sign-in, the page shell, and the two actions (deciding a request, closing a window). The app's
//! own routes live under `/_/`; every page also answers `?fragment=1` with just its `<main>`, which the page's script
//! fetches every few seconds for the live view.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, Path as UrlPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;

use crate::auth::{self, Access, Auth, Viewer};
use crate::control::Failed;
use crate::model::{Cache, Source};
use crate::views::{self, Ctx, esc};

pub struct App {
    pub source: Source,
    pub auth: Auth,
    pub base_url: Option<String>,
    pub trusted_proxies: Vec<IpAddr>,
    pub cache: Cache,
    /// The sign-in service's address, for its passkey step-up page (`/stepup`).
    pub issuer: Option<String>,
    /// What the dashboard's own token is (`GET /v1/token`).
    token: std::sync::RwLock<Option<TokenInfo>>,
}

/// The dashboard's control plane token: its scope, and the tenant it shows (people signed in to another are turned
/// away).
#[derive(Debug, Clone, Default)]
pub struct TokenInfo {
    pub scope: String,
    pub tenant: String,
    pub slug: String,
}

/// What a viewer may do through the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Powers {
    /// Decide requests and close windows.
    pub act: bool,
    /// Approve a request that opens a role window, after their passkey at the sign-in service.
    pub windows: bool,
}

type St = State<Arc<App>>;

impl App {
    pub fn new(source: Source, auth: Auth, base_url: Option<String>, trusted_proxies: Vec<IpAddr>) -> App {
        App {
            source,
            auth,
            base_url,
            trusted_proxies,
            cache: Cache::default(),
            issuer: None,
            token: std::sync::RwLock::new(None),
        }
    }

    /// Ask the control plane what the dashboard's token is (at start, then every minute).
    pub async fn refresh_scope(&self) {
        if let Source::Control(c) = &self.source {
            match c.token().await {
                Ok(t) => *self.token.write().unwrap_or_else(|e| e.into_inner()) = Some(t),
                Err(e) => tracing::warn!("asking the control plane what the dashboard's token is: {e}"),
            }
        }
    }

    fn token_scope(&self) -> Option<String> {
        self.token.read().unwrap_or_else(|e| e.into_inner()).as_ref().map(|t| t.scope.clone())
    }

    /// Whether a person signed in to `tenant` (an ID or a slug, from their ID token) belongs here: the dashboard shows
    /// one tenant, its token's. When that can't be told yet, they don't (fail closed). The demo has no tenant.
    pub async fn tenant_ok(&self, tenant: &str) -> bool {
        if matches!(self.source, Source::Demo(_)) {
            return true;
        }
        if self.token.read().unwrap_or_else(|e| e.into_inner()).is_none() {
            self.refresh_scope().await;
        }
        let t = self.token.read().unwrap_or_else(|e| e.into_inner());
        t.as_ref().is_some_and(|t| !tenant.is_empty() && (tenant == t.tenant || tenant == t.slug))
    }

    /// A signed-in person acts as themselves, through a delegate token, so the control plane checks and records them;
    /// the login key's holder acts through an admin token. Nobody approves a role window without their own passkey,
    /// so only a person, through a delegate token, does (the admin token would skip the step-up).
    pub async fn powers(&self, v: &Viewer) -> Powers {
        if matches!(self.source, Source::Demo(_)) {
            return Powers { act: true, windows: false };
        }
        if self.token_scope().is_none() {
            self.refresh_scope().await;
        }
        let scope = self.token_scope();
        match &v.person {
            Some(_) => {
                let act = scope.as_deref() == Some("delegate") && self.auth.may_act(v).await == Access::Yes;
                Powers { act, windows: act && self.issuer.is_some() && self.base_url.is_some() }
            }
            None => Powers { act: scope.as_deref() == Some("admin"), windows: false },
        }
    }

    /// Who the control plane should act as: the signed-in person, when the token is a delegate token.
    fn acting_as<'a>(&self, v: &'a Viewer) -> Option<&'a str> {
        (self.token_scope().as_deref() == Some("delegate")).then_some(())?;
        v.person.as_ref().map(|p| p.sub.as_str())
    }

    /// The sign-in service's passkey page for approval `id`, coming back to the confirm page here.
    fn stepup_url(&self, v: &Viewer, id: &str) -> Option<String> {
        let (issuer, base, person) = (self.issuer.as_deref()?, self.base_url.as_deref()?, v.person.as_ref()?);
        let e = |s: &str| form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
        let back = format!("{base}/_/approvals/{}/confirm", e(id));
        Some(format!("{issuer}/stepup?tenant={}&approval={}&return={}", e(&person.tenant), e(id), e(&back)))
    }

    /// Cookies are Secure unless the site is plain HTTP on loopback.
    fn secure_cookies(&self) -> bool {
        !self.base_url.as_deref().is_some_and(|b| b.starts_with("http://"))
    }

    /// Why the dashboard can't do its job, if it can't: the fleet can't be read with its token.
    pub async fn problem(&self) -> Option<String> {
        let s = self.cache.get(&self.source).await;
        s.errors.get("fleet").map(|e| format!("the dashboard can't read the fleet from the control plane: {e}"))
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/_/login", get(login_page).post(login))
        .route("/_/login/start", get(login_start))
        .route("/_/auth/callback", get(auth_callback))
        .route("/_/signed-out", get(signed_out))
        .route("/_/auth/backchannel-logout", post(backchannel_logout))
        .route("/_/logout", post(logout))
        .route("/_/assets/{name}", get(asset))
        .route("/_/approvals/{id}", post(decide))
        .route("/_/approvals/{id}/confirm", get(confirm))
        .route("/_/windows/{role}/close", post(close_window))
        .route("/", get(page_overview))
        .route("/machines", get(page_machines))
        .route("/sessions", get(page_sessions))
        .route("/events", get(page_events))
        .route("/access", get(page_access))
        .route("/graph", get(page_graph))
        .route("/readings", get(page_readings))
        .fallback(not_found)
        .layer(axum::middleware::map_response(security_headers))
        .with_state(app)
}

pub async fn serve(app: Arc<App>, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
    axum::serve(listener, router(app).into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

async fn security_headers(mut res: Response) -> Response {
    let h = res.headers_mut();
    let set = |h: &mut HeaderMap, k: &'static str, v: &'static str| {
        h.insert(header::HeaderName::from_static(k), HeaderValue::from_static(v));
    };
    set(
        h,
        "content-security-policy",
        "default-src 'none'; script-src 'self'; style-src 'self'; style-src-attr 'unsafe-inline'; img-src 'self'; font-src 'self'; \
        connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
    );
    set(h, "x-frame-options", "DENY");
    set(h, "referrer-policy", "no-referrer");
    set(h, "x-content-type-options", "nosniff");
    if !h.contains_key(header::CACHE_CONTROL) {
        set(h, "cache-control", "no-store");
    }
    res
}

const ASSETS: &[(&str, &str, &str)] = &[
    // Fleetlayer's shared look first (the hub's and sign-in's too), then the dashboard's own.
    (
        "dashboard.css",
        "text/css; charset=utf-8",
        concat!(include_str!("../../../web/fleetlayer.css"), include_str!("../web/dashboard.css")),
    ),
    ("dashboard.js", "text/javascript; charset=utf-8", include_str!("../web/dashboard.js")),
    ("icon.svg", "image/svg+xml", include_str!("../web/icon.svg")),
];

/// The site's three faces (web/fonts/, SIL Open Font License), which dashboard.css names in its @font-face rules.
const FONTS: &[(&str, &[u8])] = &[
    ("inter.woff2", include_bytes!("../../../web/fonts/inter.woff2")),
    ("space-grotesk.woff2", include_bytes!("../../../web/fonts/space-grotesk.woff2")),
    ("jetbrains-mono.woff2", include_bytes!("../../../web/fonts/jetbrains-mono.woff2")),
];

async fn asset(UrlPath(name): UrlPath<String>) -> Response {
    match ASSETS.iter().find(|(n, _, _)| *n == name) {
        Some((_, kind, body)) => {
            ([(header::CONTENT_TYPE, *kind), (header::CACHE_CONTROL, "no-cache")], *body).into_response()
        }
        None => match FONTS.iter().find(|(n, _)| *n == name) {
            Some((_, body)) => {
                ([(header::CONTENT_TYPE, "font/woff2"), (header::CACHE_CONTROL, "public, max-age=604800")], *body)
                    .into_response()
            }
            None => StatusCode::NOT_FOUND.into_response(),
        },
    }
}

/// 200 while the fleet can be read with the dashboard's token, 503 with why when it can't.
async fn healthz(State(app): St) -> Response {
    match app.problem().await {
        None => (StatusCode::OK, "ok").into_response(),
        Some(p) => (StatusCode::SERVICE_UNAVAILABLE, p).into_response(),
    }
}

// ------------------------------------------------------------------ the shell

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Overview,
    Machines,
    Sessions,
    Events,
    Access,
    Graph,
    Readings,
}

const NAV: &[(Page, &str, &str)] = &[
    (Page::Overview, "/", "Overview"),
    (Page::Machines, "/machines", "Machines"),
    (Page::Sessions, "/sessions", "Sessions"),
    (Page::Events, "/events", "Events"),
    (Page::Access, "/access", "Access"),
    (Page::Graph, "/graph", "Graph"),
    (Page::Readings, "/readings", "Readings"),
];

#[derive(Deserialize, Default)]
struct PageQuery {
    #[serde(default)]
    fragment: Option<String>,
    #[serde(default)]
    node: Option<String>,
    #[serde(default)]
    msg: Option<String>,
}

fn shell(page: Page, viewer: &Viewer, app: &App, refresh: &str, main: &str) -> String {
    let (_, path, title) = NAV.iter().find(|(p, _, _)| *p == page).copied().unwrap_or((page, "/", "Overview"));
    let nav: String = NAV
        .iter()
        .map(|(p, href, label)| {
            format!(r#"<a href="{href}"{}>{label}</a>"#, if *p == page { r#" aria-current="page""# } else { "" })
        })
        .collect();
    let who = if app.auth.is_open() {
        String::new()
    } else {
        format!(
            r#"<form method="post" action="/_/logout" class="who"><span class="who-name">{}</span><input type="hidden" name="csrf" value="{}"><button class="btn btn-sm btn-ghost" type="submit">Sign out</button></form>"#,
            esc(&viewer.label()),
            esc(&viewer.csrf)
        )
    };
    let _ = path;
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="color-scheme" content="light dark">
<meta name="robots" content="noindex">
<title>{title} · Fleetlayer</title>
<link rel="icon" href="/_/assets/icon.svg" type="image/svg+xml">
<link rel="stylesheet" href="/_/assets/dashboard.css">
<script src="/_/assets/dashboard.js" defer></script>
</head>
<body>
<header class="topbar"><div class="wrap topbar-in">
<a class="brand" href="/"><img src="/_/assets/icon.svg" alt="" width="26" height="26">Fleetlayer</a>
<nav class="tabs" aria-label="Dashboard">{nav}</nav>
<div class="topbar-end"><span class="live" data-state="on" aria-live="polite"><span class="live-dot"></span><span class="live-label">Live</span></span>{who}</div>
</div></header>
<main id="main" class="wrap" data-refresh="{}">{main}</main>
</body>
</html>"#,
        esc(refresh)
    )
}

fn simple(status: StatusCode, title: &str, body: &str) -> Response {
    // The sign-in page and the few full-page messages share the hub's login layout.
    let heading = if title == "Fleetlayer" { String::new() } else { format!("<h2>{}</h2>", esc(title)) };
    (
        status,
        Html(format!(
            r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="color-scheme" content="light dark"><meta name="robots" content="noindex"><title>{t} · Fleetlayer</title><link rel="icon" href="/_/assets/icon.svg" type="image/svg+xml"><link rel="stylesheet" href="/_/assets/dashboard.css"></head><body class="login-body"><main class="login"><div class="login-brand"><img src="/_/assets/icon.svg" alt="" width="32" height="32"><h1>Fleetlayer</h1></div><div class="login-card">{heading}{body}</div></main></body></html>"#,
            t = esc(title)
        )),
    )
        .into_response()
}

/// The viewer, if they may look; else the response that says why (or sends them to sign in).
#[allow(clippy::result_large_err)]
async fn viewer(app: &App, headers: &HeaderMap, here: &str) -> Result<Viewer, Response> {
    let Some(v) = app.auth.viewer(headers).await else {
        let next: String = form_urlencoded::byte_serialize(here.as_bytes()).collect();
        return Err(Redirect::to(&format!("/_/login?next={next}")).into_response());
    };
    if let Some(p) = &v.person
        && !app.tenant_ok(&p.tenant).await
    {
        return Err(simple(
            StatusCode::FORBIDDEN,
            "Not this dashboard",
            "<p>You're signed in to another place than the one this dashboard shows. Sign out and sign in to this one.</p>",
        ));
    }
    match app.auth.may_view(&v).await {
        Access::Yes => Ok(v),
        Access::No => Err(simple(
            StatusCode::FORBIDDEN,
            "No access",
            "<p>The access policy doesn't let you into the dashboard. Ask whoever runs this fleet.</p>",
        )),
        Access::CantTell(_) => Err(simple(
            StatusCode::SERVICE_UNAVAILABLE,
            "Try again soon",
            "<p>The dashboard can't check your access right now (the control plane isn't answering), so it shows nothing.</p>",
        )),
    }
}

async fn page_overview(s: St, h: HeaderMap, q: Query<PageQuery>) -> Response {
    page(s, h, q, Page::Overview).await
}

async fn page_machines(s: St, h: HeaderMap, q: Query<PageQuery>) -> Response {
    page(s, h, q, Page::Machines).await
}

async fn page_sessions(s: St, h: HeaderMap, q: Query<PageQuery>) -> Response {
    page(s, h, q, Page::Sessions).await
}

async fn page_events(s: St, h: HeaderMap, q: Query<PageQuery>) -> Response {
    page(s, h, q, Page::Events).await
}

async fn page_access(s: St, h: HeaderMap, q: Query<PageQuery>) -> Response {
    page(s, h, q, Page::Access).await
}

async fn page_graph(s: St, h: HeaderMap, q: Query<PageQuery>) -> Response {
    page(s, h, q, Page::Graph).await
}

async fn page_readings(s: St, h: HeaderMap, q: Query<PageQuery>) -> Response {
    page(s, h, q, Page::Readings).await
}

async fn page(State(app): St, headers: HeaderMap, Query(q): Query<PageQuery>, which: Page) -> Response {
    let path = NAV.iter().find(|(p, _, _)| *p == which).map(|(_, h, _)| *h).unwrap_or("/");
    let mut here = path.to_string();
    let mut refresh = format!("{path}?fragment=1");
    if let Some(n) = &q.node {
        let n: String = form_urlencoded::byte_serialize(n.as_bytes()).collect();
        here = format!("{path}?node={n}");
        refresh = format!("{path}?fragment=1&node={n}");
    }
    let v = match viewer(&app, &headers, &here).await {
        Ok(v) => v,
        Err(r) => {
            // The live view's fetch gets a status it can act on (reload, to sign in again), not a page.
            if q.fragment.is_some() {
                return (r.status(), "").into_response();
            }
            return r;
        }
    };
    let powers = app.powers(&v).await;
    let snap = app.cache.get(&app.source).await;
    let stepup = |id: &str| if powers.windows { app.stepup_url(&v, id) } else { None };
    let c = Ctx { can_act: powers.act, csrf: &v.csrf, now: views::now_ms(), stepup: &stepup, here: path };
    let mut main = match which {
        Page::Overview => views::overview(&snap, &c),
        Page::Machines => views::machines(&snap, &c),
        Page::Sessions => views::sessions(&snap, &c, q.node.as_deref()),
        Page::Events => views::events(&snap, &c),
        Page::Access => views::access(&snap, &c),
        Page::Graph => views::graph(&snap, &c),
        Page::Readings => views::readings(&snap, &c),
    };
    if let Some(m) = q.msg.as_deref().filter(|_| q.fragment.is_none()) {
        main.insert_str(
            0,
            &format!(
                r#"<div class="notice is-ok" role="status">{}<p class="notice-text">{}</p></div>"#,
                crate::icons::icon("check"),
                esc(m)
            ),
        );
    }
    if q.fragment.is_some() {
        return Html(main).into_response();
    }
    Html(shell(which, &v, &app, &refresh, &main)).into_response()
}

async fn not_found(State(app): St, headers: HeaderMap) -> Response {
    if app.auth.viewer(&headers).await.is_none() {
        return Redirect::to("/_/login").into_response();
    }
    simple(StatusCode::NOT_FOUND, "Not here", r#"<p><a href="/">Back to the overview</a></p>"#)
}

// ------------------------------------------------------------------ actions

#[derive(Deserialize)]
struct DecideForm {
    csrf: String,
    decision: String,
    /// The page the form was on, to go back to (the browser sends no Referer: `no-referrer`).
    #[serde(default)]
    back: Option<String>,
}

#[derive(Deserialize)]
struct CsrfForm {
    csrf: String,
    #[serde(default)]
    back: Option<String>,
}

/// Where to go back to after an action: the page the form was on, when that's a page here, else Access.
fn back(from: Option<&str>, msg: &str) -> Response {
    let from = from.and_then(|f| NAV.iter().find(|(_, h, _)| *h == f).map(|(_, h, _)| *h)).unwrap_or("/access");
    let m: String = form_urlencoded::byte_serialize(msg.as_bytes()).collect();
    Redirect::to(&format!("{from}?msg={m}")).into_response()
}

/// The viewer, when they may act and the form is theirs.
#[allow(clippy::result_large_err)]
async fn actor(app: &App, headers: &HeaderMap, csrf: &str) -> Result<Viewer, Response> {
    let v = viewer(app, headers, "/access").await?;
    if !auth_eq(&v.csrf, csrf) {
        return Err(simple(
            StatusCode::FORBIDDEN,
            "Try again",
            "<p>That form had expired. Go back, reload and try again.</p>",
        ));
    }
    if v.person.is_some() {
        match app.auth.may_act(&v).await {
            Access::Yes => {}
            Access::No => {
                return Err(simple(
                    StatusCode::FORBIDDEN,
                    "Not yours to decide",
                    "<p>Deciding requests and closing windows needs <code>admin</code> on the dashboard in the access policy.</p>",
                ));
            }
            Access::CantTell(_) => {
                return Err(simple(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Try again soon",
                    "<p>Your access can't be checked right now.</p>",
                ));
            }
        }
    }
    if !app.powers(&v).await.act {
        return Err(simple(
            StatusCode::FORBIDDEN,
            "Not from here",
            "<p>The dashboard's control plane token can't act for you. Signed-in people act through a delegate token \
            (<code>fleetlayer-control token add dashboard --scope delegate</code>); the login key through an admin token.</p>",
        ));
    }
    Ok(v)
}

fn auth_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn failed_words(e: &Failed) -> String {
    match e {
        Failed::Refused(m) => format!("The control plane refused: {m}."),
        Failed::Missing(m) => format!("Nothing to do: {m}."),
        Failed::Down(m) => format!("That didn't go through: {m}."),
    }
}

async fn decide(
    State(app): St,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
    Form(f): Form<DecideForm>,
) -> Response {
    let v = match actor(&app, &headers, &f.csrf).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let approve = match f.decision.as_str() {
        "approve" => true,
        "deny" => false,
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    let to = f.back.as_deref();
    // Opening a role window needs the approver's own passkey for that request: only a signed-in person, through a
    // delegate token, approves one here (the control plane checks the step-up); never the admin token. So an approval
    // goes ahead only when a fresh read shows the request, and that it doesn't open a window or this person may.
    if approve {
        let fresh = match &app.source {
            Source::Control(c) => c
                .get("/v1/approvals")
                .await
                .ok()
                .and_then(|v| serde_json::from_value::<Vec<crate::model::Approval>>(v["approvals"].clone()).ok()),
            Source::Demo(d) => Some(d.snapshot().approvals),
        };
        let Some(list) = fresh else {
            return back(
                to,
                "The request can't be read from the control plane right now, so nothing was approved. Try again.",
            );
        };
        let Some(a) = list.iter().find(|a| a.id == id && a.status == "pending") else {
            return back(to, "That request isn't waiting any more.");
        };
        if a.verb == "activate" && !app.powers(&v).await.windows {
            return back(
                to,
                "Opening a role window is approved with your passkey: sign in to the dashboard as yourself, or approve it from your deck.",
            );
        }
    }
    let msg = match &app.source {
        Source::Demo(_) => format!("Demo: the request would be {}.", if approve { "approved" } else { "denied" }),
        Source::Control(c) => match c.decide(&id, approve, app.acting_as(&v)).await {
            Ok(_) => {
                tracing::info!(who = %v.label(), id, approve, "decided a request from the dashboard");
                format!("{} the request.", if approve { "Approved" } else { "Denied" })
            }
            Err(e) => failed_words(&e),
        },
    };
    app.cache.forget().await;
    back(to, &msg)
}

async fn close_window(
    State(app): St,
    headers: HeaderMap,
    UrlPath(role): UrlPath<String>,
    Form(f): Form<CsrfForm>,
) -> Response {
    let v = match actor(&app, &headers, &f.csrf).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let msg = match &app.source {
        Source::Demo(_) => format!("Demo: the {role} window would close."),
        Source::Control(c) => match c.close_window(&role, app.acting_as(&v)).await {
            Ok(r) if r["closed"].as_bool() == Some(false) => format!("The {role} window was already closed."),
            Ok(_) => {
                tracing::info!(who = %v.label(), role, "closed a window from the dashboard");
                format!("Closed the {role} window.")
            }
            Err(e) => failed_words(&e),
        },
    };
    app.cache.forget().await;
    back(f.back.as_deref(), &msg)
}

/// `/_/approvals/<id>/confirm`: where the sign-in service's passkey page sends the person back. The step-up is
/// recorded; this page shows the request once more and approves it with one press, within the five minutes it holds.
async fn confirm(
    State(app): St,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let here = format!("/_/approvals/{}/confirm", form_urlencoded::byte_serialize(id.as_bytes()).collect::<String>());
    // Arriving from the sign-in service is a cross-site navigation, which doesn't carry the SameSite=Strict session
    // cookie: go round once through a page here, so the browser sends it.
    if auth::session_id(&headers).is_none() && !q.contains_key("again") {
        return Html(fleetlayer_oidc::continue_page(&format!("{here}?again=1"), "Fleetlayer", "Continue"))
            .into_response();
    }
    let v = match viewer(&app, &headers, &here).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    app.cache.forget().await;
    let snap = app.cache.get(&app.source).await;
    let Some(a) = snap.approvals.iter().find(|a| a.id == id && a.status == "pending") else {
        return simple(
            StatusCode::NOT_FOUND,
            "Nothing to approve",
            r#"<p>That request isn't waiting any more: it was decided, or it expired.</p><a class="btn btn-block" href="/access">Back to Access</a>"#,
        );
    };
    if !app.powers(&v).await.windows {
        return simple(
            StatusCode::FORBIDDEN,
            "Not from here",
            r#"<p>Opening a role window from the dashboard needs you signed in as yourself, and the dashboard's delegate token.</p><a class="btn btn-block" href="/access">Back to Access</a>"#,
        );
    }
    let what = a.to.strip_prefix("window:").unwrap_or(&a.to);
    simple(
        StatusCode::OK,
        "Open the window?",
        &format!(
            r#"<p><span class="addr">{}</span> asks to open the <span class="addr">{}</span> window.{}</p><p>Your passkey confirmed it. Approve within five minutes.</p><form class="login-form" method="post" action="/_/approvals/{}"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="back" value="/access"><button class="btn btn-primary btn-block" name="decision" value="approve">Open the window</button></form><a class="btn btn-ghost btn-block" href="/access">Not now</a>"#,
            esc(&a.from),
            esc(what),
            a.detail.as_deref().map(|d| format!(" {}", esc(d))).unwrap_or_default(),
            esc(&form_urlencoded::byte_serialize(id.as_bytes()).collect::<String>()),
            esc(&v.csrf),
        ),
    )
}

// ------------------------------------------------------------------ sign-in

/// `/_/approvals/<id>/confirm`, with an ID of plain characters and nothing after it but `?again=1`.
fn is_confirm_path(p: &str) -> bool {
    let p = p.strip_suffix("?again=1").unwrap_or(p);
    p.strip_prefix("/_/approvals/")
        .and_then(|r| r.strip_suffix("/confirm"))
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)))
}

fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") && !n.starts_with("/\\") && !n.starts_with("/_/") => {
            n.to_string()
        }
        // The one app page someone comes back to after signing in: confirming a window after their passkey.
        Some(n) if is_confirm_path(n) => n.to_string(),
        _ => "/".into(),
    }
}

fn login_html(app: &App, msg: &str, next: &str) -> String {
    let enc_next: String = form_urlencoded::byte_serialize(next.as_bytes()).collect();
    let mut body = String::new();
    if app.auth.oidc.is_some() {
        body.push_str(&format!(
            r#"<a class="btn btn-primary btn-block" href="/_/login/start?next={}">Sign in with Fleetlayer</a>"#,
            esc(&enc_next)
        ));
    }
    if app.auth.key.is_some() {
        let (err, invalid) = if msg.is_empty() {
            (String::new(), "")
        } else {
            (
                format!(r#"<p class="field-error" id="login-error" role="alert">{}</p>"#, esc(msg)),
                r#" aria-invalid="true" aria-describedby="login-error""#,
            )
        };
        body.push_str(&format!(
            r#"<form class="login-form" method="post" action="/_/login"><input type="hidden" name="next" value="{}"><label class="label" for="login">Login key</label><input class="input mono" type="password" id="login" name="login" autocomplete="current-password" required spellcheck="false"{invalid}>{err}<button class="btn {} btn-block" type="submit">Sign in{}</button></form>"#,
            esc(next),
            if app.auth.oidc.is_some() { "btn-ghost" } else { "btn-primary" },
            if app.auth.oidc.is_some() { " with the login key" } else { "" },
        ));
    } else if !msg.is_empty() {
        body.push_str(&format!(r#"<p class="field-error" role="alert">{}</p>"#, esc(msg)));
    }
    body
}

#[derive(Deserialize)]
struct NextQuery {
    next: Option<String>,
}

async fn login_page(State(app): St, Query(q): Query<NextQuery>) -> Response {
    let next = safe_next(q.next.as_deref());
    if app.auth.is_open() {
        return Redirect::to(&next).into_response();
    }
    if app.auth.oidc.is_some() && app.auth.key.is_none() {
        let enc: String = form_urlencoded::byte_serialize(next.as_bytes()).collect();
        return Redirect::to(&format!("/_/login/start?next={enc}")).into_response();
    }
    simple(StatusCode::OK, "Fleetlayer", &login_html(&app, "", &next))
}

async fn signed_out(State(app): St) -> Response {
    simple(
        StatusCode::OK,
        "Signed out",
        &format!(r#"<p class="hint">You're signed out.</p>{}"#, login_html(&app, "", "/")),
    )
}

const OIDC_COOKIE: &str = "dashboard_oidc";

fn oidc_cookie(state: &str, secure: bool, max_age: u64) -> String {
    let s = if secure { "; Secure" } else { "" };
    format!("{OIDC_COOKIE}={state}; HttpOnly{s}; SameSite=Lax; Path=/_/auth; Max-Age={max_age}")
}

async fn login_start(State(app): St, Query(q): Query<NextQuery>) -> Response {
    let next = safe_next(q.next.as_deref());
    let Some(oidc) = &app.auth.oidc else { return Redirect::to("/_/login").into_response() };
    match oidc.start(&next).await {
        Ok((url, state)) => (
            StatusCode::SEE_OTHER,
            [(header::SET_COOKIE, oidc_cookie(&state, app.secure_cookies(), 600)), (header::LOCATION, url)],
        )
            .into_response(),
        Err(e) => {
            tracing::warn!("starting a sign-in: {e:#}");
            simple(
                StatusCode::SERVICE_UNAVAILABLE,
                "Try again soon",
                &login_html(&app, "The sign-in service can't be reached right now. Try again in a minute.", &next),
            )
        }
    }
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn auth_callback(State(app): St, headers: HeaderMap, Query(q): Query<CallbackQuery>) -> Response {
    let Some(oidc) = &app.auth.oidc else { return Redirect::to("/_/login").into_response() };
    let secure = app.secure_cookies();
    let fail = |status: StatusCode, msg: &str| {
        let mut r = simple(status, "Sign in", &login_html(&app, msg, "/"));
        r.headers_mut().append(header::SET_COOKIE, oidc_cookie("", secure, 0).parse().expect("a cookie"));
        r
    };
    if let Some(e) = &q.error {
        tracing::info!("the sign-in service sent back {e:?}");
        return fail(
            StatusCode::FORBIDDEN,
            "Signing in didn't finish. Try again, or ask whoever runs this fleet for access.",
        );
    }
    let (Some(code), Some(state)) = (&q.code, &q.state) else {
        return fail(StatusCode::BAD_REQUEST, "That sign-in link isn't complete. Sign in again.");
    };
    let bound = auth::cookie_value(&headers, OIDC_COOKIE);
    match oidc.finish(code, state, bound.as_deref()).await {
        Ok((person, next)) => {
            if !app.tenant_ok(&person.tenant).await {
                tracing::warn!(who = %person.sub, tenant = %person.tenant, "a sign-in to another tenant was turned away");
                return fail(
                    StatusCode::FORBIDDEN,
                    "You're signed in to another place than the one this dashboard shows.",
                );
            }
            tracing::info!(who = %person.sub, handle = ?person.handle, "signed in to the dashboard");
            let sid = app.auth.start(Some(person)).await;
            // A page, not a redirect: the SameSite=Strict cookie isn't sent on the issuer's cross-site redirect.
            let body = fleetlayer_oidc::continue_page(&next, "Signed in · Fleetlayer", "Continue");
            let mut res = Html(body).into_response();
            let h = res.headers_mut();
            h.append(header::SET_COOKIE, auth::cookie(&sid, secure).parse().expect("a cookie"));
            h.append(header::SET_COOKIE, oidc_cookie("", secure, 0).parse().expect("a cookie"));
            res
        }
        Err(e) => {
            tracing::warn!("a sign-in failed: {e:#}");
            fail(StatusCode::FORBIDDEN, "Signing in didn't work. Sign in again.")
        }
    }
}

#[derive(Deserialize)]
struct LoginForm {
    login: String,
    next: Option<String>,
}

async fn login(
    State(app): St,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(f): Form<LoginForm>,
) -> Response {
    let next = safe_next(f.next.as_deref());
    let Some(key) = &app.auth.key else { return Redirect::to("/_/login").into_response() };
    let ip = auth::client_ip(peer.ip(), &headers, &app.trusted_proxies);
    if key.locked_out(ip).await {
        return simple(
            StatusCode::TOO_MANY_REQUESTS,
            "Fleetlayer",
            &login_html(&app, "Too many tries: wait 15 minutes.", &next),
        );
    }
    if !key.check(&f.login) {
        key.failed(ip).await;
        tracing::warn!(%ip, "a failed dashboard login");
        return simple(
            StatusCode::UNAUTHORIZED,
            "Fleetlayer",
            &login_html(&app, "That isn't the dashboard's login key.", &next),
        );
    }
    let sid = app.auth.start(None).await;
    ([(header::SET_COOKIE, auth::cookie(&sid, app.secure_cookies()))], Redirect::to(&next)).into_response()
}

#[derive(Deserialize)]
struct BackchannelForm {
    logout_token: Option<String>,
}

async fn backchannel_logout(State(app): St, Form(f): Form<BackchannelForm>) -> Response {
    let no_store = [(header::CACHE_CONTROL, "no-store")];
    let Some(oidc) = &app.auth.oidc else { return (StatusCode::NOT_FOUND, no_store).into_response() };
    let Some(token) = f.logout_token else { return (StatusCode::BAD_REQUEST, no_store).into_response() };
    match oidc.verify_logout(&token).await {
        Ok(out) => {
            let ended = app.auth.end_signed_out(&out).await;
            tracing::info!(ended, "signed out by the sign-in service");
            (StatusCode::OK, no_store).into_response()
        }
        Err(e) => {
            tracing::warn!("a back-channel logout was refused: {e:#}");
            (StatusCode::BAD_REQUEST, no_store).into_response()
        }
    }
}

async fn logout(State(app): St, headers: HeaderMap, Form(f): Form<CsrfForm>) -> Response {
    if let Some(v) = app.auth.viewer(&headers).await
        && !auth_eq(&v.csrf, &f.csrf)
    {
        return simple(StatusCode::FORBIDDEN, "Try again", "<p>That form had expired. Reload and try again.</p>");
    }
    let ended = app.auth.end(&headers).await;
    let clear = [(header::SET_COOKIE, auth::clear_cookie(app.secure_cookies()))];
    if ended.is_some_and(|v| v.person.is_some())
        && let Some(oidc) = &app.auth.oidc
        && let Some(url) = oidc.logout_url().await
    {
        let body = fleetlayer_oidc::continue_page(&url, "Signing out · Fleetlayer", "Continue signing out");
        return (clear, Html(body)).into_response();
    }
    (StatusCode::SEE_OTHER, clear, [(header::LOCATION, "/_/signed-out")]).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn demo() -> Arc<App> {
        Arc::new(App::new(
            Source::Demo(crate::demo::Demo::new()),
            Auth::open(),
            Some("http://127.0.0.1:7690".into()),
            vec![],
        ))
    }

    fn keyed() -> Arc<App> {
        let key = auth::LoginKey::new("k".repeat(40)).unwrap();
        let auth = Auth::new(Some(key), None, None, "dashboard".into()).unwrap();
        Arc::new(App::new(Source::Demo(crate::demo::Demo::new()), auth, Some("https://d.example".into()), vec![]))
    }

    async fn get(app: &Arc<App>, path: &str, cookie: Option<&str>) -> (StatusCode, HeaderMap, String) {
        let mut req = Request::get(path);
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, c);
        }
        let r = router(app.clone()).oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let (status, headers) = (r.status(), r.headers().clone());
        let b = axum::body::to_bytes(r.into_body(), 1 << 22).await.unwrap();
        (status, headers, String::from_utf8_lossy(&b).into())
    }

    #[tokio::test]
    async fn the_demo_serves_every_page_and_its_fragments() {
        let app = demo();
        for (_, path, title) in NAV {
            let (status, h, body) = get(&app, path, None).await;
            assert_eq!(status, StatusCode::OK, "{path}");
            assert!(body.contains(&format!("<title>{title} · Fleetlayer</title>")), "{path}");
            assert!(h["content-security-policy"].to_str().unwrap().contains("script-src 'self'"));
            let (status, _, frag) = get(&app, &format!("{path}?fragment=1"), None).await;
            assert_eq!(status, StatusCode::OK);
            assert!(!frag.contains("<html"), "{path}: a fragment is only the main");
        }
        let (status, _, body) = get(&app, "/healthz", None).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "ok"));
        let (_, _, css) = get(&app, "/_/assets/dashboard.css", None).await;
        assert!(css.contains("--color-accent"));
    }

    #[tokio::test]
    async fn without_a_session_pages_send_you_to_sign_in() {
        let app = keyed();
        let (status, h, _) = get(&app, "/sessions?node=build", None).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(h[header::LOCATION], "/_/login?next=%2Fsessions%3Fnode%3Dbuild");
        let (status, _, _) = get(&app, "/?fragment=1", None).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let (status, _, body) = get(&app, "/_/login", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Login key"));
        let sid = app.auth.start(None).await;
        let (status, _, body) = get(&app, "/", Some(&format!("{}={sid}", auth::COOKIE))).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Sign out"));
    }

    #[tokio::test]
    async fn an_action_needs_the_form_s_token() {
        let app = keyed();
        let sid = app.auth.start(None).await;
        let cookie = format!("{}={sid}", auth::COOKIE);
        let post = |body: &'static str| {
            Request::post("/_/approvals/ap_7d21")
                .header(header::COOKIE, cookie.clone())
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::REFERER, "https://d.example/access")
                .body(Body::from(body))
                .unwrap()
        };
        let r = router(app.clone()).oneshot(post("csrf=wrong&decision=approve")).await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let csrf = app
            .auth
            .viewer(&{
                let mut h = HeaderMap::new();
                h.insert(header::COOKIE, cookie.parse().unwrap());
                h
            })
            .await
            .unwrap()
            .csrf;
        let body: &'static str = Box::leak(format!("csrf={csrf}&decision=approve").into_boxed_str());
        let r = router(app.clone()).oneshot(post(body)).await.unwrap();
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert!(r.headers()[header::LOCATION].to_str().unwrap().starts_with("/access?msg=Demo"));
    }

    /// What reached the fake control plane: each decision or close, and who it was for.
    type Seen = Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;

    /// A control plane that answers like the real one for a delegate token, and remembers who each decision was for.
    async fn fake_control(scope: &'static str) -> (String, Seen) {
        use axum::routing::{delete, get, post};
        let seen: Seen = Arc::default();
        let s1 = seen.clone();
        let s2 = seen.clone();
        let person = |h: &HeaderMap| h.get("fleetlayer-person").and_then(|v| v.to_str().ok()).map(str::to_string);
        let router = Router::new()
            .route("/v1/token", get(move || async move { axum::Json(serde_json::json!({"scope": scope, "name": "dashboard", "tenant": "default", "tenantSlug": "home"})) }))
            .route("/v1/fleet", get(|| async { axum::Json(serde_json::json!({"nodes": [], "sessions": []})) }))
            .route("/v1/events", get(|| async { axum::Json(serde_json::json!({"events": []})) }))
            .route("/v1/readings", get(|| async { axum::Json(serde_json::json!({"readings": []})) }))
            .route("/v1/tools", get(|| async { axum::Json(serde_json::json!({"tools": []})) }))
            .route("/v1/windows", get(|| async { axum::Json(serde_json::json!({"roles": [], "windows": []})) }))
            .route(
                "/v1/approvals",
                get(|| async {
                    axum::Json(serde_json::json!({"approvals": [
                        {"id": "ap_tool", "from": "a@n", "verb": "use_tool", "to": "tool:b@n", "status": "pending", "createdAt": 1},
                        {"id": "ap_win", "from": "a@n", "verb": "activate", "to": "window:ops", "status": "pending", "createdAt": 1}]}))
                }),
            )
            .route(
                "/v1/approvals/{id}",
                post(move |UrlPath(id): UrlPath<String>, h: HeaderMap| async move {
                    s1.lock().unwrap().push((id.clone(), person(&h)));
                    axum::Json(serde_json::json!({"id": id, "approved": true}))
                }),
            )
            .route(
                "/v1/windows/{role}",
                delete(move |UrlPath(role): UrlPath<String>, h: HeaderMap| async move {
                    s2.lock().unwrap().push((format!("close {role}"), person(&h)));
                    axum::Json(serde_json::json!({"role": role, "closed": true}))
                }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
        (url, seen)
    }

    async fn signed_in_person(
        scope: &'static str,
    ) -> (Arc<App>, String, String, Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>) {
        let (url, seen) = fake_control(scope).await;
        let f = fleetlayer_oidc::testing::FakeIssuer::start().await;
        let oidc = fleetlayer_oidc::Oidc::new(&f.url, "dashboard", "s", "https://d.example").unwrap();
        let checker = auth::Checker::Fixed(Box::new(|_: &str, _: &str| Ok(true)));
        let auth = Auth::new(None, Some(oidc), Some(checker), "dashboard".into()).unwrap();
        let control = crate::control::Control::new(&url, "t").unwrap();
        let mut app = App::new(Source::Control(control), auth, Some("https://d.example".into()), vec![]);
        app.issuer = Some("https://signin.example".into());
        let app = Arc::new(app);
        let person = fleetlayer_oidc::Person {
            sub: "p_sam".into(),
            tenant: "home".into(),
            handle: Some("sam".into()),
            name: None,
            sid: None,
        };
        let sid = app.auth.start(Some(person)).await;
        let cookie = format!("{}={sid}", auth::COOKIE);
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, cookie.parse().unwrap());
        let csrf = app.auth.viewer(&h).await.unwrap().csrf;
        (app, cookie, csrf, seen)
    }

    async fn post_form(app: &Arc<App>, path: &str, cookie: &str, body: String) -> Response {
        let req = Request::post(path)
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::REFERER, "https://d.example/access")
            .body(Body::from(body))
            .unwrap();
        router(app.clone()).oneshot(req).await.unwrap()
    }

    #[tokio::test]
    async fn a_signed_in_person_decides_as_themselves_and_opens_windows_only_through_their_passkey() {
        let (app, cookie, csrf, seen) = signed_in_person("delegate").await;
        // An ordinary request, and closing a window: sent as the person.
        let r = post_form(&app, "/_/approvals/ap_tool", &cookie, format!("csrf={csrf}&decision=approve")).await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        let r = post_form(&app, "/_/windows/ops/close", &cookie, format!("csrf={csrf}")).await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            *seen.lock().unwrap(),
            [("ap_tool".to_string(), Some("p_sam".to_string())), ("close ops".to_string(), Some("p_sam".to_string()))]
        );
        // A window request: Access offers the passkey page, which comes back to the confirm page here.
        let (_, _, page) = get(&app, "/access", Some(&cookie)).await;
        let want = "https://signin.example/stepup?tenant=home&amp;approval=ap_win&amp;return=https%3A%2F%2Fd.example%2F_%2Fapprovals%2Fap_win%2Fconfirm";
        assert!(page.contains(want), "{page}");
        let (status, _, page) = get(&app, "/_/approvals/ap_win/confirm", Some(&cookie)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(page.contains("Open the window") && page.contains(r#"action="/_/approvals/ap_win""#), "{page}");
        let r = post_form(&app, "/_/approvals/ap_win", &cookie, format!("csrf={csrf}&decision=approve")).await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert_eq!(seen.lock().unwrap().last().cloned(), Some(("ap_win".to_string(), Some("p_sam".to_string()))));
    }

    #[tokio::test]
    async fn with_an_admin_token_a_signed_in_person_does_nothing_and_no_window_is_approved() {
        let (app, cookie, csrf, seen) = signed_in_person("admin").await;
        let r = post_form(&app, "/_/approvals/ap_tool", &cookie, format!("csrf={csrf}&decision=approve")).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "the admin token would record 'admin token', not the person");
        let r = post_form(&app, "/_/approvals/ap_win", &cookie, format!("csrf={csrf}&decision=approve")).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let (_, _, page) = get(&app, "/access", Some(&cookie)).await;
        assert!(!page.contains("stepup") && !page.contains(r#"value="approve""#), "{page}");
        assert!(seen.lock().unwrap().is_empty(), "nothing reached the control plane");
    }

    /// The login key's holder, with a dashboard whose token is `scope`.
    async fn keyed_control(scope: &'static str, url: Option<String>) -> (Arc<App>, String, String, Seen) {
        let (fake, seen) = fake_control(scope).await;
        let key = auth::LoginKey::new("k".repeat(40)).unwrap();
        let auth = Auth::new(Some(key), None, None, "dashboard".into()).unwrap();
        let control = crate::control::Control::new(&url.unwrap_or(fake), "t").unwrap();
        let app = Arc::new(App::new(Source::Control(control), auth, Some("https://d.example".into()), vec![]));
        let sid = app.auth.start(None).await;
        let cookie = format!("{}={sid}", auth::COOKIE);
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, cookie.parse().unwrap());
        let csrf = app.auth.viewer(&h).await.unwrap().csrf;
        (app, cookie, csrf, seen)
    }

    #[tokio::test]
    async fn the_login_key_never_opens_a_window_and_an_unreadable_request_is_never_approved() {
        let (app, cookie, csrf, seen) = keyed_control("admin", None).await;
        // An empty cache can't let a window through: the request is read afresh.
        app.cache.forget().await;
        let r = post_form(&app, "/_/approvals/ap_win", &cookie, format!("csrf={csrf}&decision=approve&back=/")).await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert!(
            r.headers()[header::LOCATION].to_str().unwrap().starts_with("/?msg=Opening"),
            "back to the form's page"
        );
        assert!(seen.lock().unwrap().is_empty(), "nothing reached the control plane");
        // An ordinary request goes ahead with the admin token; denying a window does too.
        let r = post_form(&app, "/_/approvals/ap_tool", &cookie, format!("csrf={csrf}&decision=approve")).await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        let r = post_form(&app, "/_/approvals/ap_win", &cookie, format!("csrf={csrf}&decision=deny")).await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert_eq!(seen.lock().unwrap().len(), 2);

        // A control plane that can't be read: the dashboard can't tell what its token is, so it acts for nobody.
        let (app, cookie, csrf, _) = keyed_control("admin", Some("http://127.0.0.1:9".into())).await;
        let r = post_form(&app, "/_/approvals/ap_tool", &cookie, format!("csrf={csrf}&decision=approve")).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn someone_signed_in_to_another_tenant_sees_nothing() {
        let (app, _, _, _) = signed_in_person("delegate").await;
        let elsewhere =
            fleetlayer_oidc::Person { sub: "p_x".into(), tenant: "other".into(), handle: None, name: None, sid: None };
        let sid = app.auth.start(Some(elsewhere)).await;
        let (status, _, body) = get(&app, "/", Some(&format!("{}={sid}", auth::COOKIE))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains("Not this dashboard"), "{body}");
        assert!(app.tenant_ok("home").await && app.tenant_ok("default").await && !app.tenant_ok("").await);
    }

    #[tokio::test]
    async fn coming_back_from_the_passkey_page_reaches_the_confirm_page() {
        let (app, cookie, _, _) = signed_in_person("delegate").await;
        // Cross-site, so no cookie: one round through a page here, which the browser then visits with it.
        let (status, _, body) = get(&app, "/_/approvals/ap_win/confirm", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("/_/approvals/ap_win/confirm?again=1"), "{body}");
        let (status, _, body) = get(&app, "/_/approvals/ap_win/confirm?again=1", Some(&cookie)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Open the window"), "{body}");
        // Signed out by then: to sign-in, and back to the confirm page after.
        let (status, h, _) = get(&app, "/_/approvals/ap_win/confirm?again=1", None).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(h[header::LOCATION], "/_/login?next=%2F_%2Fapprovals%2Fap_win%2Fconfirm");
    }

    #[test]
    fn next_stays_on_this_site() {
        assert_eq!(safe_next(Some("/events")), "/events");
        assert_eq!(safe_next(Some("//evil.example")), "/");
        assert_eq!(safe_next(Some("https://evil.example")), "/");
        assert_eq!(safe_next(Some("/_/logout")), "/");
        assert_eq!(safe_next(Some("/_/approvals/ap_1/confirm")), "/_/approvals/ap_1/confirm");
        assert_eq!(safe_next(Some("/_/approvals/ap_1/confirm?again=1")), "/_/approvals/ap_1/confirm?again=1");
        assert_eq!(safe_next(Some("/_/approvals/../logout/confirm")), "/");
    }
}
