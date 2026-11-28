//! Apps: a folder of HTML, CSS and JavaScript (or a build's `dist/`), hosted on a subdomain of its own,
//! `<app>--<space>.<apps domain>`, so the browser keeps each app's scripts away from every other app, from pages and
//! from the sign-in. Scripts run; what they may reach is set by the app's `app.json` within its space's ceiling, and
//! enforced by the app origin's Content-Security-Policy.
//!
//! Sign-in is shaped like OpenID Connect, so moving to Fleetlayer sign-in is a swap: an app origin with no session
//! sends the browser to the pages site's authorize endpoint with a random `state` (kept in a short-lived cookie on the
//! app's origin). The pages site, once the person is signed in there and may read the app, sends them back to that
//! app's own host (worked out from the app's name, never taken from the request: no open redirect) with a code that is
//! single-use, under a minute old and bound to that host and `state`. The app origin exchanges it for its own
//! `__Host-` cookie. No app sees the pages site's cookie, and a sibling app can't set or overwrite another's.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::Request;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::auth::{self, Viewer};
use crate::render::{self, Problem};
use crate::space::{self, SpaceConfig};
use crate::web::{self, App, esc};

const CODE_TTL: Duration = Duration::from_secs(60);
const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);
/// Cookie names and attributes. Over HTTPS: `__Host-` (host-only, Secure, Path=/, so a sibling app can't set or
/// overwrite them). Over plain HTTP (a laptop's `*.localhost`), browsers refuse Secure cookies on subdomains, so the
/// names drop the prefix and the attribute; that's only for loopback setups, where nothing crosses a network.
struct Cookies {
    state: &'static str,
    session: &'static str,
    attrs: &'static str,
}

fn cookies(secure: bool) -> Cookies {
    if secure {
        Cookies { state: "__Host-fl_state", session: "__Host-fl_app", attrs: "Secure; HttpOnly; SameSite=Lax; Path=/" }
    } else {
        Cookies { state: "fl_state", session: "fl_app", attrs: "HttpOnly; SameSite=Lax; Path=/" }
    }
}

/// Folders and files never served from an app, whatever its build leaves behind.
const NEVER_SERVED: &[&str] = &["node_modules", "app.json", "migrations", "queries.sql"];

#[derive(Default)]
pub struct Broker {
    codes: Mutex<HashMap<String, Code>>,
    sessions: Mutex<HashMap<String, AppSession>>,
}

struct Code {
    host: String,
    state: String,
    viewer: Viewer,
    at: Instant,
}

struct AppSession {
    host: String,
    viewer: Viewer,
    at: Instant,
}

/// `app.json`. Every field is optional.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Manifest {
    pub name: Option<String>,
    pub description: Option<String>,
    /// An image in the app's folder, for the list and the home screen (a 512-pixel PNG works everywhere).
    pub icon: Option<String>,
    pub theme_color: Option<String>,
    /// The folder to serve, relative to the app's: `dist` by default when it exists, else the app's folder itself.
    pub serve: Option<String>,
    /// Origins the app's scripts may connect to (`https://api.example.com`). None by default.
    pub network: Vec<String>,
    /// Reload open copies of the app when its files change.
    pub live: bool,
}

/// An app as found on disk.
#[derive(Debug, Clone)]
pub struct Info {
    pub space: String,
    pub name: String,
    /// The folder served.
    pub root: PathBuf,
    pub manifest: Manifest,
    /// Origins allowed after the space's ceiling.
    pub network: Vec<String>,
    pub problems: Vec<Problem>,
}

impl Info {
    pub fn title(&self) -> String {
        self.manifest.name.clone().unwrap_or_else(|| self.name.clone())
    }
}

/// An app or space name: lower case letters, digits and single hyphens (`--` separates them in the host name).
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
}

pub fn host_for(space: &str, name: &str, domain: &str) -> String {
    format!("{name}--{space}.{domain}")
}

/// The app a host name belongs to: `(space, name)`.
pub fn parse_host(host: &str, domain: &str) -> Option<(String, String)> {
    let host = host.split(':').next()?.to_ascii_lowercase();
    let label = host.strip_suffix(domain)?.strip_suffix('.')?;
    if label.contains('.') {
        return None;
    }
    let (name, space) = label.split_once("--")?;
    (valid_name(name) && valid_name(space)).then(|| (space.to_string(), name.to_string()))
}

/// A `network` entry: an https origin (scheme and host, optional port), nothing else.
fn valid_origin(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("https://") else { return false };
    !rest.is_empty()
        && rest.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
        && !rest.starts_with('.')
        && rest.contains('.')
}

/// The app's folder, if it exists in the space's apps folder.
fn app_dir(cfg: &SpaceConfig, name: &str) -> Option<PathBuf> {
    let apps = cfg.apps.as_ref()?;
    if !valid_name(name) {
        return None;
    }
    let d = apps.join(name);
    d.is_dir().then_some(d)
}

/// Reads an app's folder: its manifest, what's served, and what's wrong.
pub fn inspect(space: &str, cfg: &SpaceConfig, name: &str) -> Option<Info> {
    let dir = app_dir(cfg, name)?;
    let mut problems = vec![];
    let manifest = match std::fs::read_to_string(dir.join("app.json")) {
        Ok(s) => match serde_json::from_str::<Manifest>(&s) {
            Ok(m) => m,
            Err(e) => {
                problems.push(Problem::new(format!("app.json can't be read: {e}.")));
                Manifest::default()
            }
        },
        Err(_) => Manifest::default(),
    };
    let root = match &manifest.serve {
        Some(s) => {
            let p = Path::new(s);
            if render::servable(p) && !p.components().any(|c| c.as_os_str() == "node_modules") {
                dir.join(p)
            } else {
                problems.push(Problem::new(format!("app.json's serve folder {s} isn't allowed.")));
                dir.clone()
            }
        }
        None if dir.join("dist").is_dir() => dir.join("dist"),
        None => dir.clone(),
    };
    let index = std::fs::read_to_string(root.join("index.html"));
    match &index {
        Ok(html) => problems.extend(check_html(html)),
        Err(_) => problems.push(Problem::new("There's no index.html in the folder the app serves.")),
    }
    let mut network = vec![];
    for o in &manifest.network {
        let o = o.trim_end_matches('/');
        if !valid_origin(o) {
            problems.push(Problem::new(format!(
                "app.json's network entry {o} isn't an https origin (https://host or https://host:port)."
            )));
        } else if cfg.untrusted {
            problems.push(Problem::new(format!(
                "This space's apps can't reach other sites, so {o} is blocked: network entries are refused while the space is untrusted."
            )));
        } else {
            network.push(o.to_string());
        }
    }
    if let Some(icon) = &manifest.icon
        && (!render::servable(Path::new(icon)) || !root.join(icon).is_file())
    {
        problems.push(Problem::new(format!("app.json's icon {icon} isn't in the folder the app serves.")));
    }
    Some(Info { space: space.to_string(), name: name.to_string(), root, manifest, network, problems })
}

/// The apps in a space, by name.
pub fn list(space: &str, cfg: &SpaceConfig) -> Vec<Info> {
    let Some(apps) = &cfg.apps else { return vec![] };
    let Ok(rd) = std::fs::read_dir(apps) else { return vec![] };
    let mut out: Vec<Info> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| inspect(space, cfg, &e.file_name().to_string_lossy()))
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// What an app's HTML does that its policy will block: inline scripts and files from other sites.
fn check_html(html: &str) -> Vec<Problem> {
    let mut problems = vec![];
    let lower = html.to_ascii_lowercase();
    let mut from = 0;
    let mut inline = false;
    while let Some(p) = lower[from..].find("<script") {
        let at = from + p;
        let Some(gt) = lower[at..].find('>').map(|g| at + g) else { break };
        let tag = &lower[at..gt];
        let close = lower[gt..].find("</script").map_or(lower.len(), |c| gt + c);
        let data_block = tag.contains("application/json") || tag.contains("application/ld+json");
        if !tag.contains("src=") && !data_block && !lower[gt + 1..close].trim().is_empty() {
            inline = true;
        }
        from = close.max(gt + 1);
    }
    if inline {
        problems.push(Problem::new(
            "index.html has an inline <script>, which this app's policy blocks: move it into a .js file.",
        ));
    }
    if crate::render::has_event_handler(&lower) {
        problems.push(Problem::new(
            "index.html has an on…= attribute, which this app's policy blocks: use addEventListener in a .js file.",
        ));
    }
    for r in render::references(html) {
        let l = r.to_ascii_lowercase();
        if (l.starts_with("https://") || l.starts_with("http://") || l.starts_with("//")) && render::is_asset(&l) {
            problems.push(Problem::new(format!(
                "index.html loads {r} from another site, which this app's policy blocks: bundle or copy it into the app."
            )));
        }
    }
    problems
}

/// The app origin's Content-Security-Policy: everything from itself, scripts only from files (no inline, no eval),
/// connections only to itself and the origins allowed.
pub fn csp(info: &Info) -> String {
    let mut connect = String::from("'self'");
    for o in &info.network {
        connect.push(' ');
        connect.push_str(o);
    }
    format!(
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; \
        font-src 'self' data:; media-src 'self' blob:; connect-src {connect}; worker-src 'self'; \
        manifest-src 'self'; object-src 'none'; frame-src 'self'; frame-ancestors 'none'; form-action 'self'; \
        base-uri 'self'"
    )
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let c = headers.get(header::COOKIE)?.to_str().ok()?;
    c.split(';').find_map(|p| p.trim().strip_prefix(&format!("{name}=")).map(str::to_string))
}

fn query_param(req: &Parts, key: &str) -> Option<String> {
    let q = req.uri.query()?;
    form_urlencoded::parse(q.as_bytes()).find(|(k, _)| k == key).map(|(_, v)| v.into_owned())
}

fn hex_encode(s: &str) -> String {
    hex::encode(s.as_bytes())
}

fn hex_decode(s: &str) -> Option<String> {
    String::from_utf8(hex::decode(s).ok()?).ok()
}

/// A path to come back to after signing in: only a local path.
fn safe_path(p: &str) -> String {
    if p.starts_with('/') && !p.starts_with("//") && !p.starts_with("/\\") && !p.contains(['\r', '\n']) {
        p.to_string()
    } else {
        "/".into()
    }
}

impl Broker {
    /// Issues a code for an app host, once the pages site has decided the viewer may read the app.
    pub async fn issue(&self, host: &str, state: &str, viewer: Viewer) -> String {
        let code = auth::new_id();
        let mut codes = self.codes.lock().await;
        codes.retain(|_, c| c.at.elapsed() < CODE_TTL);
        codes.insert(code.clone(), Code { host: host.into(), state: state.into(), viewer, at: Instant::now() });
        code
    }

    /// Exchanges a code: single use, under a minute old, for this exact host and state.
    async fn redeem(&self, code: &str, host: &str, state: &str) -> Option<Viewer> {
        let c = self.codes.lock().await.remove(code)?;
        (c.at.elapsed() < CODE_TTL && c.host == host && c.state == state).then_some(c.viewer)
    }

    async fn start(&self, host: &str, viewer: Viewer) -> String {
        let sid = auth::new_id();
        let mut s = self.sessions.lock().await;
        s.retain(|_, x| x.at.elapsed() < SESSION_TTL);
        s.insert(sid.clone(), AppSession { host: host.into(), viewer, at: Instant::now() });
        sid
    }

    /// End the app sessions, and codes not yet used, of someone a back-channel logout signs out; how many ended.
    pub async fn end_signed_out(&self, out: &fleetlayer_oidc::SignedOut) -> usize {
        let ends = |v: &Viewer| v.person.as_ref().is_some_and(|p| out.ends(p));
        self.codes.lock().await.retain(|_, c| !ends(&c.viewer));
        let mut s = self.sessions.lock().await;
        let before = s.len();
        s.retain(|_, x| !ends(&x.viewer));
        before - s.len()
    }

    async fn session(&self, headers: &HeaderMap, host: &str, secure: bool) -> Option<Viewer> {
        let sid = cookie_value(headers, cookies(secure).session)?;
        let s = self.sessions.lock().await;
        let x = s.get(&sid)?;
        (x.host == host && x.at.elapsed() < SESSION_TTL).then(|| x.viewer.clone())
    }
}

fn with_headers(mut res: Response, csp: &str) -> Response {
    let h = res.headers_mut();
    if let Ok(v) = HeaderValue::from_str(csp) {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::HeaderName::from_static("cross-origin-opener-policy"), HeaderValue::from_static("same-origin"));
    if !h.contains_key(header::CACHE_CONTROL) {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    res
}

fn plain(status: StatusCode, msg: &str) -> Response {
    with_headers(
        (status, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], msg.to_string()).into_response(),
        "default-src 'none'",
    )
}

/// Everything asked of an app's host.
pub async fn handle(app: Arc<App>, host: String, space: String, name: String, req: Request) -> Response {
    let Some(cfg) = app.spaces.get(&space) else { return plain(StatusCode::NOT_FOUND, "There's no app here.") };
    let Some(info) = inspect(&space, cfg, &name) else { return plain(StatusCode::NOT_FOUND, "There's no app here.") };
    let (req, _body) = req.into_parts();
    let path = req.uri.path().to_string();
    let csp = csp(&info);

    if path == "/_fl/callback" {
        return callback(&app, &host, &req).await;
    }
    let Some(viewer) = app.broker.session(&req.headers, &host, app.secure_cookies()).await else {
        return send_to_sign_in(&app, &space, &name, &req);
    };
    // The access may have been withdrawn since the session started.
    match app.auth.may(&viewer, &format!("app:{space}/{name}")).await {
        crate::auth::Access::Yes => {}
        crate::auth::Access::No => return plain(StatusCode::FORBIDDEN, "You can't open this app any more."),
        crate::auth::Access::CantTell(_) => {
            return plain(StatusCode::SERVICE_UNAVAILABLE, "Can't check your access right now: try again in a minute.");
        }
    }
    match path.as_str() {
        "/_fl/me" => {
            let body = serde_json::json!({"principal": viewer.principal(), "app": name, "space": space});
            return with_headers(
                ([(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response(),
                &csp,
            );
        }
        "/_fl/manifest.webmanifest" => return with_headers(manifest(&info), &csp),
        "/_fl/icon.svg" => {
            return with_headers(
                ([(header::CONTENT_TYPE, "image/svg+xml")], default_icon(&info.title())).into_response(),
                &csp,
            );
        }
        "/_fl/live.js" => {
            return with_headers(
                ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], LIVE_JS).into_response(),
                &csp,
            );
        }
        "/_fl/events" => {
            let root = info.root.clone();
            return with_headers(
                Sse::new(web::watch(root, Duration::from_secs(1)))
                    .keep_alive(KeepAlive::new().interval(Duration::from_secs(20)))
                    .into_response(),
                &csp,
            );
        }
        p if p.starts_with("/_fl/") => return plain(StatusCode::NOT_FOUND, "Not found."),
        _ => {}
    }
    with_headers(serve_file(&info, &path).await, &csp)
}

fn send_to_sign_in(app: &App, space: &str, name: &str, req: &Parts) -> Response {
    let Some(base) = &app.base_url else {
        return plain(StatusCode::SERVICE_UNAVAILABLE, "Apps need base_url in the pages config.");
    };
    let c = cookies(app.secure_cookies());
    let state = auth::new_id();
    let back = safe_path(req.uri.path_and_query().map_or("/", |p| p.as_str()));
    let url = format!("{base}/_/apps/authorize?app={space}/{name}&state={state}", base = base.trim_end_matches('/'),);
    let cookie = format!("{}={state}.{}; {}; Max-Age=600", c.state, hex_encode(&back), c.attrs);
    with_headers(([(header::SET_COOKIE, cookie)], Redirect::to(&url)).into_response(), "default-src 'none'")
}

async fn callback(app: &App, host: &str, req: &Parts) -> Response {
    let (Some(code), Some(state)) = (query_param(req, "code"), query_param(req, "state")) else {
        return plain(StatusCode::BAD_REQUEST, "The sign-in link is incomplete.");
    };
    let c = cookies(app.secure_cookies());
    let Some((kept, back)) = cookie_value(&req.headers, c.state).and_then(|v| {
        let (s, b) = v.split_once('.')?;
        Some((s.to_string(), hex_decode(b)?))
    }) else {
        return plain(StatusCode::BAD_REQUEST, "The sign-in took too long or started elsewhere: open the app again.");
    };
    if kept != state {
        return plain(StatusCode::BAD_REQUEST, "The sign-in doesn't match this browser: open the app again.");
    }
    let Some(viewer) = app.broker.redeem(&code, host, &state).await else {
        return plain(StatusCode::BAD_REQUEST, "The sign-in link has expired or was already used: open the app again.");
    };
    let sid = app.broker.start(host, viewer).await;
    let mut res = Redirect::to(&safe_path(&back)).into_response();
    let h = res.headers_mut();
    for c in [
        format!("{}={sid}; {}; Max-Age={}", c.session, c.attrs, SESSION_TTL.as_secs()),
        format!("{}=; {}; Max-Age=0", c.state, c.attrs),
    ] {
        if let Ok(v) = HeaderValue::from_str(&c) {
            h.append(header::SET_COOKIE, v);
        }
    }
    with_headers(res, "default-src 'none'")
}

fn hidden(rel: &Path) -> bool {
    !render::servable(rel) || rel.components().any(|c| NEVER_SERVED.iter().any(|n| c.as_os_str() == *n))
}

async fn serve_file(info: &Info, path: &str) -> Response {
    let rel = path.trim_start_matches('/');
    let decoded: String = form_urlencoded::parse(format!("x={}", rel.replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default();
    let rel = if decoded.is_empty() || decoded.ends_with('/') { format!("{decoded}index.html") } else { decoded };
    let rel_path = Path::new(&rel);
    let mut file = if hidden(rel_path) { None } else { space::resolve(&info.root, &rel) };
    // A single-page app's own routes (`/settings`) get its index.html.
    if file.is_none() && rel_path.extension().is_none() && !hidden(rel_path) {
        file = space::resolve(&info.root, "index.html");
    }
    let Some(file) = file.filter(|f| f.is_file()) else {
        return (StatusCode::NOT_FOUND, "Not found.").into_response();
    };
    let kind = web::content_type(&file);
    let Ok(bytes) = tokio::fs::read(&file).await else {
        return (StatusCode::NOT_FOUND, "Not found.").into_response();
    };
    let body = if kind.starts_with("text/html") {
        Body::from(inject(&String::from_utf8_lossy(&bytes), info))
    } else {
        Body::from(bytes)
    };
    let mut res = (StatusCode::OK, body).into_response();
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(kind));
    if kind == "application/octet-stream" {
        res.headers_mut().insert(header::CONTENT_DISPOSITION, HeaderValue::from_static("attachment"));
    }
    res
}

/// Adds the home-screen manifest and icon to an HTML page that doesn't name its own, and the reload script when the
/// app asked for it.
fn inject(html: &str, info: &Info) -> String {
    let lower = html.to_ascii_lowercase();
    let mut tags = String::new();
    if !lower.contains("rel=\"manifest\"") && !lower.contains("rel='manifest'") && !lower.contains("rel=manifest") {
        tags.push_str(r#"<link rel="manifest" href="/_fl/manifest.webmanifest">"#);
    }
    if !lower.contains("apple-touch-icon") {
        let icon = info.manifest.icon.as_deref().map_or("/_fl/icon.svg".to_string(), |i| format!("/{i}"));
        tags.push_str(&format!(r#"<link rel="apple-touch-icon" href="{}">"#, esc(&icon)));
        tags.push_str(r#"<meta name="apple-mobile-web-app-capable" content="yes">"#);
    }
    if info.manifest.live {
        tags.push_str(r#"<script src="/_fl/live.js" defer></script>"#);
    }
    if tags.is_empty() {
        return html.to_string();
    }
    match lower.find("<head").and_then(|h| lower[h..].find('>').map(|g| h + g + 1)) {
        Some(at) => format!("{}{tags}{}", &html[..at], &html[at..]),
        None => format!("{tags}{html}"),
    }
}

fn manifest(info: &Info) -> Response {
    let icon = match &info.manifest.icon {
        Some(i) => serde_json::json!({"src": format!("/{i}"), "sizes": "512x512", "purpose": "any"}),
        None => serde_json::json!({"src": "/_fl/icon.svg", "sizes": "any", "type": "image/svg+xml"}),
    };
    let body = serde_json::json!({
        "name": info.title(),
        "short_name": info.title(),
        "description": info.manifest.description,
        "start_url": "/",
        "scope": "/",
        "display": "standalone",
        "theme_color": info.manifest.theme_color.clone().unwrap_or_else(|| "#3b4fd6".into()),
        "background_color": "#ffffff",
        "icons": [icon],
    });
    ([(header::CONTENT_TYPE, "application/manifest+json")], body.to_string()).into_response()
}

/// An icon for an app with none: its first letter on the accent colour.
pub(crate) fn default_icon(title: &str) -> String {
    let letter = title.chars().find(|c| c.is_alphanumeric()).map_or('A', |c| c.to_ascii_uppercase());
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512"><rect width="512" height="512" rx="112" fill="#3b4fd6"/><text x="256" y="340" font-family="system-ui, sans-serif" font-size="260" font-weight="700" text-anchor="middle" fill="#fff">{}</text></svg>"##,
        esc(&letter.to_string())
    )
}

const LIVE_JS: &str = r#""use strict";
(() => {
  let backoff = 1000;
  const connect = () => {
    const es = new EventSource("/_fl/events");
    es.addEventListener("ready", () => { backoff = 1000; });
    es.addEventListener("change", () => location.reload());
    es.onerror = () => { es.close(); setTimeout(connect, backoff); backoff = Math.min(backoff * 2, 30000); };
  };
  connect();
})();
"#;

// ------------------------------------------------------------ the pages site's side

/// The notice shown before opening an app from a space whose agent reads outside input.
pub fn notice_html(info: &Info, state: &str) -> String {
    let reach = if info.network.is_empty() {
        "It can't reach any other site.".to_string()
    } else {
        format!("It can reach: {}.", info.network.iter().map(|o| esc(o)).collect::<Vec<_>>().join(", "))
    };
    format!(
        r#"<main class="wrap"><div class="notice"><h1>Open {title}?</h1><p>This app was made by the agent in the <strong>{space}</strong> space, which reads messages from outside (email, the web). Treat what it shows as you'd treat those messages.</p><p>{reach}</p><form method="post" action="/_/apps/authorize"><input type="hidden" name="app" value="{space}/{name}"><input type="hidden" name="state" value="{state}"><div class="notice-actions"><button class="btn btn-primary" type="submit">Open the app</button><a class="btn btn-ghost" href="/">Back</a></div></form></div></main>"#,
        title = esc(&info.title()),
        space = esc(&info.space),
        name = esc(&info.name),
        state = esc(state),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_back_channel_logout_ends_that_sign_in_sessions_app_sessions_and_codes() {
        let viewer = |sid: Option<&str>| Viewer {
            person: Some(fleetlayer_oidc::Person {
                sub: "p_1".into(),
                tenant: "home".into(),
                handle: None,
                name: None,
                sid: sid.map(String::from),
            }),
        };
        let b = Broker::default();
        let host = "recipes--personal.apps.example";
        let laptop = b.start(host, viewer(Some("s-laptop"))).await;
        let phone = b.start(host, viewer(Some("s-phone"))).await;
        b.start(host, Viewer { person: None }).await;
        let code = b.issue(host, "st", viewer(Some("s-laptop"))).await;
        let cookie = |sid: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::COOKIE, HeaderValue::from_str(&format!("{}={sid}", cookies(true).session)).unwrap());
            h
        };
        assert!(b.session(&cookie(&laptop), host, true).await.is_some());

        let out = fleetlayer_oidc::SignedOut { sub: Some("p_1".into()), sid: Some("s-laptop".into()) };
        assert_eq!(b.end_signed_out(&out).await, 1);
        assert!(b.session(&cookie(&laptop), host, true).await.is_none(), "the laptop's app session ended");
        assert!(b.session(&cookie(&phone), host, true).await.is_some(), "the phone's didn't");
        assert!(b.redeem(&code, host, "st").await.is_none(), "nor can its unused code be redeemed");
        assert_eq!(b.sessions.lock().await.len(), 2, "the login key's session stays");
    }

    #[test]
    fn host_names_map_to_apps() {
        let d = "apps.home.example";
        assert_eq!(parse_host("recipes--personal.apps.home.example", d), Some(("personal".into(), "recipes".into())));
        assert_eq!(
            parse_host("Recipes--Personal.apps.home.example:443", d),
            Some(("personal".into(), "recipes".into()))
        );
        assert_eq!(parse_host("a.b--c.apps.home.example", d), None);
        assert_eq!(parse_host("recipes.apps.home.example", d), None);
        assert_eq!(parse_host("pages.home.example", d), None);
        assert_eq!(parse_host("x--y.apps.home.example.evil.com", d), None);
        assert_eq!(parse_host("evilapps.home.example", d), None);
        assert_eq!(host_for("personal", "recipes", d), "recipes--personal.apps.home.example");
    }

    #[test]
    fn names_have_no_double_hyphen() {
        assert!(valid_name("my-app"));
        assert!(!valid_name("my--app"));
        assert!(!valid_name("-a"));
        assert!(!valid_name("App"));
    }

    #[test]
    fn only_https_origins_are_network_entries() {
        assert!(valid_origin("https://api.example.com"));
        assert!(valid_origin("https://api.example.com:8443"));
        assert!(!valid_origin("http://api.example.com"));
        assert!(!valid_origin("https://*.example.com"));
        assert!(!valid_origin("https://api.example.com/path"));
        assert!(!valid_origin("https://a.com 'unsafe-inline'"));
        assert!(!valid_origin("https://localhost"));
    }

    #[test]
    fn inline_scripts_and_outside_files_are_problems() {
        let p = check_html(
            r#"<script src="app.js"></script><script type="application/json">{"a":1}</script><script>go()</script>
            <script src="https://cdn.example/x.js"></script><button onclick="x()">"#,
        );
        let m: Vec<_> = p.iter().map(|p| p.message.as_str()).collect();
        assert!(m.iter().any(|p| p.contains("inline <script>")), "{m:?}");
        assert!(m.iter().any(|p| p.contains("cdn.example")), "{m:?}");
        assert!(m.iter().any(|p| p.contains("on…=")), "{m:?}");
        assert!(check_html(r#"<script src="app.js"></script><script type="module" src="m.js"></script>"#).is_empty());
    }

    #[test]
    fn the_manifest_and_icon_are_added_once() {
        let info = Info {
            space: "s".into(),
            name: "a".into(),
            root: PathBuf::new(),
            manifest: Manifest { live: true, ..Default::default() },
            network: vec![],
            problems: vec![],
        };
        let out = inject("<html><head><title>x</title></head><body></body></html>", &info);
        assert!(out.starts_with(r#"<html><head><link rel="manifest""#), "{out}");
        assert!(out.contains("apple-touch-icon") && out.contains("/_fl/live.js"));
        let own = r#"<head><link rel="manifest" href="/m.json"><link rel="apple-touch-icon" href="/i.png"></head>"#;
        let info2 = Info { manifest: Manifest::default(), ..info };
        assert_eq!(inject(own, &info2), own);
    }

    #[test]
    fn the_policy_allows_only_listed_origins() {
        let info = Info {
            space: "s".into(),
            name: "a".into(),
            root: PathBuf::new(),
            manifest: Manifest::default(),
            network: vec!["https://api.example.com".into()],
            problems: vec![],
        };
        let c = csp(&info);
        assert!(c.contains("connect-src 'self' https://api.example.com;"), "{c}");
        assert!(c.contains("script-src 'self';"));
        assert!(!c.contains("unsafe-eval"));
    }

    #[test]
    fn return_paths_are_local() {
        assert_eq!(safe_path("/settings?x=1"), "/settings?x=1");
        assert_eq!(safe_path("//evil.example"), "/");
        assert_eq!(safe_path("https://evil.example"), "/");
    }
}
