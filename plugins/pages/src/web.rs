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

use crate::auth::{self, Auth, Viewer};
use crate::render::{self, Kind, Rendered};
use crate::space::{self, Cache, SpaceConfig, Summary};

pub struct App {
    pub spaces: BTreeMap<String, SpaceConfig>,
    pub auth: Auth,
    pub trusted_proxies: Vec<IpAddr>,
    pub login_hint: Option<String>,
    pub cache: Cache,
}

type Ctx = State<Arc<App>>;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/_/healthz", get(healthz))
        .route("/_/login", get(login_page).post(login))
        .route("/_/logout", post(logout))
        .route("/_/assets/{name}", get(asset))
        .route("/_/events", get(events))
        .route("/", get(spaces_page))
        .route("/{space}", get(add_slash))
        .route("/{space}/", get(space_page))
        .route("/{space}/{folder}", get(add_slash2))
        .route("/{space}/{folder}/", get(page_view))
        .route("/{space}/{folder}/{*file}", get(file))
        .fallback(not_found_any)
        .layer(axum::middleware::map_response(security_headers))
        .with_state(app)
}

pub async fn serve(app: Arc<App>, listen: SocketAddr) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(%listen, "pages listening");
    axum::serve(listener, router(app).into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
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
    let hint = app.login_hint.as_deref().map_or(String::new(), |h| format!(r#"<p class="login-hint">{}</p>"#, esc(h)));
    let (msg, invalid) = if msg.is_empty() {
        (String::new(), "")
    } else {
        (
            format!(r#"<p class="field-error" id="login-error" role="alert">{}</p>"#, esc(msg)),
            r#" aria-invalid="true" aria-describedby="login-error""#,
        )
    };
    Html(
        LOGIN_HTML
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

async fn login_page(State(app): Ctx, Query(q): Query<NextQuery>) -> Html<String> {
    login_html(&app, "", &safe_next(q.next.as_deref()))
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
    let Auth::LoginKey(key) = &app.auth;
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
    let sid = key.start().await;
    ([(header::SET_COOKIE, auth::cookie(&sid))], Redirect::to(&next)).into_response()
}

async fn logout(State(app): Ctx, headers: HeaderMap) -> Response {
    let Auth::LoginKey(key) = &app.auth;
    key.end(&headers).await;
    ([(header::SET_COOKIE, auth::clear_cookie())], Redirect::to("/_/login")).into_response()
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

/// The space's config, when it exists and the viewer may read it. A space they may not read is "not found".
async fn readable<'a>(app: &'a App, v: &Viewer, name: &str) -> Option<&'a SpaceConfig> {
    let cfg = app.spaces.get(name)?;
    app.auth.may_read(v, name).await.then_some(cfg)
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
        r#"<main class="wrap empty-page"><div class="empty"><h1>Not here</h1><p>{}</p><a class="btn" href="/">All spaces</a></div></main>"#,
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
        if !app.auth.may_read(&v, name).await {
            continue;
        }
        rows.push((name.clone(), cfg.clone(), app.cache.list(cfg)));
    }
    if rows.len() == 1 {
        return Redirect::to(&format!("/{}/", rows[0].0)).into_response();
    }
    let mut body = String::from(r#"<main class="wrap"><header class="page-head"><h1>Spaces</h1></header>"#);
    if rows.is_empty() {
        body.push_str(r#"<div class="empty"><h2>No spaces you can read</h2><p>Ask whoever runs this service to share a space with you.</p></div>"#);
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
            r#"<div class="empty"><h2>No pages yet</h2><p>A page is a folder in this space with an <code>index.md</code> or <code>index.html</code>. It shows here as soon as it's saved.</p><p class="empty-path mono">{}/&lt;date&gt;-&lt;name&gt;/index.md</p></div>"#,
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
    let Some(cfg) = readable(&app, &v, &name).await else { return not_found("There's no space by that name.") };
    let pages = app.cache.list(cfg);
    let list = space_list_html(&name, cfg, &pages);
    if q.fragment.is_some() {
        return Html(list).into_response();
    }
    let title = cfg.title.clone().unwrap_or_else(|| name.clone());
    let body = format!(
        r#"<main class="wrap"><header class="page-head"><h1>{t}</h1><p class="page-sub">{c}</p></header><div id="content" class="space-list">{list}</div></main>"#,
        t = esc(&title),
        c = count(pages.len(), "page"),
    );
    let crumbs = [Crumb { label: title.clone(), href: None }];
    html_page(StatusCode::OK, layout(&format!("{title} · Pages"), &crumbs, &body, Some(&name), true))
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
    let Some(cfg) = readable(&app, &v, &name).await else { return not_found("There's no space by that name.") };
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

fn content_type(path: &Path) -> &'static str {
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
        "json" => "application/json",
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
    let Some(cfg) = readable(&app, &v, &name).await else { return not_found("There's no space by that name.") };
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
    let Some(cfg) = readable(&app, &v, &name).await else { return StatusCode::NOT_FOUND.into_response() };
    let (dir, every) = match &folder {
        Some(f) if space::valid_folder(f) => (cfg.dir.join(f), Duration::from_secs(1)),
        Some(_) => return StatusCode::NOT_FOUND.into_response(),
        None => (cfg.dir.clone(), Duration::from_secs(5)),
    };
    Sse::new(watch(dir, every)).keep_alive(KeepAlive::new().interval(Duration::from_secs(20))).into_response()
}

fn watch(dir: std::path::PathBuf, every: Duration) -> impl Stream<Item = Result<Event, Infallible>> {
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
            SpaceConfig { dir: d.path().to_path_buf(), title: None, owner: None, tenant: None },
        );
        let app = App {
            spaces,
            auth: Auth::LoginKey(auth::LoginKey::new(KEY.into()).unwrap()),
            trusted_proxies: vec![],
            login_hint: None,
            cache: Cache::default(),
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
        let Auth::LoginKey(k) = &app.auth;
        k.start().await
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

    #[test]
    fn after_signing_in_only_local_paths() {
        assert_eq!(safe_next(Some("/personal/")), "/personal/");
        assert_eq!(safe_next(Some("//evil.example")), "/");
        assert_eq!(safe_next(Some("https://evil.example")), "/");
        assert_eq!(safe_next(None), "/");
    }
}
