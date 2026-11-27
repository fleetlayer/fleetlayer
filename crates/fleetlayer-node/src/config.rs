//! The node's config: `~/.config/fleetlayer/node.yaml` (or `$FLEETLAYER_CONFIG`). Every field is optional; with no
//! file at all the node runs with no plugins, named after the machine.
//!
//! ```yaml
//! name: laptop            # the name the control plane knows this node by
//! control:
//!   url: wss://control.example.com/v1/link
//!   token_file: ~/.config/fleetlayer/node-token
//! plugins:
//!   - name: herdr
//!     command: fleetlayer-herdr
//!     config: { program: herdr }
//!     grants: [message.send]
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// The node's name. Default: the machine's name, plus `-<user>` for a user other than the machine's first.
    pub name: Option<String>,
    /// The machine's name. Default: its hostname, without a domain.
    pub machine: Option<String>,
    /// The local socket the CLI talks to.
    pub socket: Option<PathBuf>,
    #[serde(default)]
    pub plugins: Vec<PluginConfig>,
    /// The control plane to link to. Without it the node works on its own machine only.
    pub control: Option<ControlConfig>,
    /// When a session has no harness channel (no messaging socket), type the message into its terminal through the
    /// session manager. Default true. Typing can land in the middle of what a person is typing in that tab, so a
    /// machine where people work in the agents' tabs may turn it off; such messages then fail at once instead.
    #[serde(default = "yes")]
    pub typing_fallback: bool,
    /// Report the node's health to a monitor (a Gatus external endpoint, say).
    pub heartbeat: Option<HeartbeatConfig>,
    /// Tools this node shares with sessions on other nodes (`crate::tools`).
    #[serde(default)]
    pub tools: Vec<crate::tools::ToolConfig>,
    /// The gateway's address (`http://127.0.0.1:7680`): the node's health includes the gateway's `/healthz`, so a
    /// gateway that keeps dying turns the node's heartbeat red (`fleetlayer up` sets it).
    pub gateway: Option<String>,
    /// Self-update from Fleetlayer's releases (`crate::update`).
    pub update: Option<crate::update::UpdateConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatConfig {
    /// Posted to with `?success=true|false&error=<why>` added.
    pub url: String,
    /// A file holding a bearer token for the URL.
    pub token_file: Option<PathBuf>,
    /// Seconds between reports (default 60).
    #[serde(default = "sixty")]
    pub every: u64,
}

fn sixty() -> u64 {
    60
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlConfig {
    /// The control plane's link URL: `wss://…/v1/link` (or `ws://` on a private network).
    pub url: String,
    /// A file holding this node's token (from `fleetlayer-control node add`), mode 0600.
    pub token_file: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginConfig {
    pub name: String,
    /// The executable: a path, or a name on PATH.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Passed to the plugin untouched, in `initialize`.
    #[serde(default)]
    pub config: Value,
    /// What the plugin may ask the node for (`message.send`, `event.publish`, `sessions.query`...).
    #[serde(default)]
    pub grants: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

impl PluginConfig {
    pub fn granted(&self, what: &str) -> bool {
        self.grants.iter().any(|g| g == what || g == "*")
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub fn default_path() -> PathBuf {
    match std::env::var_os("FLEETLAYER_CONFIG") {
        Some(p) => PathBuf::from(p),
        None => config_dir().join("node.yaml"),
    }
}

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config")).join("fleetlayer")
}

/// The local socket's default path: under `$XDG_RUNTIME_DIR` when there is one, else `~/.local/state`.
pub fn default_socket() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) => PathBuf::from(d).join("fleetlayer/node.sock"),
        None => home().join(".local/state/fleetlayer/node.sock"),
    }
}

impl NodeConfig {
    /// Read the config file; a missing file is the empty config.
    pub fn load(path: &Path) -> anyhow::Result<NodeConfig> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let c: NodeConfig =
                    yaml_serde::from_str(&text).map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
                if let Some(h) = &c.heartbeat {
                    reqwest::Url::parse(&h.url).map_err(|e| anyhow::anyhow!("heartbeat url {}: {e}", h.url))?;
                }
                Ok(c)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(NodeConfig::default()),
            Err(e) => Err(anyhow::anyhow!("reading {}: {e}", path.display())),
        }
    }

    pub fn machine(&self) -> String {
        self.machine.clone().unwrap_or_else(|| {
            let h =
                nix::unistd::gethostname().ok().and_then(|h| h.into_string().ok()).unwrap_or_else(|| "machine".into());
            h.split('.').next().unwrap_or(&h).to_lowercase()
        })
    }

    pub fn user(&self) -> String {
        std::env::var("USER").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| {
            nix::unistd::User::from_uid(nix::unistd::getuid())
                .ok()
                .flatten()
                .map(|u| u.name)
                .unwrap_or_else(|| "user".into())
        })
    }

    pub fn name(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.machine())
    }

    pub fn socket(&self) -> PathBuf {
        self.socket.clone().unwrap_or_else(default_socket)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_reads_with_defaults() {
        let c: NodeConfig = yaml_serde::from_str(
            "name: laptop\nplugins:\n  - name: herdr\n    command: fleetlayer-herdr\n    config: { program: herdr }\n",
        )
        .unwrap();
        assert_eq!(c.name(), "laptop");
        let p = &c.plugins[0];
        assert!(p.enabled);
        assert_eq!(p.config["program"], "herdr");
        assert!(!p.granted("message.send"));
    }

    #[test]
    fn unknown_fields_are_an_error_not_silently_ignored() {
        assert!(yaml_serde::from_str::<NodeConfig>("plugin: []\n").is_err());
    }
}
