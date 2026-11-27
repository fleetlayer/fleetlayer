//! The same registry as config for a server's own proxy and monitor: a Traefik file-provider file and Gatus
//! endpoints. Moving a service is then repointing its route (one `url:` line, or the new machine's announcement) and
//! emitting again; `--watch` keeps the file current for Traefik's file watcher as announced addresses change.

use serde_json::{Value, json};

use crate::registry::Route;

#[derive(Debug, Clone, Default)]
pub struct TraefikOptions {
    pub domain: String,
    pub entrypoints: Vec<String>,
    pub cert_resolver: Option<String>,
    pub middlewares: Vec<String>,
}

/// `*--*.apps` under `home.example` → a Traefik v3 `HostRegexp` regular expression.
fn pattern_regex(pattern: &str, domain: &str) -> String {
    let mut out = String::from("^");
    for c in pattern.chars() {
        match c {
            '*' => out.push_str("[a-z0-9-]+"),
            '.' => out.push_str("\\."),
            c => out.push(c),
        }
    }
    out.push_str("\\.");
    out.push_str(&domain.replace('.', "\\."));
    out.push('$');
    out
}

pub fn traefik(routes: &[Route], o: &TraefikOptions) -> String {
    let mut routers = serde_json::Map::new();
    let mut services = serde_json::Map::new();
    for r in routes {
        let id = format!("fleetlayer-{}", r.name);
        let mut rule = format!("Host(`{}.{}`)", r.name, o.domain);
        for h in &r.hosts {
            rule.push_str(&format!(" || HostRegexp(`{}`)", pattern_regex(h, &o.domain)));
        }
        let mut router = json!({"rule": rule, "service": id});
        if !o.entrypoints.is_empty() {
            router["entryPoints"] = json!(o.entrypoints);
        }
        if let Some(cr) = &o.cert_resolver {
            router["tls"] = json!({"certResolver": cr});
        }
        if !o.middlewares.is_empty() {
            router["middlewares"] = json!(o.middlewares);
        }
        routers.insert(id.clone(), router);
        services.insert(id, json!({"loadBalancer": {"servers": [{"url": r.url}]}}));
    }
    let doc = json!({"http": {"routers": routers, "services": services}});
    header("Traefik file provider") + &yaml_serde::to_string(&doc).expect("JSON values serialise")
}

#[derive(Debug, Clone, Default)]
pub struct GatusOptions {
    pub group: String,
    /// Alert types to attach to every endpoint (`matrix`, `slack`...), as configured in Gatus.
    pub alerts: Vec<String>,
}

/// Gatus endpoints for every route with a health path, and external endpoints for every heartbeat. Tokens are
/// `${…}` references for Gatus to fill from its environment, never values.
pub fn gatus(routes: &[Route], heartbeats: &[(String, u64)], o: &GatusOptions) -> String {
    let alerts: Vec<Value> = o.alerts.iter().map(|t| json!({"type": t})).collect();
    let mut endpoints = vec![];
    for r in routes {
        let Some(h) = &r.health else { continue };
        let mut e = json!({
            "name": r.name, "group": o.group,
            "url": format!("{}{}", r.url.trim_end_matches('/'), h),
            "interval": "60s", "conditions": ["[STATUS] == 200"],
        });
        if !alerts.is_empty() {
            e["alerts"] = json!(alerts);
        }
        endpoints.push(e);
    }
    let mut external = vec![];
    for (name, every) in heartbeats {
        let var = format!("FLEETLAYER_HEARTBEAT_{}_TOKEN", name.to_uppercase().replace('-', "_"));
        let mut e = json!({
            "name": name, "group": o.group, "token": format!("${{{var}}}"),
            "heartbeat": {"interval": format!("{}s", every.max(&10) * 2)},
        });
        if !alerts.is_empty() {
            e["alerts"] = json!(alerts);
        }
        external.push(e);
    }
    let doc = json!({"endpoints": endpoints, "external-endpoints": external});
    header("Gatus") + &yaml_serde::to_string(&doc).expect("JSON values serialise")
}

fn header(what: &str) -> String {
    format!(
        "# {what} config for Fleetlayer's services, written by fleetlayer-gateway emit. Edits here are overwritten.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routes() -> Vec<Route> {
        vec![
            Route {
                name: "pages".into(),
                url: "http://10.0.0.5:7670".into(),
                health: Some("/healthz".into()),
                hosts: vec!["*--*.apps".into()],
                source: "listed".into(),
            },
            Route {
                name: "control".into(),
                url: "http://127.0.0.1:7600".into(),
                health: None,
                hosts: vec![],
                source: "listed".into(),
            },
        ]
    }

    #[test]
    fn traefik_routes_and_wildcards() {
        let out = traefik(
            &routes(),
            &TraefikOptions {
                domain: "home.example".into(),
                entrypoints: vec!["websecure".into()],
                cert_resolver: Some("le".into()),
                middlewares: vec!["auth@file".into()],
            },
        );
        let v: Value = yaml_serde::from_str(&out).unwrap();
        let r = &v["http"]["routers"]["fleetlayer-pages"];
        assert_eq!(
            r["rule"],
            "Host(`pages.home.example`) || HostRegexp(`^[a-z0-9-]+--[a-z0-9-]+\\.apps\\.home\\.example$`)"
        );
        assert_eq!(r["tls"]["certResolver"], "le");
        assert_eq!(
            v["http"]["services"]["fleetlayer-pages"]["loadBalancer"]["servers"][0]["url"],
            "http://10.0.0.5:7670"
        );
        assert_eq!(v["http"]["routers"]["fleetlayer-control"]["middlewares"][0], "auth@file");
    }

    #[test]
    fn gatus_endpoints_and_heartbeats() {
        let out = gatus(
            &routes(),
            &[("node".into(), 60)],
            &GatusOptions { group: "fleetlayer".into(), alerts: vec!["matrix".into()] },
        );
        let v: Value = yaml_serde::from_str(&out).unwrap();
        assert_eq!(v["endpoints"].as_array().unwrap().len(), 1, "only routes with a health path");
        assert_eq!(v["endpoints"][0]["url"], "http://10.0.0.5:7670/healthz");
        assert_eq!(v["external-endpoints"][0]["token"], "${FLEETLAYER_HEARTBEAT_NODE_TOKEN}");
        assert_eq!(v["external-endpoints"][0]["heartbeat"]["interval"], "120s");
        assert_eq!(v["external-endpoints"][0]["alerts"][0]["type"], "matrix");
    }
}
