//! The status page (`fleetlayer.<domain>`, and the bare domain): what's running and where, each check's state, and
//! where alerts go, with the routes and settings under Advanced. Also `/api/status` (JSON), the heartbeat endpoint
//! (`POST /api/v1/endpoints/<key>/external?success=…&error=…`, Gatus's own path) and the gateway's `/healthz`.

use std::collections::HashMap;

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use serde_json::json;

use crate::monitor::{Check, Kind};
use crate::registry::Route;
use crate::{App, html_escape as esc};

pub async fn api_status(app: &App) -> Response {
    let routes = app.reg.routes().await;
    let s = app.monitor.state.lock().await;
    let checks: Vec<_> =
        s.checks.values().map(|(kind, name, c)| json!({"kind": kind, "name": name, "check": c})).collect();
    let services: Vec<_> = routes
        .iter()
        .map(|r| {
            json!({"name": r.name, "address": app.reg.cfg.address(&r.name), "backend": r.url,
            "health": r.health, "hosts": r.hosts, "source": r.source})
        })
        .collect();
    axum::Json(json!({"services": services, "checks": checks, "queuedAlerts": s.queued.len()})).into_response()
}

pub async fn healthz(app: &App) -> Response {
    // The gateway is healthy when it can answer; say whether it can read the node, for whoever looks.
    let node = app.reg.node_ok.read().await.clone();
    let body = json!({"ok": true, "node": match node { Some(Ok(())) => json!("ok"), Some(Err(e)) => json!(e), None => json!(null) }});
    (StatusCode::OK, axum::Json(body)).into_response()
}

pub async fn heartbeat(app: &App, key: &str, query: &HashMap<String, String>, headers: &HeaderMap) -> Response {
    // Gatus keys are `<group>_<name>`; take the config's name either way.
    let name = if app.reg.cfg.heartbeats.contains_key(key) {
        key.to_string()
    } else {
        match key.rsplit_once('_') {
            Some((_, n)) if app.reg.cfg.heartbeats.contains_key(n) => n.to_string(),
            _ => return (StatusCode::NOT_FOUND, "no heartbeat by that key in gateway.yaml\n").into_response(),
        }
    };
    let hb = &app.reg.cfg.heartbeats[&name];
    if let Some(file) = &hb.token_file {
        let want = std::fs::read_to_string(crate::config::expand(file)).unwrap_or_default();
        let got =
            headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
        if want.trim().is_empty() || !got.is_some_and(|g| constant_eq(g.as_bytes(), want.trim().as_bytes())) {
            return (StatusCode::UNAUTHORIZED, "wrong or missing bearer token\n").into_response();
        }
    }
    let Some(success) = query.get("success").map(|s| s == "true") else {
        return (StatusCode::BAD_REQUEST, "success=true|false is required\n").into_response();
    };
    app.monitor.push(&name, success, query.get("error").cloned()).await;
    StatusCode::OK.into_response()
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn state_cell(c: Option<&Check>) -> String {
    match c.and_then(|c| c.up) {
        Some(true) => "<span class=up>up</span>".into(),
        Some(false) => format!(
            "<span class=down>down</span> <small>{}</small>",
            esc(c.and_then(|c| c.last_error.as_deref()).unwrap_or(""))
        ),
        None => "<span class=wait>checking</span>".into(),
    }
}

fn ago(t: Option<jiff::Timestamp>) -> String {
    match t {
        None => "never".into(),
        Some(t) => {
            let s = jiff::Timestamp::now().duration_since(t).as_secs().max(0);
            match s {
                0..60 => format!("{s} s ago"),
                60..3600 => format!("{} min ago", s / 60),
                _ => format!("{} h ago", s / 3600),
            }
        }
    }
}

pub async fn page(app: &App, unknown: Option<&str>) -> Response {
    let cfg = &app.reg.cfg;
    let routes: Vec<Route> = app.reg.routes().await;
    let s = app.monitor.state.lock().await;
    let find = |kind: Kind, name: &str| s.checks.values().find(|(k, n, _)| *k == kind && n == name).map(|(_, _, c)| c);

    let mut rows = String::new();
    for r in &routes {
        let c = find(Kind::Service, &r.name);
        let addr = cfg.address(&r.name);
        let state = if r.health.is_some() { state_cell(c) } else { "<small>no health check</small>".into() };
        rows.push_str(&format!(
            "<tr><td><a href=\"{a}/\">{n}</a><br><small>{a}</small></td><td>{state}</td><td>{last}</td></tr>",
            a = esc(&addr),
            n = esc(&r.name),
            last = ago(c.and_then(|c| c.last_ok)),
        ));
    }
    // Services that were here and aren't now.
    for (kind, name, c) in s.checks.values() {
        if *kind == Kind::Service && !routes.iter().any(|r| &r.name == name) {
            rows.push_str(&format!(
                "<tr><td>{n}<br><small>gone from the registry</small></td><td>{st}</td><td>{last}</td></tr>",
                n = esc(name),
                st = state_cell(Some(c)),
                last = ago(c.last_ok)
            ));
        }
    }
    let mut others = String::new();
    if cfg.node {
        let c = find(Kind::Node, "node");
        others.push_str(&format!(
            "<tr><td>The node</td><td>{}</td><td>{}</td></tr>",
            state_cell(c),
            ago(c.and_then(|c| c.last_ok))
        ));
    }
    for name in cfg.heartbeats.keys() {
        let c = find(Kind::Heartbeat, name);
        others.push_str(&format!(
            "<tr><td>Heartbeat {}</td><td>{}</td><td>{}</td></tr>",
            esc(name),
            state_cell(c),
            ago(c.and_then(|c| c.last_push))
        ));
    }
    let alerts = match (&cfg.alerts.session, cfg.alerts.desktop) {
        (Some(s), true) => format!("a desktop notification and a message to <b>{}</b>", esc(s)),
        (Some(s), false) => format!("a message to <b>{}</b>", esc(s)),
        (None, true) => {
            "a desktop notification. Set <code>alerts.session</code> in gateway.yaml so an agent hears about it too"
                .into()
        }
        (None, false) => "<b>nowhere</b>: set <code>alerts.session</code> in gateway.yaml".into(),
    };
    let queued = if s.queued.is_empty() {
        String::new()
    } else {
        format!("<p class=note>{} alert message(s) wait for the node to come back.</p>", s.queued.len())
    };
    let missing = unknown
        .map_or(String::new(), |n| format!("<p class=note>Nothing called <b>{}</b> runs here. These do:</p>", esc(n)));
    let mut advanced = String::new();
    for r in &routes {
        advanced.push_str(&format!(
            "<tr><td>{}</td><td><code>{}</code></td><td>{}</td><td>{}</td></tr>",
            esc(&r.name),
            esc(&r.url),
            esc(&r.hosts.iter().map(|h| format!("{h}.{}", cfg.domain)).collect::<Vec<_>>().join(", ")),
            esc(&r.source),
        ));
    }
    let body = format!(
        r#"<!doctype html><html lang=en><head><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1">
<title>Fleetlayer on this machine</title><meta http-equiv=refresh content=15><style>{CSS}</style></head><body><main>
<h1>Fleetlayer is running on this machine</h1>
{missing}
<table><thead><tr><th>Service</th><th>State</th><th>Last answered</th></tr></thead><tbody>{rows}</tbody></table>
<table><tbody>{others}</tbody></table>
<p>When something goes down or comes back: {alerts}.</p>{queued}
<p>Moving a piece to a server: see the guide “Start on your laptop” in Fleetlayer's docs.</p>
<details><summary>Advanced</summary>
<p>Gateway on <code>{listen}</code>, services at <code>&lt;name&gt;.{domain}</code>. Settings: <code>{cfgpath}</code>. State: <code>{state}</code>.</p>
<table><thead><tr><th>Name</th><th>Backend</th><th>Other hosts</th><th>From</th></tr></thead><tbody>{advanced}</tbody></table>
<p>For a server's proxy and monitor: <code>fleetlayer-gateway emit traefik</code>, <code>fleetlayer-gateway emit gatus</code>. JSON: <a href="/api/status">/api/status</a>.</p>
</details></main></body></html>"#,
        listen = cfg.listen,
        domain = esc(&cfg.domain),
        cfgpath = esc(&app.config_path.display().to_string()),
        state = esc(&cfg.state_dir().display().to_string()),
    );
    let status = if unknown.is_some() { StatusCode::NOT_FOUND } else { StatusCode::OK };
    (status, [(header::CACHE_CONTROL, "no-store")], Html(body)).into_response()
}

/// A route whose backend doesn't answer: say so, with the way to the status page.
pub fn backend_down(app: &App, name: &str, why: &str) -> Response {
    let body = format!(
        r#"<!doctype html><html lang=en><head><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1">
<title>{n} isn't answering</title><style>{CSS}</style></head><body><main><h1>{n} isn't answering</h1>
<p><small>{why}</small></p><p>It may be starting or restarting. <a href="{status}/">What's running on this machine</a></p></main></body></html>"#,
        n = esc(name),
        why = esc(why),
        status = esc(&app.reg.cfg.address(crate::registry::STATUS)),
    );
    (StatusCode::BAD_GATEWAY, [(header::CACHE_CONTROL, "no-store")], Html(body)).into_response()
}

const CSS: &str = r#":root{color-scheme:light dark;--bg:#fafaf9;--fg:#1c1917;--mute:#78716c;--line:#e7e5e4;--up:#15803d;--down:#b91c1c}
@media (prefers-color-scheme:dark){:root{--bg:#1c1917;--fg:#f5f5f4;--mute:#a8a29e;--line:#44403c;--up:#4ade80;--down:#f87171}}
body{background:var(--bg);color:var(--fg);font:16px/1.5 system-ui,sans-serif;margin:0}main{max-width:52rem;margin:0 auto;padding:2rem 1rem}
h1{font-size:1.5rem}table{width:100%;border-collapse:collapse;margin:1rem 0}td,th{text-align:left;padding:.5rem;border-bottom:1px solid var(--line);vertical-align:top}
th{color:var(--mute);font-weight:500;font-size:.875rem}small{color:var(--mute)}a{color:inherit}.up{color:var(--up);font-weight:600}.down{color:var(--down);font-weight:600}
.wait{color:var(--mute)}.note{border-left:3px solid var(--line);padding-left:.75rem}details{margin-top:2rem}summary{cursor:pointer;color:var(--mute)}code{font-size:.875em}"#;
