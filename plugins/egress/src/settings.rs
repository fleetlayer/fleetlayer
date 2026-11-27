//! The settings file (`~/.config/fleetlayer/egress.yaml` on the proxy's user): the mode, the ports, and each role's
//! users and hosts. Read again within a few seconds of a change.

use std::collections::BTreeMap;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use crate::hosts::{Cidr, Pattern, Target};

/// `log` lets everything through and records what it would refuse; `enforce` refuses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Log,
    Enforce,
}

impl Mode {
    pub fn word(self) -> &'static str {
        match self {
            Mode::Log => "log",
            Mode::Enforce => "enforce",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Ports {
    #[serde(default = "default_proxy_port")]
    pub proxy: u16,
    #[serde(default = "default_tls_port")]
    pub tls: u16,
    #[serde(default = "default_http_port")]
    pub http: u16,
    #[serde(default = "default_dns_port")]
    pub dns: u16,
}

fn default_proxy_port() -> u16 {
    3128
}
fn default_tls_port() -> u16 {
    3129
}
fn default_http_port() -> u16 {
    3130
}
fn default_dns_port() -> u16 {
    3153
}

impl Default for Ports {
    fn default() -> Self {
        Ports { proxy: 3128, tls: 3129, http: 3130, dns: 3153 }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoleFile {
    /// The Unix users in this role.
    pub users: Vec<String>,
    /// The role's node, which asks for hosts (`-@<node>` to access control). Without it, the role can't ask.
    #[serde(default)]
    pub node: Option<String>,
    /// This role's mode, when it differs from the file's.
    #[serde(default)]
    pub mode: Option<Mode>,
    #[serde(default)]
    pub presets: Vec<String>,
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Private address ranges (the LAN, the tailnet) this role may reach, through hosts that resolve into them.
    #[serde(default)]
    pub networks: Vec<String>,
    /// Whether the role's users may reach services on this machine's loopback (their own test servers). Default true.
    #[serde(default = "yes")]
    pub loopback: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SettingsFile {
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub ports: Ports,
    /// DNS servers the resolver forwards allowed names to (`1.1.1.1:53`); default the ones in /etc/resolv.conf.
    #[serde(default)]
    pub dns_upstream: Vec<String>,
    /// Days of audit log kept (default 30).
    #[serde(default)]
    pub keep_days: Option<u32>,
    #[serde(default)]
    pub roles: BTreeMap<String, RoleFile>,
}

/// A role, ready to decide.
#[derive(Debug, Clone)]
pub struct Role {
    pub name: String,
    pub users: Vec<String>,
    pub node: Option<String>,
    pub mode: Mode,
    pub presets: Vec<String>,
    pub patterns: Vec<(Pattern, Source)>,
    pub networks: Vec<Cidr>,
    pub loopback: bool,
}

/// Where an allowlist entry came from, for the audit line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Listed,
    Preset(String),
}

impl Source {
    pub fn words(&self) -> String {
        match self {
            Source::Listed => "listed".into(),
            Source::Preset(p) => format!("preset {p}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub mode: Mode,
    pub ports: Ports,
    pub dns_upstream: Vec<std::net::SocketAddr>,
    pub keep_days: u32,
    pub roles: Vec<Role>,
}

/// The built-in presets: hosts most fleets need. A role names them in `presets:`.
pub const PRESETS: &[(&str, &str, &[&str])] = &[
    (
        "anthropic",
        "Claude and Claude Code",
        &[
            "api.anthropic.com",
            "claude.ai",
            "console.anthropic.com",
            "statsig.anthropic.com",
            "mcp-proxy.anthropic.com",
        ],
    ),
    ("openai", "OpenAI's API, ChatGPT and Codex", &["api.openai.com", "chatgpt.com", "auth.openai.com"]),
    ("google-ai", "Gemini's API", &["generativelanguage.googleapis.com"]),
    (
        "github",
        "GitHub over HTTPS: git, the API, releases, raw files, container images",
        &["github.com", "*.github.com", "*.githubusercontent.com", "ghcr.io", "*.ghcr.io"],
    ),
    ("npm", "npm and Yarn registries", &["registry.npmjs.org", "registry.yarnpkg.com", "nodejs.org"]),
    ("pypi", "Python packages", &["pypi.org", "files.pythonhosted.org"]),
    (
        "crates",
        "Rust crates and toolchains",
        &["crates.io", "index.crates.io", "static.crates.io", "static.rust-lang.org", "sh.rustup.rs"],
    ),
    ("go", "Go modules", &["proxy.golang.org", "sum.golang.org", "go.dev"]),
    ("rubygems", "Ruby gems", &["rubygems.org", "index.rubygems.org"]),
    (
        "docs",
        "Common documentation sites",
        &[
            "docs.rs",
            "doc.rust-lang.org",
            "developer.mozilla.org",
            "docs.python.org",
            "docs.github.com",
            "docs.anthropic.com",
            "docs.claude.com",
            "pkg.go.dev",
        ],
    ),
];

pub fn preset(name: &str) -> Option<&'static [&'static str]> {
    PRESETS.iter().find(|(n, _, _)| *n == name).map(|(_, _, h)| *h)
}

pub fn parse(text: &str) -> Result<Settings, String> {
    let file: SettingsFile = if text.trim().is_empty() {
        SettingsFile::default()
    } else {
        yaml_serde::from_str(text).map_err(|e| e.to_string())?
    };
    from_file(file)
}

fn from_file(file: SettingsFile) -> Result<Settings, String> {
    let mut problems = vec![];
    let mut roles = vec![];
    let mut seen_users: BTreeMap<String, String> = BTreeMap::new();
    for (name, r) in &file.roles {
        if r.users.is_empty() {
            problems.push(format!("role {name}: users is empty"));
        }
        for u in &r.users {
            if u.is_empty() || u == "root" {
                problems.push(format!("role {name}: {u:?} can't be a restricted user"));
            }
            if let Some(other) = seen_users.insert(u.clone(), name.clone()) {
                problems.push(format!("user {u} is in two roles ({other} and {name})"));
            }
        }
        let mut patterns = vec![];
        for p in &r.presets {
            match preset(p) {
                Some(hosts) => {
                    for h in hosts {
                        patterns.push((Pattern::parse(h).expect("presets parse"), Source::Preset(p.clone())));
                    }
                }
                None => problems.push(format!(
                    "role {name}: no preset is named {p:?} (presets: {})",
                    PRESETS.iter().map(|(n, _, _)| *n).collect::<Vec<_>>().join(", ")
                )),
            }
        }
        for h in &r.hosts {
            match Pattern::parse(h) {
                Ok(p) => patterns.push((p, Source::Listed)),
                Err(e) => problems.push(format!("role {name}: {e}")),
            }
        }
        let mut networks = vec![];
        for n in &r.networks {
            match Cidr::parse(n) {
                Ok(c) => networks.push(c),
                Err(e) => problems.push(format!("role {name}: {e}")),
            }
        }
        if let Some(n) = &r.node
            && (n.is_empty() || n.contains('@') || n.contains(char::is_whitespace))
        {
            problems.push(format!("role {name}: node {n:?} isn't a node name"));
        }
        roles.push(Role {
            name: name.clone(),
            users: r.users.clone(),
            node: r.node.clone(),
            mode: r.mode.unwrap_or(file.mode),
            presets: r.presets.clone(),
            patterns,
            networks,
            loopback: r.loopback,
        });
    }
    let mut dns_upstream = vec![];
    for u in &file.dns_upstream {
        let addr = u
            .parse::<std::net::SocketAddr>()
            .or_else(|_| u.parse::<IpAddr>().map(|ip| std::net::SocketAddr::new(ip, 53)));
        match addr {
            Ok(a) => dns_upstream.push(a),
            Err(_) => problems.push(format!("dns_upstream: {u:?} isn't an address like 1.1.1.1:53")),
        }
    }
    let p = &file.ports;
    let mut ports = [p.proxy, p.tls, p.http, p.dns];
    ports.sort_unstable();
    if ports.windows(2).any(|w| w[0] == w[1]) || ports.contains(&0) {
        problems.push("ports: proxy, tls, http and dns must be four different ports".into());
    }
    if !problems.is_empty() {
        return Err(problems.join("\n"));
    }
    Ok(Settings {
        mode: file.mode,
        ports: file.ports,
        dns_upstream,
        keep_days: file.keep_days.unwrap_or(30).max(1),
        roles,
    })
}

impl Settings {
    pub fn role_of_user(&self, user: &str) -> Option<&Role> {
        self.roles.iter().find(|r| r.users.iter().any(|u| u == user))
    }

    pub fn role(&self, name: &str) -> Option<&Role> {
        self.roles.iter().find(|r| r.name == name)
    }
}

impl Role {
    /// The first entry that allows `t`.
    pub fn listed(&self, t: &Target) -> Option<&(Pattern, Source)> {
        self.patterns.iter().find(|(p, _)| p.matches(t))
    }

    /// Whether a name may be resolved: some entry names it, whatever the port.
    pub fn name_listed(&self, host: &str) -> bool {
        self.patterns.iter().any(|(p, _)| p.matches_name(host))
    }

    /// Whether `ip` is in one of the role's private networks.
    pub fn network_allows(&self, ip: IpAddr) -> bool {
        self.networks.iter().any(|c| c.contains(ip))
    }
}
