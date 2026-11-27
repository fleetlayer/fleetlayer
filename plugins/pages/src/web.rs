//! The web app: the spaces you may read, each space's pages newest first, and the page itself, live.
//!
//! Paths: `/` (spaces), `/<space>/` (its pages), `/<space>/<folder>/` (a page), `/<space>/<folder>/<file>` (its files,
//! served sandboxed). The app's own routes live under `/_/`, which no space or page can be named.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{ConnectInfo, Path as UrlPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use futures::Stream;
use serde::Deserialize;

use crate::apps;
use crate::auth::{self, Access, Auth, Viewer};
use crate::render::{self, Kind, Rendered};
use crate::space::{self, Cache, SpaceConfig, Summary};

pub struct App {
    pub spaces: BTreeMap<String, SpaceConfig>,
    pub auth: Auth,
    pub trusted_proxies: Vec<IpAddr>,
    pub login_hint: Option<String>,
    pub cache: Cache,
    /// The pages site's own address (`https://pages.example`): apps send people here to sign in.
    pub base_url: Option<String>,
    /// The domain apps live under: app `recipes` of space `personal` is `recipes--personal.<apps_domain>`.
    pub apps_domain: Option<String>,
    pub broker: apps::Broker,
}

type Ctx = State<Arc<App>>;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/_/healthz", get(healthz))
        .route("/_/login", get(login_page).post(login))
        .route("/_/login/start", get(login_start))
        .route("/_/auth/callback", get(auth_callback))
        .route("/_/signed-out", get(signed_out))
        .route("/_/logout", post(logout))
        .route("/_/assets/{name}", get(asset))
        .route("/_/events", get(events))
        .route("/_/apps/authorize", get(authorize).post(authorize_confirmed))
        .route("/_/apps/{space}/{name}/icon", get(app_icon))
        .route("/", get(spaces_page))
        .route("/{space}", get(add_slash))
        .route("/{space}/", get(space_page))
        .route("/{space}/{folder}", get(add_slash2))
        .route("/{space}/{folder}/", get(page_view))
        .route("/{space}/{folder}/{*file}", get(file))
        .fallback(not_found_any)
        .layer(axum::middleware::map_response(security_headers))
        .layer(axum::middleware::from_fn_with_state(app.clone(), by_host))
        .with_state(app)
}

/// Requests for an app's host go to the app; everything else is the pages site.
async fn by_host(State(app): Ctx, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    if let Some(domain) = &app.apps_domain {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .map(str::to_string)
            .or_else(|| req.uri().host().map(str::to_string));
        if let Some(host) = host
            && let Some((space, name)) = apps::parse_host(&host, domain)
        {
            let host = apps::host_for(&space, &name, domain);
            return apps::handle(app.clone(), host, space, name, req).await;
        }
    }
    next.run(req).await
}

pub async fn serve(app: Arc<App>, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
    axum::serve(listener, router(app).into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

impl App {
    /// The host pattern for apps, relative to the gateway's domain (the pages site's domain without its first label):
    /// `apps_domain: apps.localhost` with `base_url: http://pages.localhost:7680` gives `*--*.apps`. None without apps,
    /// or when the apps domain isn't under the pages site's domain.
    pub fn apps_host_pattern(&self) -> Option<String> {
        let apps = self.apps_domain.as_deref()?;
        let base = self.base_url.as_deref()?.split_once("://")?.1;
        let host = base.split(['/', ':']).next()?;
        let domain = host.split_once('.')?.1;
        if apps == domain {
            return Some("*--*".into());
        }
        let rel = apps.strip_suffix(domain)?.strip_suffix('.')?;
        Some(format!("*--*.{rel}"))
    }
}

// ------------------------------------------------------------ headers

/// The app's pages: scripts and styles only from the service, frames only of its own files, nothing cached. A page's
/// own files get their own, stricter policy (see `file`), which this leaves alone.
async fn security_headers(mut res: Response) -> Response {
    let h = res.headers_mut();
    let set = |h: &mut HeaderMap, k: &'static str, v: &'static str| {
        h.insert(header::HeaderName::from_static(k), HeaderValue::from_static(v));
    };
    if !h.contains_key(header::CONTENT_SECURITY_POLICY) {
        set(
            h,
            "content-security-policy",
            "default-src 'none'; script-src 'self'; style-src 'self'; style-src-attr 'unsafe-inline'; img-src 'self'; \
            connect-src 'self'; frame-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
        );
        set(h, "x-frame-options", "DENY");
    }
    set(h, "referrer-policy", "no-referrer");
    set(h, "x-content-type-options", "nosniff");
    if !h.contains_key(header::CACHE_CONTROL) {
        set(h, "cache-control", "no-store");
    }
    res
}

/// For a page's own files: no scripts at all, even in an SVG or HTML file opened on its own; framed only by the app.
/// `allow-same-origin` keeps the sign-in cookie on the mock-up's own CSS and images; with no `allow-scripts`, nothing
/// can use that origin.
const FILE_CSP: &str = "sandbox allow-same-origin; default-src 'self'; script-src 'none'; \
    style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; media-src 'self'; \
    form-action 'none'; frame-ancestors 'self'; base-uri 'none'";

// ------------------------------------------------------------ assets

const ASSETS: &[(&str, &str, &str)] = &[
    ("pages.css", "text/css; charset=utf-8", include_str!("../web/pages.css")),
    ("pages.js", "text/javascript; charset=utf-8", include_str!("../web/pages.js")),
    ("icon.svg", "image/svg+xml", include_str!("../web/icon.svg")),
];

async fn asset(UrlPath(name): UrlPath<String>) -> Response {
    match ASSETS.iter().find(|(n, _, _)| *n == name) {
        Some((_, kind, body)) => {
            ([(header::CONTENT_TYPE, *kind), (header::CACHE_CONTROL, "no-cache")], *body).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ------------------------------------------------------------ health

async fn healthz(State(app): Ctx) -> Response {
    let problems = app.problems();
    if problems.is_empty() {
        (StatusCode::OK, "ok").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, problems.join("\n")).into_response()
    }
}

impl App {
    /// Cookies are Secure unless the site is served over plain HTTP (only allowed on loopback; see `main.rs`).
    pub fn secure_cookies(&self) -> bool {
        !self.base_url.as_deref().is_some_and(|b| b.starts_with("http://"))
    }

    /// What's wrong with the spaces' folders, for `/healthz` and the heartbeat.
    pub fn problems(&self) -> Vec<String> {
        self.spaces.iter().filter_map(|(n, c)| space::check(n, c).err()).collect()
    }
}

// ------------------------------------------------------------ sign-in

const LOGIN_HTML: &str = include_str!("../web/login.html");

pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            _ => o.push(c),
        }
    }
    o
}

/// Where to go after signing in: only a path on this site.
fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") && !n.starts_with("/\\") => n.to_string(),
        _ => "/".into(),
    }
}

fn login_html(app: &App, msg: &str, next: &str) -> Html<String> {
    let enc_next: String = form_urlencoded::byte_serialize(next.as_bytes()).collect();
    let sso = if app.auth.oidc.is_some() {
        format!(
            r#"<a class="btn btn-primary btn-block" href="/_/login/start?next={}">Sign in with Fleetlayer</a>"#,
            esc(&enc_next)
        )
    } else {
        String::new()
    };
    let hint = app.login_hint.as_deref().map_or(String::new(), |h| format!(r#"<p class="login-hint">{}</p>"#, esc(h)));
    let (msg, invalid) = if msg.is_empty() {
        (String::new(), "")
    } else {
        (
            format!(r#"<p class="field-error" id="login-error" role="alert">{}</p>"#, esc(msg)),
            r#" aria-invalid="true" aria-describedby="login-error""#,
        )
    };
    let mut page = LOGIN_HTML.to_string();
    if app.auth.key.is_none() {
        // Only Fleetlayer's sign-in: no key form.
        if let (Some(a), Some(b)) = (page.find("<!--key-->"), page.find("<!--/key-->")) {
            page.replace_range(a..b + "<!--/key-->".len(), "");
        }
    } else if app.auth.oidc.is_some() {
        page = page
            .replace(">Sign in</button>", ">Sign in with the login key</button>")
            .replace("btn btn-primary btn-block\" type=\"submit\"", "btn btn-ghost btn-block\" type=\"submit\"");
    }
    Html(
        page.replace("{sso}", &sso)
            .replace("{hint}", &hint)
            .replace("{msg}", &msg)
            .replace("{invalid}", invalid)
            .replace("{next}", &esc(next)),
    )
}

#[derive(Deserialize)]
struct NextQuery {
    next: Option<String>,
}

async fn login_page(State(app): Ctx, Query(q): Query<NextQuery>) -> Response {
    let next = safe_next(q.next.as_deref());
    if app.auth.oidc.is_some() && app.auth.key.is_none() {
        // Fleetlayer's sign-in only: straight there.
        let enc: String = form_urlencoded::byte_serialize(next.as_bytes()).collect();
        return Redirect::to(&format!("/_/login/start?next={enc}")).into_response();
    }
    login_html(&app, "", &next).into_response()
}

async fn signed_out(State(app): Ctx) -> Response {
    let Html(page) = login_html(&app, "", "/");
    Html(page.replacen("</h1></div>", r#"</h1></div><p class="login-hint">You're signed out.</p>"#, 1)).into_response()
}

/// The cookie that ties a sign-in to the browser that started it. Lax, since the issuer sends the browser back with
/// a cross-site navigation; scoped to the callback.
const OIDC_COOKIE: &str = "pages_oidc";

fn oidc_cookie(state: &str, secure: bool, max_age: u64) -> String {
    let s = if secure { "; Secure" } else { "" };
    format!("{OIDC_COOKIE}={state}; HttpOnly{s}; SameSite=Lax; Path=/_/auth; Max-Age={max_age}")
}

async fn login_start(State(app): Ctx, Query(q): Query<NextQuery>) -> Response {
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
            (
                StatusCode::SERVICE_UNAVAILABLE,
                login_html(&app, "The sign-in service can't be reached right now. Try again in a minute.", &next),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let c = headers.get(header::COOKIE)?.to_str().ok()?;
    c.split(';').find_map(|p| p.trim().strip_prefix(&format!("{name}=")).map(str::to_string))
}

async fn auth_callback(State(app): Ctx, headers: HeaderMap, Query(q): Query<CallbackQuery>) -> Response {
    let Some(oidc) = &app.auth.oidc else { return Redirect::to("/_/login").into_response() };
    let secure = app.secure_cookies();
    let fail = |status: StatusCode, msg: &str| {
        ([(header::SET_COOKIE, oidc_cookie("", secure, 0))], (status, login_html(&app, msg, "/"))).into_response()
    };
    if let Some(e) = &q.error {
        tracing::info!("the sign-in service sent back {e:?}");
        return fail(
            StatusCode::FORBIDDEN,
            "Signing in didn't finish. Try again, or ask whoever runs this service for access.",
        );
    }
    let (Some(code), Some(state)) = (&q.code, &q.state) else {
        return fail(StatusCode::BAD_REQUEST, "That sign-in link isn't complete. Sign in again.");
    };
    let bound = cookie_value(&headers, OIDC_COOKIE);
    match oidc.finish(code, state, bound.as_deref()).await {
        Ok((person, next)) => {
            tracing::info!(who = %person.sub, handle = ?person.handle, "signed in to pages");
            let sid = app.auth.start(Viewer { person: Some(person) }).await;
            // A page, not a redirect: the session cookie is SameSite=Strict, and a redirect still counts as the
            // issuer's cross-site navigation, so the browser wouldn't send the cookie with it.
            let body = fleetlayer_oidc::continue_page(&next, "Signed in · Pages", "Continue");
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
    #[serde(default)]
    next: Option<String>,
}

async fn login(
    State(app): Ctx,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(f): Form<LoginForm>,
) -> Response {
    let next = safe_next(f.next.as_deref());
    let Some(key) = &app.auth.key else { return Redirect::to("/_/login").into_response() };
    let ip = auth::client_ip(peer.ip(), &headers, &app.trusted_proxies);
    if key.locked_out(ip).await {
        return (StatusCode::TOO_MANY_REQUESTS, login_html(&app, "Too many tries: wait 15 minutes.", &next))
            .into_response();
    }
    if !key.check(&f.login) {
        key.failed(ip).await;
        tracing::warn!(%ip, "a failed pages login");
        return (StatusCode::UNAUTHORIZED, login_html(&app, "That isn't the login key for pages.", &next))
            .into_response();
    }
    let sid = app.auth.start(Viewer { person: None }).await;
    ([(header::SET_COOKIE, auth::cookie(&sid, app.secure_cookies()))], Redirect::to(&next)).into_response()
}

async fn logout(State(app): Ctx, headers: HeaderMap) -> Response {
    let ended = app.auth.end(&headers).await;
    let clear = [(header::SET_COOKIE, auth::clear_cookie(app.secure_cookies()))];
    // Someone who signed in through Fleetlayer signs out there too. A page that moves on, not a redirect: the form's
    // `form-action 'self'` would stop a redirect to the sign-in service.
    if ended.is_some_and(|v| v.person.is_some())
        && let Some(oidc) = &app.auth.oidc
        && let Some(url) = oidc.logout_url().await
    {
        let body = fleetlayer_oidc::continue_page(&url, "Signing out · Pages", "Continue signing out");
        return (clear, Html(body)).into_response();
    }
    (StatusCode::SEE_OTHER, clear, [(header::LOCATION, "/_/signed-out")]).into_response()
}

/// The viewer, or the response that sends them to sign in.
#[allow(clippy::result_large_err)] // the response is built once per request either way
async fn viewer(app: &App, headers: &HeaderMap, here: &str) -> Result<Viewer, Response> {
    match app.auth.viewer(headers).await {
        Some(v) => Ok(v),
        None => {
            let next: String = form_urlencoded::byte_serialize(here.as_bytes()).collect();
            Err(Redirect::to(&format!("/_/login?next={next}")).into_response())
        }
    }
}

/// The space's config, when it exists and the viewer may read `target` in it (`page:<space>/*`,
/// `page:<space>/<folder>`, `app:<space>/<name>`). Something they may not read is "not found"; when access can't be
/// checked, a page that says so (never the content).
#[allow(clippy::result_large_err)] // the response is built once per request either way
async fn readable<'a>(
    app: &'a App,
    v: &Viewer,
    name: &str,
    target: &str,
    missing: &str,
) -> Result<&'a SpaceConfig, Response> {
    let Some(cfg) = app.spaces.get(name) else { return Err(not_found(missing)) };
    match app.auth.may(v, target).await {
        Access::Yes => Ok(cfg),
        Access::No => Err(not_found(missing)),
        Access::CantTell(_) => Err(cant_check()),
    }
}

/// When the access check can't be made: fail closed, and say why.
pub(crate) fn cant_check() -> Response {
    html_page(
        StatusCode::SERVICE_UNAVAILABLE,
        layout(
            "Can't check your access · Pages",
            &[],
            r#"<main class="wrap empty-page"><div class="empty"><h1>Can't check your access right now</h1><p>Fleetlayer didn't answer when asked who may read this, so it isn't shown. Try again in a minute.</p></div></main>"#,
            None,
            true,
        ),
    )
}

// ------------------------------------------------------------ layout

struct Crumb {
    label: String,
    href: Option<String>,
}

fn layout(title: &str, crumbs: &[Crumb], body: &str, live: Option<&str>, signed_in: bool) -> String {
    layout_with(title, crumbs, body, live, signed_in, false)
}

/// `wide`: the whole page, app bar included, uses the window's width (a mock-up).
fn layout_with(title: &str, crumbs: &[Crumb], body: &str, live: Option<&str>, signed_in: bool, wide: bool) -> String {
    let mut nav = String::from(
        r#"<a class="brand" href="/"><img src="/_/assets/icon.svg" alt="" width="24" height="24"><span>Pages</span></a>"#,
    );
    for c in crumbs {
        nav.push_str(r#"<span class="crumb-sep" aria-hidden="true">/</span>"#);
        match &c.href {
            Some(h) => nav.push_str(&format!(r#"<a class="crumb" href="{}">{}</a>"#, esc(h), esc(&c.label))),
            None => {
                nav.push_str(&format!(r#"<span class="crumb crumb-here" aria-current="page">{}</span>"#, esc(&c.label)))
            }
        }
    }
    let mut live_attr = live.map_or(String::new(), |p| format!(r#" data-live="{}""#, esc(p)));
    if wide {
        live_attr.push_str(r#" class="wide""#);
    }
    let live_badge = if live.is_some() {
        r#"<span class="live" id="live" data-state="connecting" title="Updates as the files change"><span class="live-dot"></span><span class="live-label">Live</span></span>"#
    } else {
        ""
    };
    let signout = if signed_in {
        r#"<form method="post" action="/_/logout"><button class="btn btn-ghost btn-sm" type="submit">Sign out</button></form>"#
    } else {
        ""
    };
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="color-scheme" content="light dark">
<meta name="robots" content="noindex">
<title>{title}</title>
<link rel="icon" href="/_/assets/icon.svg" type="image/svg+xml">
<link rel="stylesheet" href="/_/assets/pages.css">
<script src="/_/assets/pages.js" defer></script>
</head>
<body{live_attr}>
<header class="appbar"><div class="wrap appbar-row"><nav class="crumbs" aria-label="Where you are">{nav}</nav><div class="appbar-end">{live_badge}{signout}</div></div></header>
{body}
</body>
</html>
"#,
        title = esc(title),
    )
}

fn html_page(status: StatusCode, s: String) -> Response {
    (status, Html(s)).into_response()
}

fn not_found_body(what: &str) -> String {
    format!(
        r#"<main class="wrap empty-page"><div class="empty"><h1>Not here</h1><p>{}</p><a class="btn" href="/">All pages</a></div></main>"#,
        esc(what)
    )
}

async fn not_found_any(State(app): Ctx, headers: HeaderMap) -> Response {
    let signed_in = app.auth.viewer(&headers).await.is_some();
    html_page(
        StatusCode::NOT_FOUND,
        layout("Not found · Pages", &[], &not_found_body("There's nothing at this address."), None, signed_in),
    )
}

fn not_found(what: &str) -> Response {
    html_page(StatusCode::NOT_FOUND, layout("Not found · Pages", &[], &not_found_body(what), None, true))
}

// ------------------------------------------------------------ spaces

async fn spaces_page(State(app): Ctx, headers: HeaderMap) -> Response {
    let v = match viewer(&app, &headers, "/").await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let mut rows = vec![];
    for (name, cfg) in &app.spaces {
        match app.auth.may(&v, &format!("page:{name}/*")).await {
            Access::Yes => {}
            Access::No => continue,
            Access::CantTell(_) => return cant_check(),
        }
        rows.push((name.clone(), cfg.clone(), app.cache.list(cfg)));
    }
    if rows.len() == 1 {
        return Redirect::to(&format!("/{}/", rows[0].0)).into_response();
    }
    let mut body = String::from(r#"<main class="wrap"><header class="page-head"><h1>Spaces</h1></header>"#);
    if rows.is_empty() {
        body.push_str(r#"<div class="empty"><h2>Nothing shared with you yet</h2><p>Ask whoever runs this service to share some pages with you.</p></div>"#);
    } else {
        body.push_str(r#"<ul class="spaces">"#);
        for (name, cfg, pages) in rows {
            let title = cfg.title.clone().unwrap_or_else(|| name.clone());
            let latest =
                pages.first().map_or("No pages yet".to_string(), |p| format!("Latest {}", pretty_date(&p.date)));
            body.push_str(&format!(
                r#"<li><a class="space-row" href="/{n}/"><span class="space-title">{t}</span><span class="space-meta">{c} · {l}</span></a></li>"#,
                n = esc(&name),
                t = esc(&title),
                c = count(pages.len(), "page"),
                l = esc(&latest),
            ));
        }
        body.push_str("</ul>");
    }
    body.push_str("</main>");
    html_page(StatusCode::OK, layout("Pages", &[], &body, None, true))
}

fn count(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// `2026-11-27` → `27 Nov 2026`.
fn pretty_date(d: &str) -> String {
    let parts: Vec<&str> = d.split('-').collect();
    match parts.as_slice() {
        [y, m, day] => {
            let m: usize = m.parse().unwrap_or(0);
            let day: u32 = day.parse().unwrap_or(0);
            match MONTHS.get(m.wrapping_sub(1)) {
                Some(mon) => format!("{day} {mon} {y}"),
                None => d.to_string(),
            }
        }
        _ => d.to_string(),
    }
}

#[derive(Deserialize)]
struct FragmentQuery {
    fragment: Option<String>,
}

async fn add_slash(UrlPath(space): UrlPath<String>) -> Redirect {
    Redirect::permanent(&format!("/{}/", url_seg(&space)))
}

async fn add_slash2(UrlPath((space, folder)): UrlPath<(String, String)>) -> Redirect {
    Redirect::permanent(&format!("/{}/{}/", url_seg(&space), url_seg(&folder)))
}

/// A path segment, percent-encoded.
fn url_seg(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            o.push(b as char);
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

fn space_list_html(name: &str, cfg: &SpaceConfig, pages: &[Summary]) -> String {
    if pages.is_empty() {
        return format!(
            r#"<div class="empty"><h2>No pages yet</h2><p>A page is a folder here with an <code>index.md</code> or <code>index.html</code> in it. It shows up as soon as it's saved.</p><p class="empty-path mono">{}/&lt;date&gt;-&lt;name&gt;/index.md</p></div>"#,
            esc(name)
        );
    }
    let _ = cfg;
    let mut out = String::new();
    let mut current = String::new();
    for p in pages {
        if p.date != current {
            if !current.is_empty() {
                out.push_str("</ul></section>");
            }
            current = p.date.clone();
            out.push_str(&format!(
                r#"<section class="day"><h2 class="day-label"><time datetime="{d}">{pd}</time></h2><ul class="pages">"#,
                d = esc(&p.date),
                pd = esc(&pretty_date(&p.date)),
            ));
        }
        let summary =
            p.summary.as_deref().map_or(String::new(), |s| format!(r#"<p class="row-summary">{}</p>"#, esc(s)));
        let badge = if p.problems > 0 {
            format!(r#"<span class="badge badge-warn">{}</span>"#, count(p.problems, "problem"))
        } else {
            String::new()
        };
        let kind = if p.kind == Kind::Html { r#"<span class="badge">Mock-up</span>"# } else { "" };
        out.push_str(&format!(
            r#"<li><a class="row" href="/{s}/{f}/"><span class="row-title">{t}</span>{summary}<span class="row-meta"><span class="mono row-folder">{fe}</span>{kind}{badge}</span></a></li>"#,
            s = url_seg(name),
            f = url_seg(&p.folder),
            t = esc(&p.title),
            fe = esc(&p.folder),
        ));
    }
    out.push_str("</ul></section>");
    out
}

async fn space_page(
    State(app): Ctx,
    headers: HeaderMap,
    UrlPath(name): UrlPath<String>,
    Query(q): Query<FragmentQuery>,
) -> Response {
    let v = match viewer(&app, &headers, &format!("/{name}/")).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let cfg = match readable(&app, &v, &name, &format!("page:{name}/*"), "There's no space by that name.").await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let pages = app.cache.list(cfg);
    let app_list = apps::list(&name, cfg);
    let mut list = apps_html(&app, &app_list);
    if !app_list.is_empty() && !pages.is_empty() {
        list.push_str(r#"<h2 class="section-label">Pages</h2>"#);
    }
    if !(pages.is_empty() && !app_list.is_empty()) {
        list.push_str(&space_list_html(&name, cfg, &pages));
    }
    if q.fragment.is_some() {
        return Html(list).into_response();
    }
    let title = cfg.title.clone().unwrap_or_else(|| name.clone());
    let mut sub = count(pages.len(), "page");
    if !app_list.is_empty() {
        sub.push_str(&format!(" · {}", count(app_list.len(), "app")));
    }
    let body = format!(
        r#"<main class="wrap"><header class="page-head"><h1>{t}</h1><p class="page-sub">{c}</p></header><div id="content" class="space-list">{list}</div></main>"#,
        t = esc(&title),
        c = esc(&sub),
    );
    let crumbs = [Crumb { label: title.clone(), href: None }];
    html_page(StatusCode::OK, layout(&format!("{title} · Pages"), &crumbs, &body, Some(&name), true))
}

// ------------------------------------------------------------ apps

/// The address of an app, with the pages site's scheme (and port, when it has one).
fn app_url(app: &App, space: &str, name: &str) -> Option<String> {
    let domain = app.apps_domain.as_deref()?;
    Some(format!("{}/", app_origin(app, &apps::host_for(space, name, domain))))
}

fn app_origin(app: &App, host: &str) -> String {
    let base = app.base_url.as_deref().unwrap_or("https://");
    let (scheme, rest) = base.split_once("://").unwrap_or(("https", ""));
    let authority = rest.split('/').next().unwrap_or_default();
    match authority.rsplit_once(':') {
        Some((_, port)) if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() => {
            format!("{scheme}://{host}:{port}")
        }
        _ => format!("{scheme}://{host}"),
    }
}

/// A space's apps, above its pages. Nothing at all when it has none.
fn apps_html(app: &App, list: &[apps::Info]) -> String {
    if list.is_empty() {
        return String::new();
    }
    let mut out = String::from(r#"<h2 class="section-label">Apps</h2><ul class="apps">"#);
    for a in list {
        let url = app_url(app, &a.space, &a.name);
        let host = url.as_deref().map(|u| u.split("://").nth(1).unwrap_or(u).trim_end_matches('/')).unwrap_or("");
        let problems = if a.problems.is_empty() {
            String::new()
        } else {
            let items: String = a.problems.iter().map(|p| format!("<li>{}</li>", esc(&p.message))).collect();
            format!(r#"<ul class="app-problems">{items}</ul>"#)
        };
        let badge = if a.problems.is_empty() {
            String::new()
        } else {
            format!(r#"<span class="badge badge-warn">{}</span>"#, count(a.problems.len(), "problem"))
        };
        let summary = a
            .manifest
            .description
            .as_deref()
            .map_or(String::new(), |d| format!(r#"<span class="row-summary">{}</span>"#, esc(d)));
        let open = match &url {
            Some(u) => format!(r#"href="{}" target="_blank" rel="noopener""#, esc(u)),
            None => String::new(),
        };
        out.push_str(&format!(
            r#"<li class="app-item"><a class="app-row" {open}><img class="app-icon" src="/_/apps/{s}/{n}/icon" alt="" width="40" height="40"><span class="app-text"><span class="row-title">{t}</span>{summary}<span class="row-meta"><span class="mono row-folder">{h}</span>{badge}</span></span></a>{problems}</li>"#,
            s = url_seg(&a.space),
            n = url_seg(&a.name),
            t = esc(&a.title()),
            h = esc(host),
        ));
    }
    out.push_str("</ul>");
    out
}

async fn app_icon(State(app): Ctx, headers: HeaderMap, UrlPath((space, name)): UrlPath<(String, String)>) -> Response {
    let Some(v) = app.auth.viewer(&headers).await else { return StatusCode::UNAUTHORIZED.into_response() };
    let Ok(cfg) = readable(&app, &v, &space, &format!("app:{space}/{name}"), "").await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(info) = apps::inspect(&space, cfg, &name) else { return StatusCode::NOT_FOUND.into_response() };
    let file = info.manifest.icon.as_deref().and_then(|i| space::resolve(&info.root, i));
    let (kind, body) = match file {
        Some(f) => match tokio::fs::read(&f).await {
            Ok(b) => (content_type(&f), b),
            Err(_) => return StatusCode::NOT_FOUND.into_response(),
        },
        None => ("image/svg+xml", apps::default_icon(&info.title()).into_bytes()),
    };
    let mut res = (StatusCode::OK, body).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(kind));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(FILE_CSP));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    res
}

#[derive(Deserialize)]
struct AuthorizeQuery {
    app: String,
    state: String,
}

/// The pages site's side of an app's sign-in: the person signs in here, the policy is checked, and the app's own host
/// (worked out from its name) gets a one-time code. Apps from untrusted spaces show a notice first.
async fn authorize(State(app): Ctx, headers: HeaderMap, Query(q): Query<AuthorizeQuery>) -> Response {
    authorize_inner(&app, &headers, q, false).await
}

async fn authorize_confirmed(State(app): Ctx, headers: HeaderMap, Form(q): Form<AuthorizeQuery>) -> Response {
    authorize_inner(&app, &headers, q, true).await
}

async fn authorize_inner(app: &App, headers: &HeaderMap, q: AuthorizeQuery, confirmed: bool) -> Response {
    let here = format!(
        "/_/apps/authorize?app={}&state={}",
        form_urlencoded::byte_serialize(q.app.as_bytes()).collect::<String>(),
        form_urlencoded::byte_serialize(q.state.as_bytes()).collect::<String>(),
    );
    let v = match viewer(app, headers, &here).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some((space, name)) = q.app.split_once('/') else { return not_found("There's no app by that name.") };
    let Some(domain) = app.apps_domain.as_deref() else { return not_found("There's no app by that name.") };
    let cfg = match readable(app, &v, space, &format!("app:{space}/{name}"), "There's no app by that name.").await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let Some(info) = apps::inspect(space, cfg, name) else { return not_found("There's no app by that name.") };
    if q.state.len() < 32 || q.state.len() > 128 || !q.state.chars().all(|c| c.is_ascii_alphanumeric()) {
        return not_found("That sign-in link isn't valid: open the app again.");
    }
    if cfg.untrusted && !confirmed {
        let crumbs = [Crumb { label: info.title(), href: None }];
        return html_page(
            StatusCode::OK,
            layout(
                &format!("Open {}? · Pages", info.title()),
                &crumbs,
                &apps::notice_html(&info, &q.state),
                None,
                true,
            ),
        );
    }
    let host = apps::host_for(space, name, domain);
    let code = app.broker.issue(&host, &q.state, v).await;
    Redirect::to(&format!("{}/_fl/callback?code={code}&state={}", app_origin(app, &host), q.state)).into_response()
}

// ------------------------------------------------------------ a page

fn problems_html(r: &Rendered) -> String {
    if r.problems.is_empty() {
        return String::new();
    }
    let mut s = format!(
        r#"<aside class="problems" id="problems" role="note"><p class="problems-title">{} on this page</p><ul>"#,
        if r.problems.len() == 1 { "A problem".to_string() } else { format!("{} problems", r.problems.len()) }
    );
    for p in &r.problems {
        s.push_str(&format!("<li>{}</li>", esc(&p.message)));
    }
    s.push_str("</ul></aside>");
    s
}

fn page_content(space: &str, folder: &str, r: &Rendered, date: &str) -> String {
    let title = r.title.clone().unwrap_or_else(|| folder.to_string());
    let summary = r.summary.as_deref().map_or(String::new(), |s| format!(r#"<p class="lead">{}</p>"#, esc(s)));
    let meta = format!(
        r#"<p class="meta"><time datetime="{d}">{pd}</time><span aria-hidden="true">·</span><span class="mono">{f}</span></p>"#,
        d = esc(date),
        pd = esc(&pretty_date(date)),
        f = esc(folder),
    );
    match r.kind {
        Kind::Markdown => format!(
            r#"<article class="doc"><header class="doc-head"><h1>{t}</h1>{summary}{meta}</header>{problems}<div class="prose">{body}</div></article>"#,
            t = esc(&title),
            problems = problems_html(r),
            body = r.body,
        ),
        Kind::Html => format!(
            r#"<div class="mock"><header class="doc-head doc-head-mock"><div><h1>{t}</h1>{summary}{meta}</div><a class="btn btn-sm" href="/{s}/{f}/index.html" target="_blank" rel="noopener">Open on its own</a></header>{problems}<iframe class="mock-frame" src="/{s}/{f}/index.html" title="{t}" sandbox="allow-same-origin" referrerpolicy="no-referrer"></iframe></div>"#,
            t = esc(&title),
            s = url_seg(space),
            f = url_seg(folder),
            problems = problems_html(r),
        ),
        Kind::Empty => format!(
            r#"<article class="doc"><header class="doc-head"><h1>{t}</h1>{meta}</header>{problems}</article>"#,
            t = esc(&title),
            problems = problems_html(r),
        ),
    }
}

async fn page_view(
    State(app): Ctx,
    headers: HeaderMap,
    UrlPath((name, folder)): UrlPath<(String, String)>,
    Query(q): Query<FragmentQuery>,
) -> Response {
    let v = match viewer(&app, &headers, &format!("/{name}/{folder}/")).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let cfg = match readable(&app, &v, &name, &format!("page:{name}/{folder}"), "There's no page by that name.").await {
        Ok(c) => c,
        Err(r) => return r,
    };
    if !space::valid_folder(&folder) || !cfg.dir.join(&folder).is_dir() {
        return not_found("There's no page by that name in this space.");
    }
    let r = render::page(&cfg.dir, Path::new(&folder));
    let date = space::folder_date(&folder).map(str::to_string).unwrap_or_else(|| {
        space::day(
            std::fs::metadata(cfg.dir.join(&folder))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs()),
        )
    });
    let content = page_content(&name, &folder, &r, &date);
    if q.fragment.is_some() {
        return Html(content).into_response();
    }
    let space_title = cfg.title.clone().unwrap_or_else(|| name.clone());
    let title = r.title.clone().unwrap_or_else(|| folder.clone());
    let wide = r.kind == Kind::Html;
    let body = format!(
        r#"<main class="wrap"><div id="content">{content}</div></main><dialog class="lightbox" id="lightbox" aria-label="Image"></dialog>"#
    );
    let crumbs = [
        Crumb { label: space_title, href: Some(format!("/{}/", url_seg(&name))) },
        Crumb { label: title.clone(), href: None },
    ];
    html_page(
        StatusCode::OK,
        layout_with(&format!("{title} · Pages"), &crumbs, &body, Some(&format!("{name}/{folder}")), true, wide),
    )
}

// ------------------------------------------------------------ files

pub(crate) fn content_type(path: &Path) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "css" => "text/css; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "md" | "txt" | "log" | "csv" | "diff" | "patch" => "text/plain; charset=utf-8",
        "json" | "map" => "application/json",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "webmanifest" => "application/manifest+json",
        "ico" => "image/x-icon",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ => "application/octet-stream",
    }
}

async fn file(
    State(app): Ctx,
    headers: HeaderMap,
    UrlPath((name, folder, rest)): UrlPath<(String, String, String)>,
) -> Response {
    let v = match viewer(&app, &headers, &format!("/{name}/{folder}/{rest}")).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let cfg = match readable(&app, &v, &name, &format!("page:{name}/{folder}"), "There's no file by that name.").await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let rel = format!("{folder}/{rest}");
    let Some(path) = space::resolve(&cfg.dir, &rel) else { return not_found("There's no file by that name.") };
    if path.is_dir() {
        return not_found("That's a folder, not a file.");
    }
    let kind = content_type(&path);
    let body = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(_) => return not_found("That file can't be read."),
    };
    let mut res = (StatusCode::OK, body).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(kind));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(FILE_CSP));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("SAMEORIGIN"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-cache"));
    if kind == "application/octet-stream" {
        h.insert(header::CONTENT_DISPOSITION, HeaderValue::from_static("attachment"));
    }
    res
}

// ------------------------------------------------------------ live

#[derive(Deserialize)]
struct EventsQuery {
    path: String,
}

/// Server-sent events for one space's list or one page: `change` when its files change. The folder is checked every
/// second for a page and every five for a list (polling: filesystem events miss writes made on the far side of a
/// network or virtual-machine share).
async fn events(State(app): Ctx, headers: HeaderMap, Query(q): Query<EventsQuery>) -> Response {
    let Some(v) = app.auth.viewer(&headers).await else { return StatusCode::UNAUTHORIZED.into_response() };
    let mut parts = q.path.splitn(2, '/');
    let name = parts.next().unwrap_or_default().to_string();
    let folder = parts.next().map(str::to_string);
    let target = match &folder {
        Some(f) => format!("page:{name}/{f}"),
        None => format!("page:{name}/*"),
    };
    let cfg = match readable(&app, &v, &name, &target, "").await {
        Ok(c) => c,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let (dir, every) = match &folder {
        Some(f) if space::valid_folder(f) => (cfg.dir.join(f), Duration::from_secs(1)),
        Some(_) => return StatusCode::NOT_FOUND.into_response(),
        None => (cfg.dir.clone(), Duration::from_secs(5)),
    };
    Sse::new(watch(dir, every)).keep_alive(KeepAlive::new().interval(Duration::from_secs(20))).into_response()
}

pub(crate) fn watch(dir: std::path::PathBuf, every: Duration) -> impl Stream<Item = Result<Event, Infallible>> {
    futures::stream::unfold((dir, None::<String>), move |(dir, last)| async move {
        let mut last = last;
        loop {
            let d = dir.clone();
            let fp = tokio::task::spawn_blocking(move || space::fingerprint(&d)).await.unwrap_or_default();
            match &last {
                None => {
                    last = Some(fp.clone());
                    return Some((Ok(Event::default().event("ready").data(fp)), (dir, last)));
                }
                Some(l) if *l != fp => {
                    return Some((Ok(Event::default().event("change").data(fp.clone())), (dir, Some(fp))));
                }
                _ => {}
            }
            tokio::time::sleep(every).await;
        }
    })
}

#[cfg(test)]
#[allow(clippy::cloned_ref_to_slice_refs)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    const KEY: &str = "0123456789abcdef0123456789abcdef-key";

    fn app() -> (Arc<App>, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(space::MARKER), "").unwrap();
        std::fs::create_dir_all(d.path().join("2026-11-27-qa/img")).unwrap();
        std::fs::write(d.path().join("2026-11-27-qa/index.md"), "# QA round\n\nWhat it shows.\n\n![A](img/a.svg)\n")
            .unwrap();
        std::fs::write(
            d.path().join("2026-11-27-qa/img/a.svg"),
            r#"<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script></svg>"#,
        )
        .unwrap();
        std::fs::create_dir_all(d.path().join("2026-11-26-mock")).unwrap();
        std::fs::write(d.path().join("2026-11-26-mock/index.html"), "<title>Mock</title><p>hi</p>").unwrap();
        let mut spaces = BTreeMap::new();
        spaces.insert(
            "personal".into(),
            SpaceConfig {
                dir: d.path().to_path_buf(),
                title: None,
                owner: None,
                tenant: None,
                apps: None,
                untrusted: false,
            },
        );
        let app = App {
            spaces,
            auth: auth::Auth::new(Some(auth::LoginKey::new(KEY.into()).unwrap()), None, None).unwrap(),
            trusted_proxies: vec![],
            login_hint: None,
            cache: Cache::default(),
            base_url: None,
            apps_domain: None,
            broker: apps::Broker::default(),
        };
        (Arc::new(app), d)
    }

    async fn get(app: &Arc<App>, path: &str, cookie: Option<&str>) -> Response {
        let mut req = Request::get(path);
        if let Some(c) = cookie {
            req = req.header(header::COOKIE, format!("{}={c}", auth::COOKIE));
        }
        let mut req = req.body(Body::empty()).unwrap();
        req.extensions_mut().insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))));
        router(app.clone()).oneshot(req).await.unwrap()
    }

    async fn body(r: Response) -> String {
        String::from_utf8(axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap()
    }

    async fn signed_in(app: &Arc<App>) -> String {
        app.auth.start(Viewer { person: None }).await
    }

    #[tokio::test]
    async fn everything_but_health_and_sign_in_needs_the_login() {
        let (app, _d) = app();
        let r = get(&app, "/personal/2026-11-27-qa/", None).await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert_eq!(r.headers()[header::LOCATION], "/_/login?next=%2Fpersonal%2F2026-11-27-qa%2F");
        assert_eq!(get(&app, "/personal/2026-11-27-qa/img/a.svg", None).await.status(), StatusCode::SEE_OTHER);
        assert_eq!(get(&app, "/healthz", None).await.status(), StatusCode::OK);
        assert_eq!(get(&app, "/_/login", None).await.status(), StatusCode::OK);
        assert_eq!(get(&app, "/_/events?path=personal", None).await.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_space_lists_its_pages_and_a_page_renders() {
        let (app, _d) = app();
        let sid = signed_in(&app).await;
        // One space: the root goes straight to it.
        let r = get(&app, "/", Some(&sid)).await;
        assert_eq!(r.headers()[header::LOCATION], "/personal/");
        let r = get(&app, "/personal/", Some(&sid)).await;
        assert_eq!(r.status(), StatusCode::OK);
        let b = body(r).await;
        assert!(b.find("QA round").unwrap() < b.find("Mock").unwrap(), "newest first");
        let r = get(&app, "/personal/2026-11-27-qa/", Some(&sid)).await;
        let csp = r.headers()[header::CONTENT_SECURITY_POLICY].to_str().unwrap().to_string();
        assert!(csp.contains("script-src 'self'"));
        let b = body(r).await;
        assert!(b.contains("<h1>QA round</h1>"));
        assert!(b.contains(r#"<p class="lead">What it shows.</p>"#));
        assert!(!b.contains(r#"id="problems""#));
        assert_eq!(get(&app, "/personal/2026-11-27-qa", Some(&sid)).await.status(), StatusCode::PERMANENT_REDIRECT);
    }

    #[tokio::test]
    async fn a_page_s_files_are_sandboxed_and_hidden_files_are_not_served() {
        let (app, _d) = app();
        let sid = signed_in(&app).await;
        let r = get(&app, "/personal/2026-11-27-qa/img/a.svg", Some(&sid)).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()[header::CONTENT_TYPE], "image/svg+xml");
        let csp = r.headers()[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        assert!(csp.starts_with("sandbox") && csp.contains("script-src 'none'"), "{csp}");
        assert_eq!(
            get(&app, "/personal/2026-11-27-qa/../../etc/passwd", Some(&sid)).await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(get(&app, "/personal/.fleetlayer-space/x", Some(&sid)).await.status(), StatusCode::NOT_FOUND);
        assert_eq!(get(&app, "/nope/", Some(&sid)).await.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_mock_up_is_framed_sandboxed() {
        let (app, _d) = app();
        let sid = signed_in(&app).await;
        let b = body(get(&app, "/personal/2026-11-26-mock/", Some(&sid)).await).await;
        assert!(b.contains(r#"sandbox="allow-same-origin""#), "{b}");
        assert!(!b.contains("allow-scripts"));
    }

    #[tokio::test]
    async fn health_fails_when_a_space_s_marker_is_gone() {
        let (app, d) = app();
        std::fs::remove_file(d.path().join(space::MARKER)).unwrap();
        let r = get(&app, "/healthz", None).await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(body(r).await.contains("personal"));
    }

    fn app_with_apps(untrusted: bool) -> (Arc<App>, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        let pages = d.path().join("pages");
        let appsdir = d.path().join("apps");
        std::fs::create_dir_all(pages.join("2026-11-27-qa")).unwrap();
        std::fs::write(pages.join("2026-11-27-qa/index.md"), "# QA\n\nS.\n").unwrap();
        std::fs::create_dir_all(appsdir.join("recipes/node_modules/x")).unwrap();
        std::fs::create_dir_all(appsdir.join("other")).unwrap();
        for dir in [&pages, &appsdir] {
            std::fs::write(dir.join(space::MARKER), "").unwrap();
        }
        std::fs::write(
            appsdir.join("recipes/index.html"),
            "<html><head><title>R</title></head><body><script src=\"app.js\"></script></body></html>",
        )
        .unwrap();
        std::fs::write(appsdir.join("recipes/app.js"), "console.log(1)").unwrap();
        std::fs::write(appsdir.join("recipes/node_modules/x/index.js"), "secret").unwrap();
        std::fs::write(appsdir.join("recipes/app.json"), r#"{"name":"Recipes","network":["https://api.example.com"]}"#)
            .unwrap();
        std::fs::write(appsdir.join("other/index.html"), "<p>other</p>").unwrap();
        let mut spaces = BTreeMap::new();
        spaces.insert(
            "personal".into(),
            SpaceConfig { dir: pages, title: None, owner: None, tenant: None, apps: Some(appsdir), untrusted },
        );
        let app = App {
            spaces,
            auth: auth::Auth::new(Some(auth::LoginKey::new(KEY.into()).unwrap()), None, None).unwrap(),
            trusted_proxies: vec![],
            login_hint: None,
            cache: Cache::default(),
            base_url: Some("https://pages.test".into()),
            apps_domain: Some("apps.test".into()),
            broker: apps::Broker::default(),
        };
        (Arc::new(app), d)
    }

    async fn req(
        app: &Arc<App>,
        method: &str,
        host: &str,
        path: &str,
        cookies: &[String],
        form: Option<&str>,
    ) -> Response {
        let mut r = Request::builder().method(method).uri(path).header(header::HOST, host);
        if !cookies.is_empty() {
            r = r.header(header::COOKIE, cookies.join("; "));
        }
        let body = match form {
            Some(f) => {
                r = r.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
                Body::from(f.to_string())
            }
            None => Body::empty(),
        };
        let mut r = r.body(body).unwrap();
        r.extensions_mut().insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))));
        router(app.clone()).oneshot(r).await.unwrap()
    }

    fn location(r: &Response) -> String {
        r.headers()[header::LOCATION].to_str().unwrap().to_string()
    }

    /// `name=value` from a response's Set-Cookie headers.
    fn set_cookie(r: &Response, name: &str) -> String {
        r.headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .find(|c| c.starts_with(&format!("{name}=")))
            .map(|c| c.split(';').next().unwrap().to_string())
            .unwrap()
    }

    fn param(url: &str, key: &str) -> String {
        let q = url.split_once('?').unwrap().1;
        form_urlencoded::parse(q.as_bytes()).find(|(k, _)| k == key).unwrap().1.into_owned()
    }

    #[tokio::test]
    async fn an_app_signs_in_through_the_pages_site_with_a_one_time_code() {
        let (app, _d) = app_with_apps(false);
        let host = "recipes--personal.apps.test";
        let pages_cookie = format!("{}={}", auth::COOKIE, signed_in(&app).await);

        // No session on the app: off to the pages site, with a state kept in a cookie on the app.
        let r = req(&app, "GET", host, "/list?x=1", &[], None).await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        let to = location(&r);
        assert!(to.starts_with("https://pages.test/_/apps/authorize?app=personal/recipes&state="), "{to}");
        let state = param(&to, "state");
        let state_cookie = set_cookie(&r, "__Host-fl_state");

        // The pages site sends a code to the app's own host, worked out from its name.
        let path = to.trim_start_matches("https://pages.test");
        let r = req(&app, "GET", "pages.test", path, &[pages_cookie.clone()], None).await;
        let back = location(&r);
        assert!(back.starts_with(&format!("https://{host}/_fl/callback?code=")), "{back}");
        let cb = back.trim_start_matches(&format!("https://{host}"));

        // Exchanging it sets the app's own cookie and returns to where the person was going.
        let r = req(&app, "GET", host, cb, &[state_cookie.clone()], None).await;
        assert_eq!(location(&r), "/list?x=1");
        let session = set_cookie(&r, "__Host-fl_app");
        let raw = r
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(raw.contains("__Host-fl_app=") && raw.contains("Secure; HttpOnly; SameSite=Lax; Path=/"), "{raw}");

        // The app works, with its own policy and the home-screen manifest added.
        let r = req(&app, "GET", host, "/", &[session.clone()], None).await;
        assert_eq!(r.status(), StatusCode::OK);
        let csp = r.headers()[header::CONTENT_SECURITY_POLICY].to_str().unwrap().to_string();
        assert!(csp.contains("connect-src 'self' https://api.example.com;"), "{csp}");
        assert!(body(r).await.contains(r#"<link rel="manifest" href="/_fl/manifest.webmanifest">"#));
        // A single-page app's own route gets index.html; node_modules and app.json never.
        assert_eq!(req(&app, "GET", host, "/settings", &[session.clone()], None).await.status(), StatusCode::OK);
        assert_eq!(
            req(&app, "GET", host, "/node_modules/x/index.js", &[session.clone()], None).await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(req(&app, "GET", host, "/app.json", &[session.clone()], None).await.status(), StatusCode::NOT_FOUND);

        // The code works once, only for its host and state.
        assert_eq!(req(&app, "GET", host, cb, &[state_cookie.clone()], None).await.status(), StatusCode::BAD_REQUEST);
        // The session doesn't open another app.
        let r = req(&app, "GET", "other--personal.apps.test", "/", &[session.clone()], None).await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert!(location(&r).contains("app=personal/other"));
        let _ = state;
    }

    #[tokio::test]
    async fn a_code_for_one_app_doesn_t_open_another() {
        let (app, _d) = app_with_apps(false);
        let pages_cookie = format!("{}={}", auth::COOKIE, signed_in(&app).await);
        let state = "a".repeat(64);
        let r = req(
            &app,
            "GET",
            "pages.test",
            &format!("/_/apps/authorize?app=personal/recipes&state={state}"),
            &[pages_cookie],
            None,
        )
        .await;
        let back = location(&r);
        let code = param(&back, "code");
        let state_cookie = format!("__Host-fl_state={state}.{}", hex::encode("/"));
        let r = req(
            &app,
            "GET",
            "other--personal.apps.test",
            &format!("/_fl/callback?code={code}&state={state}"),
            &[state_cookie],
            None,
        )
        .await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn signing_in_to_an_app_needs_the_pages_login_and_a_real_app() {
        let (app, _d) = app_with_apps(false);
        let state = "b".repeat(64);
        let r =
            req(&app, "GET", "pages.test", &format!("/_/apps/authorize?app=personal/recipes&state={state}"), &[], None)
                .await;
        assert!(location(&r).starts_with("/_/login?next="), "{}", location(&r));
        let pages_cookie = format!("{}={}", auth::COOKIE, signed_in(&app).await);
        let r = req(
            &app,
            "GET",
            "pages.test",
            &format!("/_/apps/authorize?app=personal/nope&state={state}"),
            &[pages_cookie.clone()],
            None,
        )
        .await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let r =
            req(&app, "GET", "pages.test", "/_/apps/authorize?app=personal/recipes&state=short", &[pages_cookie], None)
                .await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_untrusted_space_s_app_shows_a_notice_and_reaches_no_other_site() {
        let (app, _d) = app_with_apps(true);
        let pages_cookie = format!("{}={}", auth::COOKIE, signed_in(&app).await);
        let state = "c".repeat(64);
        let r = req(
            &app,
            "GET",
            "pages.test",
            &format!("/_/apps/authorize?app=personal/recipes&state={state}"),
            &[pages_cookie.clone()],
            None,
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let b = body(r).await;
        assert!(b.contains("reads messages from outside") && b.contains("can't reach any other site"), "{b}");
        let r = req(
            &app,
            "POST",
            "pages.test",
            "/_/apps/authorize",
            &[pages_cookie],
            Some(&format!("app=personal%2Frecipes&state={state}")),
        )
        .await;
        assert!(location(&r).contains("/_fl/callback?code="));
        let info = apps::inspect("personal", &app.spaces["personal"], "recipes").unwrap();
        assert!(info.network.is_empty());
        assert!(apps::csp(&info).contains("connect-src 'self';"));
        assert!(info.problems.iter().any(|p| p.message.contains("can't reach other sites")));
    }

    #[tokio::test]
    async fn a_space_lists_its_apps_only_when_it_has_some() {
        let (app, _d) = app_with_apps(false);
        let sid = signed_in(&app).await;
        let b = body(get_host(&app, "pages.test", "/personal/", &sid).await).await;
        assert!(b.contains("Recipes") && b.contains("recipes--personal.apps.test"), "{b}");
        let (app2, _d2) = self::app();
        let sid2 = signed_in(&app2).await;
        let b = body(get(&app2, "/personal/", Some(&sid2)).await).await;
        assert!(!b.contains("Apps"), "{b}");
    }

    async fn get_host(app: &Arc<App>, host: &str, path: &str, sid: &str) -> Response {
        req(app, "GET", host, path, &[format!("{}={sid}", auth::COOKIE)], None).await
    }

    #[test]
    fn the_apps_host_pattern_is_relative_to_the_pages_site_s_domain() {
        let (mut a, _d) = app();
        let m = Arc::get_mut(&mut a).unwrap();
        m.base_url = Some("http://pages.localhost:7680".into());
        m.apps_domain = Some("apps.localhost".into());
        assert_eq!(a.apps_host_pattern().as_deref(), Some("*--*.apps"));
        let m = Arc::get_mut(&mut a).unwrap();
        m.base_url = Some("https://pages.home.example".into());
        m.apps_domain = Some("home.example".into());
        assert_eq!(a.apps_host_pattern().as_deref(), Some("*--*"));
        let m = Arc::get_mut(&mut a).unwrap();
        m.apps_domain = Some("apps.other.example".into());
        assert_eq!(a.apps_host_pattern(), None);
    }

    #[test]
    fn after_signing_in_only_local_paths() {
        assert_eq!(safe_next(Some("/personal/")), "/personal/");
        assert_eq!(safe_next(Some("//evil.example")), "/");
        assert_eq!(safe_next(Some("https://evil.example")), "/");
        assert_eq!(safe_next(None), "/");
    }

    /// A site with Fleetlayer's sign-in only, against a fake issuer; the checker lets alice read `personal` only, and
    /// can't be reached for `down`.
    async fn signin_app() -> (Arc<App>, tempfile::TempDir, Arc<fleetlayer_oidc::testing::FakeIssuer>) {
        let (base, d) = app();
        let f = fleetlayer_oidc::testing::FakeIssuer::start().await;
        let oidc = crate::oidc::Oidc::new(&f.url, "pages", "s3cret", "https://pages.test").unwrap();
        let checker = auth::Checker::Fixed(Box::new(|t: &str| match t {
            "page:personal/*" | "page:personal/2026-11-27-qa" => Ok(true),
            t if t.starts_with("page:down/") => Err("no answer".into()),
            _ => Ok(false),
        }));
        let mut spaces = base.spaces.clone();
        let mut down = spaces["personal"].clone();
        down.title = Some("Down".into());
        spaces.insert("down".into(), down);
        let app = App {
            spaces,
            auth: auth::Auth::new(None, Some(oidc), Some(checker)).unwrap(),
            trusted_proxies: vec![],
            login_hint: None,
            cache: Cache::default(),
            base_url: Some("https://pages.test".into()),
            apps_domain: None,
            broker: apps::Broker::default(),
        };
        (Arc::new(app), d, f)
    }

    async fn get_with(app: &Arc<App>, path: &str, cookie: &str) -> Response {
        let mut req = Request::get(path).header(header::COOKIE, cookie).body(Body::empty()).unwrap();
        req.extensions_mut().insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))));
        router(app.clone()).oneshot(req).await.unwrap()
    }

    fn cookie_set(r: &Response, name: &str) -> Option<String> {
        r.headers().get_all(header::SET_COOKIE).iter().filter_map(|v| v.to_str().ok()).find_map(|c| {
            c.strip_prefix(&format!("{name}=")).map(|rest| rest.split(';').next().unwrap_or_default().to_string())
        })
    }

    #[tokio::test]
    async fn signing_in_through_fleetlayer_then_reading_what_the_policy_allows() {
        let (app, _d, f) = signin_app().await;
        // No key: the login page goes straight to the sign-in service.
        let r = get(&app, "/_/login?next=/personal/", None).await;
        assert_eq!(r.headers()[header::LOCATION], "/_/login/start?next=%2Fpersonal%2F");
        let r = get(&app, "/_/login/start?next=/personal/", None).await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        let to = r.headers()[header::LOCATION].to_str().unwrap().to_string();
        assert!(to.starts_with(&format!("{}/authorize?", f.url)), "{to}");
        let bound = cookie_set(&r, "pages_oidc").unwrap();
        let state = fleetlayer_oidc::testing::param(&to, "state");
        assert_eq!(bound, state);
        *f.token.lock().unwrap() =
            Some(fleetlayer_oidc::testing::good(&f, f.claims(&fleetlayer_oidc::testing::param(&to, "nonce"))));

        // Coming back without the browser's cookie fails; with it, signed in.
        let cb = format!("/_/auth/callback?code=c1&state={state}");
        assert_eq!(get(&app, &cb, None).await.status(), StatusCode::FORBIDDEN);
        let r = get(&app, "/_/login/start?next=/personal/", None).await;
        let to = r.headers()[header::LOCATION].to_str().unwrap().to_string();
        let state = fleetlayer_oidc::testing::param(&to, "state");
        *f.token.lock().unwrap() =
            Some(fleetlayer_oidc::testing::good(&f, f.claims(&fleetlayer_oidc::testing::param(&to, "nonce"))));
        let r =
            get_with(&app, &format!("/_/auth/callback?code=c2&state={state}"), &format!("pages_oidc={state}")).await;
        assert_eq!(r.status(), StatusCode::OK);
        let sid = cookie_set(&r, auth::COOKIE).unwrap();
        assert!(body(r).await.contains(r#"url=/personal/"#));
        // The same state again is refused.
        let r =
            get_with(&app, &format!("/_/auth/callback?code=c2&state={state}"), &format!("pages_oidc={state}")).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);

        // Alice reads personal, not work-only spaces, and sees "can't check" (not the page) when the check fails.
        assert_eq!(get(&app, "/personal/", Some(&sid)).await.status(), StatusCode::OK);
        assert_eq!(get(&app, "/personal/2026-11-27-qa/", Some(&sid)).await.status(), StatusCode::OK);
        assert_eq!(get(&app, "/personal/2026-11-26-mock/", Some(&sid)).await.status(), StatusCode::NOT_FOUND);
        let r = get(&app, "/down/2026-11-27-qa/", Some(&sid)).await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!body(r).await.contains("QA round"));
        assert_eq!(get(&app, "/", Some(&sid)).await.status(), StatusCode::SERVICE_UNAVAILABLE);
        // The key form isn't there, and posting a key does nothing.
        let r = get(&app, "/_/signed-out", None).await;
        let b = body(r).await;
        assert!(b.contains("Sign in with Fleetlayer") && !b.contains("Login key"), "{b}");

        // Signing out ends the session here and sends the browser to the sign-in service's logout.
        let mut req = Request::post("/_/logout")
            .header(header::COOKIE, format!("{}={sid}", auth::COOKIE))
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))));
        let r = router(app.clone()).oneshot(req).await.unwrap();
        let b = body(r).await;
        assert!(b.contains(&format!("url={}/logout?", f.url)) && b.contains("signed-out"), "{b}");
        assert_eq!(get(&app, "/personal/", Some(&sid)).await.status(), StatusCode::SEE_OTHER);
    }
}
