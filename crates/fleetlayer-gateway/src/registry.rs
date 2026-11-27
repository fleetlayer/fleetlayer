//! The registry of services: the ones the node's plugins announce (`_fleetlayer.dev/service/listening`, read here with
//! `node/services`) merged with the ones listed in the gateway's file. A listed service overrides an announced one of
//! the same name, field by field, which is how a move works: pages now on another machine is one `url:` line.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::config::{Config, ServiceConfig};

/// The name the status page answers on, and the one no service may take.
pub const STATUS: &str = "fleetlayer";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Announced {
    /// The plugin that announced it.
    pub plugin: String,
    pub name: String,
    pub url: String,
    pub health: Option<String>,
    #[serde(default)]
    pub hosts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Route {
    pub name: String,
    pub url: String,
    pub health: Option<String>,
    pub hosts: Vec<String>,
    /// `listed` (the gateway's file), or the plugin that announced it.
    pub source: String,
}

pub fn check_name(name: &str) -> anyhow::Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if !ok {
        anyhow::bail!("{name:?}: a service name is lower-case letters, digits and hyphens (one DNS label)");
    }
    if name == STATUS {
        anyhow::bail!("{name:?} is the status page's own name");
    }
    Ok(())
}

/// A host pattern under the domain: labels of letters, digits, hyphens and `*`, where `*` stands for one or more of
/// those characters inside one label (`*--*.apps`).
pub fn check_pattern(p: &str) -> anyhow::Result<()> {
    let ok = !p.is_empty()
        && p.split('.').all(|l| {
            !l.is_empty() && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'*')
        });
    if !ok {
        anyhow::bail!("{p:?}: a host pattern is dot-separated labels of a-z, 0-9, - and *");
    }
    Ok(())
}

/// `*` matches one or more label characters; everything else matches itself.
fn glob(pattern: &str, label: &str) -> bool {
    fn go(p: &[u8], s: &[u8]) -> bool {
        match p.split_first() {
            None => s.is_empty(),
            Some((b'*', rest)) => (1..=s.len()).any(|i| go(rest, &s[i..])),
            Some((c, rest)) => s.first() == Some(c) && go(rest, &s[1..]),
        }
    }
    go(pattern.as_bytes(), label.as_bytes())
}

fn pattern_matches(pattern: &str, sub: &str) -> bool {
    let p: Vec<&str> = pattern.split('.').collect();
    let s: Vec<&str> = sub.split('.').collect();
    p.len() == s.len() && p.iter().zip(&s).all(|(p, s)| glob(p, s))
}

/// What a request's Host header asks for.
#[derive(Debug, PartialEq)]
pub enum Target {
    Status,
    Route(Route),
    /// Under the domain, but no service by that name: the part before the domain.
    Unknown(String),
    /// Not under the domain at all.
    Foreign,
}

/// Merge announced and listed services into routes, by name.
pub fn merge(cfg: &Config, announced: &[Announced]) -> Vec<Route> {
    let mut out: BTreeMap<String, Route> = BTreeMap::new();
    for a in announced {
        if check_name(&a.name).is_err() || a.hosts.iter().any(|h| check_pattern(h).is_err()) {
            tracing::warn!(plugin = a.plugin, name = a.name, "ignoring an announced service with a bad name or host");
            continue;
        }
        out.insert(
            a.name.clone(),
            Route {
                name: a.name.clone(),
                url: a.url.clone(),
                health: a.health.clone(),
                hosts: a.hosts.clone(),
                source: a.plugin.clone(),
            },
        );
    }
    for (name, s) in &cfg.services {
        let ServiceConfig { url, health, hosts, hidden } = s;
        if *hidden {
            out.remove(name);
            continue;
        }
        match (out.get_mut(name), url) {
            (Some(r), _) => {
                if let Some(u) = url {
                    r.url = u.clone();
                    r.source = "listed".into();
                }
                if health.is_some() {
                    r.health = health.clone();
                }
                if !hosts.is_empty() {
                    r.hosts = hosts.clone();
                }
            }
            (None, Some(u)) => {
                out.insert(
                    name.clone(),
                    Route {
                        name: name.clone(),
                        url: u.clone(),
                        health: health.clone(),
                        hosts: hosts.clone(),
                        source: "listed".into(),
                    },
                );
            }
            // Listed with no URL and nothing announced (yet): no route.
            (None, None) => {}
        }
    }
    out.into_values().collect()
}

/// Which route a Host header (`pages.localhost:7680`) asks for.
pub fn resolve(routes: &[Route], domain: &str, host: &str) -> Target {
    let host = host.rsplit_once(':').filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())).map_or(host, |(h, _)| h);
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host == domain {
        return Target::Status;
    }
    let Some(sub) = host.strip_suffix(domain).and_then(|s| s.strip_suffix('.')) else {
        return Target::Foreign;
    };
    if sub == STATUS {
        return Target::Status;
    }
    if let Some(r) = routes.iter().find(|r| r.name == sub) {
        return Target::Route(r.clone());
    }
    if let Some(r) = routes.iter().find(|r| r.hosts.iter().any(|p| pattern_matches(p, sub))) {
        return Target::Route(r.clone());
    }
    Target::Unknown(sub.to_string())
}

/// The live registry: the config, and the announced services last read from the node (kept on disk too, so routes
/// keep working while the node restarts).
pub struct Registry {
    pub cfg: Config,
    announced: RwLock<Vec<Announced>>,
    /// Whether the last read of the node worked.
    pub node_ok: RwLock<Option<Result<(), String>>>,
}

impl Registry {
    pub fn new(cfg: Config) -> Arc<Registry> {
        let saved = std::fs::read(cfg.state_dir().join("announced.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Arc::new(Registry { cfg, announced: RwLock::new(saved), node_ok: RwLock::new(None) })
    }

    pub async fn routes(&self) -> Vec<Route> {
        merge(&self.cfg, &self.announced.read().await)
    }

    pub async fn set_announced(&self, list: Vec<Announced>) {
        let mut cur = self.announced.write().await;
        if *cur != list {
            let dir = self.cfg.state_dir();
            let _ = std::fs::create_dir_all(&dir);
            if let Ok(json) = serde_json::to_vec_pretty(&list) {
                let _ = std::fs::write(dir.join("announced.json"), json);
            }
            *cur = list;
        }
    }

    /// Keep the announced services fresh: ask the node every few seconds.
    pub async fn follow_node(self: Arc<Self>) {
        if !self.cfg.node {
            return;
        }
        loop {
            let res = crate::node::services(&self.cfg.node_socket()).await;
            match res {
                Ok(list) => {
                    self.set_announced(list).await;
                    *self.node_ok.write().await = Some(Ok(()));
                }
                Err(e) => *self.node_ok.write().await = Some(Err(e.to_string())),
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(yaml: &str) -> Config {
        let c: Config = yaml_serde::from_str(yaml).unwrap();
        c.check().unwrap();
        c
    }

    fn ann(name: &str, url: &str, hosts: &[&str]) -> Announced {
        Announced {
            plugin: name.into(),
            name: name.into(),
            url: url.into(),
            health: Some("/healthz".into()),
            hosts: hosts.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn listed_overrides_announced_field_by_field() {
        let c = cfg(
            "services:\n  control: { url: 'http://127.0.0.1:7600' }\n  pages: { url: 'http://10.0.0.5:7670' }\n  hub: { hidden: true }\n  later: { health: /x }\n",
        );
        let routes = merge(
            &c,
            &[ann("pages", "http://127.0.0.1:40001", &["*--*.apps"]), ann("hub", "http://127.0.0.1:40002", &[])],
        );
        let names: Vec<_> = routes.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["control", "pages"]);
        let pages = &routes[1];
        assert_eq!(pages.url, "http://10.0.0.5:7670");
        assert_eq!(pages.source, "listed");
        assert_eq!(pages.health.as_deref(), Some("/healthz"));
        assert_eq!(pages.hosts, ["*--*.apps"]);
    }

    #[test]
    fn hosts_resolve() {
        let c = cfg("{}");
        let routes = merge(&c, &[ann("pages", "http://p", &["*--*.apps"]), ann("hub", "http://h", &[])]);
        let name = |host: &str| match resolve(&routes, "localhost", host) {
            Target::Route(r) => r.name,
            other => format!("{other:?}"),
        };
        assert_eq!(name("hub.localhost:7680"), "hub");
        assert_eq!(name("HUB.localhost."), "hub");
        assert_eq!(name("recipes--personal.apps.localhost:7680"), "pages");
        assert_eq!(name("recipes.apps.localhost"), "Unknown(\"recipes.apps\")");
        assert_eq!(name("--personal.apps.localhost"), "Unknown(\"--personal.apps\")");
        assert_eq!(name("example.com"), "Foreign");
        assert_eq!(name("evilhub.localhost.example.com"), "Foreign");
        assert_eq!(resolve(&routes, "localhost", "localhost:7680"), Target::Status);
        assert_eq!(resolve(&routes, "localhost", "fleetlayer.localhost:7680"), Target::Status);
    }

    #[test]
    fn names_and_patterns_are_checked() {
        assert!(check_name("pages").is_ok());
        for bad in ["", "Pages", "a.b", "-a", "fleetlayer", "a_b"] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
        assert!(check_pattern("*--*.apps").is_ok());
        for bad in ["", "a..b", "a/b", ".a"] {
            assert!(check_pattern(bad).is_err(), "{bad}");
        }
        assert!(glob("*--*", "a--b") && !glob("*--*", "a-b") && !glob("*", ""));
    }
}
