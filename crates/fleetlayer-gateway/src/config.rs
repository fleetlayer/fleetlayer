//! The gateway's settings: `~/.config/fleetlayer/gateway.yaml` (or `$FLEETLAYER_GATEWAY_CONFIG`). Every field is
//! optional; with no file at all it serves `<name>.localhost:7680` for whatever the node's plugins announce.
//!
//! ```yaml
//! domain: localhost              # services are <name>.<domain>
//! listen: 127.0.0.1:7680         # loopback only unless you mean otherwise
//! services:                      # services that aren't plugins, or that moved to another machine
//!   control: { url: http://127.0.0.1:7600, health: /healthz }
//!   pages: { url: http://10.0.0.5:7670 }   # overrides what this node's pages plugin announces
//! alerts:
//!   session: planning            # a Fleetlayer message to this session, plus a desktop notification
//! heartbeats:                    # pushes taken at /api/v1/endpoints/<key>/external, as Gatus takes them
//!   node: { every: 60 }
//! ```

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "localhost")]
    pub domain: String,
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// The port people type, when it isn't `listen`'s (a gateway behind another proxy). Default: `listen`'s.
    pub public_port: Option<u16>,
    #[serde(default)]
    pub services: BTreeMap<String, ServiceConfig>,
    #[serde(default)]
    pub alerts: AlertsConfig,
    #[serde(default)]
    pub heartbeats: BTreeMap<String, HeartbeatConfig>,
    /// The node's local socket, for the services its plugins announce. Default: the node's own default.
    pub node_socket: Option<PathBuf>,
    /// Ask the node for its announced services (default true).
    #[serde(default = "yes")]
    pub node: bool,
    /// Where its state lives. Default `~/.local/state/fleetlayer/gateway`.
    pub state_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        yaml_serde::from_str("{}").expect("an empty config is valid")
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    /// Where it really listens: `http://127.0.0.1:7600`. Left out, an announced service of this name keeps its own.
    pub url: Option<String>,
    /// The path that answers 2xx while it works (`/healthz`). Left out, no health check (unless announced).
    pub health: Option<String>,
    /// More host patterns under the domain, `*` standing for part of one label: `*--*.apps`.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Hide an announced service: no route, no check.
    #[serde(default)]
    pub hidden: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AlertsConfig {
    /// A session (`tab` or `tab@node`) that gets a Fleetlayer message for each change.
    pub session: Option<String>,
    /// A desktop notification for each change (default true).
    #[serde(default = "yes")]
    pub desktop: bool,
}

impl Default for AlertsConfig {
    fn default() -> Self {
        AlertsConfig { session: None, desktop: true }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatConfig {
    /// Seconds between pushes; a push missing for twice this is a failure.
    #[serde(default = "sixty")]
    pub every: u64,
    /// A file holding the bearer token pushes must carry. Without one, any local process may push.
    pub token_file: Option<PathBuf>,
}

fn localhost() -> String {
    "localhost".into()
}
fn default_listen() -> SocketAddr {
    "127.0.0.1:7680".parse().expect("a valid address")
}
fn yes() -> bool {
    true
}
fn sixty() -> u64 {
    60
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub fn expand(p: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => home().join(rest),
        Err(_) => p.to_path_buf(),
    }
}

pub fn default_path() -> PathBuf {
    match std::env::var_os("FLEETLAYER_GATEWAY_CONFIG") {
        Some(p) => PathBuf::from(p),
        None => std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".config"))
            .join("fleetlayer/gateway.yaml"),
    }
}

/// The node's default socket, as the node works it out.
pub fn default_node_socket() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) => PathBuf::from(d).join("fleetlayer/node.sock"),
        None => home().join(".local/state/fleetlayer/node.sock"),
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let c: Config = match std::fs::read_to_string(path) {
            Ok(text) => yaml_serde::from_str(&text).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => anyhow::bail!("reading {}: {e}", path.display()),
        };
        c.check()?;
        Ok(c)
    }

    pub fn check(&self) -> anyhow::Result<()> {
        for (name, s) in &self.services {
            crate::registry::check_name(name)?;
            if let Some(u) = &s.url {
                let parsed = reqwest::Url::parse(u).map_err(|e| anyhow::anyhow!("services.{name}.url {u}: {e}"))?;
                if parsed.scheme() != "http" {
                    anyhow::bail!("services.{name}.url {u}: the gateway reaches backends over http://");
                }
            }
            for h in &s.hosts {
                crate::registry::check_pattern(h).map_err(|e| anyhow::anyhow!("services.{name}.hosts: {e}"))?;
            }
        }
        for key in self.heartbeats.keys() {
            crate::registry::check_name(key)?;
        }
        Ok(())
    }

    pub fn state_dir(&self) -> PathBuf {
        self.state_dir.as_deref().map(expand).unwrap_or_else(|| home().join(".local/state/fleetlayer/gateway"))
    }

    pub fn node_socket(&self) -> PathBuf {
        self.node_socket.as_deref().map(expand).unwrap_or_else(default_node_socket)
    }

    pub fn port(&self) -> u16 {
        self.public_port.unwrap_or(self.listen.port())
    }

    /// The address people use for a service: `http://pages.localhost:7680`.
    pub fn address(&self, name: &str) -> String {
        let port = self.port();
        if port == 80 {
            format!("http://{name}.{}", self.domain)
        } else {
            format!("http://{name}.{}:{port}", self.domain)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_full_configs_load() {
        let c = Config::default();
        assert_eq!(c.domain, "localhost");
        assert_eq!(c.listen.port(), 7680);
        assert!(c.alerts.desktop);
        let c: Config = yaml_serde::from_str(
            "services:\n  control: { url: 'http://127.0.0.1:7600', health: /healthz }\n  pages: { hosts: ['*--*.apps'] }\nalerts: { session: planning }\nheartbeats:\n  node: { every: 30 }\n",
        )
        .unwrap();
        c.check().unwrap();
        assert_eq!(c.address("control"), "http://control.localhost:7680");
        assert_eq!(c.heartbeats["node"].every, 30);
    }

    #[test]
    fn bad_values_are_refused() {
        for bad in [
            "services:\n  x: { url: 'https://a' }\n",
            "services:\n  'a.b': { url: 'http://a' }\n",
            "services:\n  x: { hosts: ['a..b'] }\n",
            "nonsense: 1\n",
        ] {
            let ok = yaml_serde::from_str::<Config>(bad).is_ok_and(|c| c.check().is_ok());
            assert!(!ok, "{bad}");
        }
    }
}
