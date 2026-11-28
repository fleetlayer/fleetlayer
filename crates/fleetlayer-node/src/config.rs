//! The node's config: `~/.config/fleetlayer/node.yaml` (or `$FLEETLAYER_CONFIG`). Every field is optional; with no
//! file at all the node runs with no plugins, named after the machine.
//!
//! ```yaml
//! name: laptop            # the name the control plane knows this node by
//! control:
//!   url: wss://control.example.com/v1/link
//!   token_file: ~/.config/fleetlayer/node-token
//! harnesses:             # what sessions started here run with (`fleetlayer session start`)
//!   claude: { args: [--permission-mode, acceptEdits] }
//! session_commands: [/compact, /clear]   # what `fleetlayer session command` may type (the default)
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
    /// The agent programs sessions started here run, by harness name (`claude`, `codex`): the arguments every session
    /// started or resumed on this node gets, such as its permission mode. Only this file sets them; a caller can't.
    #[serde(default)]
    pub harnesses: std::collections::BTreeMap<String, HarnessConfig>,
    /// The slash commands `fleetlayer session command` may type into this node's sessions (each may be followed by
    /// arguments). Default `[/compact, /clear]`; `[]` allows none.
    #[serde(default = "default_session_commands")]
    pub session_commands: Vec<String>,
    /// Report the node's health to a monitor (a Gatus external endpoint, say).
    pub heartbeat: Option<HeartbeatConfig>,
    /// Tools this node shares with sessions on other nodes (`crate::tools`).
    #[serde(default)]
    pub tools: Vec<crate::tools::ToolConfig>,
    /// Commands this node runs, with typed parameters, for sessions on other nodes (`crate::commands`,
    /// `docs/guides/shared-commands.md`).
    #[serde(default)]
    pub commands: Vec<crate::commands::CommandConfig>,
    /// The gateway's address (`http://127.0.0.1:7680`): the node's health includes the gateway's `/healthz`, so a
    /// gateway that keeps dying turns the node's heartbeat red (`fleetlayer up` sets it).
    pub gateway: Option<String>,
    /// Self-update from Fleetlayer's releases (`crate::update`).
    pub update: Option<crate::update::UpdateConfig>,
    /// Install the skills that ship with the release where Claude Code and Codex read them (`crate::skills`).
    /// Default true.
    #[serde(default = "yes")]
    pub skills: bool,
    /// Hooks for role windows (`crate::window`): on one of the role's nodes, `on_end` runs when its window closes;
    /// on a node that only watches the role (a trusted runner), `on_start` and `on_end` both run.
    #[serde(default)]
    pub windows: Vec<crate::window::WindowHooks>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessConfig {
    /// Put before Fleetlayer's own arguments (`--name <tab>`, `--resume <id>`).
    #[serde(default)]
    pub args: Vec<String>,
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

pub fn default_session_commands() -> Vec<String> {
    vec!["/compact".into(), "/clear".into()]
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
    fleetlayer_protocol::local::default_node_socket()
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
                for t in &c.tools {
                    t.check().map_err(|e| anyhow::anyhow!("{}: tool {}: {e}", path.display(), t.name))?;
                }
                let mut names = std::collections::HashSet::new();
                for cmd in &c.commands {
                    cmd.check().map_err(|e| anyhow::anyhow!("{}: command {}: {e}", path.display(), cmd.name))?;
                    anyhow::ensure!(
                        names.insert(cmd.name.clone()),
                        "{}: two commands are called {}",
                        path.display(),
                        cmd.name
                    );
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

/// Save a moved control plane's link URL as `control.url` in node.yaml, keeping the rest of the file as it is (written
/// to a temporary file with the same permissions and renamed into place).
pub fn set_control_url(path: &Path, url: &str) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path)?;
    let out = fleetlayer_protocol::yaml_text::set_in_section(&text, "control", "url", url)
        .ok_or_else(|| anyhow::anyhow!("no control: block with a url: line in it"))?;
    let check: NodeConfig = yaml_serde::from_str(&out)?;
    anyhow::ensure!(check.control.is_some_and(|c| c.url == url), "the edit didn't take");
    fleetlayer_protocol::local::write_atomic(path, &out, 0o600)?;
    Ok(())
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
