//! What each component is made of on the machine it leaves: its settings (one file, or its block in node.yaml), its
//! state (files and folders that must arrive intact and must not already exist at the destination), content folders
//! (pages' spaces, copied only when the destination doesn't have them, since a shared or synced folder moves with the
//! share), its address and its route on the gateway.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::archive::{self, Setting};
use crate::up;
use fleetlayer_protocol::yaml_text;

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Control,
    Node,
    Gateway,
    /// A plugin of the node, by its name in node.yaml.
    Plugin(String),
}

impl Kind {
    /// `hub`, `pages`, `apps`, `sign-in`... or any plugin's name in node.yaml.
    pub fn parse(s: &str) -> Kind {
        match s {
            "control" | "control-plane" => Kind::Control,
            "node" => Kind::Node,
            "gateway" => Kind::Gateway,
            "hub" | "claude-hub" | "claude-accounts-hub" => Kind::Plugin("claude-auth-manager".into()),
            "apps" => Kind::Plugin("pages".into()),
            "sign-in" | "signin" => Kind::Plugin("signin".into()),
            other => Kind::Plugin(other.strip_prefix("plugin:").unwrap_or(other).to_string()),
        }
    }

    pub fn id(&self) -> String {
        match self {
            Kind::Control => "control".into(),
            Kind::Node => "node".into(),
            Kind::Gateway => "gateway".into(),
            Kind::Plugin(n) => format!("plugin:{n}"),
        }
    }

    pub fn from_id(s: &str) -> anyhow::Result<Kind> {
        Ok(match s {
            "control" => Kind::Control,
            "node" => Kind::Node,
            "gateway" => Kind::Gateway,
            _ => Kind::Plugin(
                s.strip_prefix("plugin:").ok_or_else(|| anyhow::anyhow!("unknown component {s:?}"))?.to_string(),
            ),
        })
    }

    pub fn describe(&self) -> String {
        match self {
            Kind::Control => "the control plane".into(),
            Kind::Node => "the node".into(),
            Kind::Gateway => "the gateway".into(),
            Kind::Plugin(n) if n == "claude-auth-manager" => {
                "the Claude accounts hub (plugin claude-auth-manager)".into()
            }
            Kind::Plugin(n) if n == "pages" => "pages, with its apps (plugin pages)".into(),
            Kind::Plugin(n) if n == "signin" => "sign-in (plugin signin)".into(),
            Kind::Plugin(n) => format!("plugin {n}"),
        }
    }
}

/// A route on the gateway for a plugin's web service.
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub name: String,
    pub health: Option<String>,
    pub hosts: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Inspected {
    /// Files and folders that make up its state, under the home folder.
    pub state: Vec<PathBuf>,
    /// Folders copied only when the destination hasn't got them.
    pub content: Vec<PathBuf>,
    /// Its settings file and that file's text (control.yaml, gateway.yaml, node.yaml).
    pub settings_file: Option<(PathBuf, String)>,
    /// Its block in node.yaml (a plugin).
    pub plugin_block: Option<String>,
    /// The port it listens on now, when it has a fixed one.
    pub port: Option<u16>,
    /// Whether it serves anything over the network (a plugin with a `listen:`).
    pub listens: bool,
    pub route: Option<Route>,
    pub notes: Vec<String>,
}

pub struct Here {
    pub home: PathBuf,
    pub layout: up::Layout,
}

impl Here {
    pub fn new() -> Here {
        Here { home: home(), layout: up::Layout { config: up::config_dir(), state: up::state_dir() } }
    }

    pub fn node_yaml_text(&self) -> Option<String> {
        std::fs::read_to_string(self.layout.node_yaml()).ok()
    }

    fn expand(&self, s: &str) -> PathBuf {
        match s.strip_prefix("~/") {
            Some(rest) => self.home.join(rest),
            None => PathBuf::from(s),
        }
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn yaml(text: &str) -> anyhow::Result<Value> {
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    Ok(yaml_serde::from_str(text)?)
}

fn port_of(listen: &str) -> Option<u16> {
    listen.rsplit_once(':').and_then(|(_, p)| p.trim_matches('"').parse().ok()).filter(|p| *p != 0)
}

/// Keys whose value names a program, not state.
const PROGRAM_KEYS: &[&str] = &["command", "program", "claude", "codex", "herdr", "rex", "socket", "node_socket"];

/// Every existing path under the home folder that a plugin's config names: its state, unless it's a program.
fn paths_in(here: &Here, v: &Value, key: &str, out: &mut Vec<(String, PathBuf)>) {
    match v {
        Value::String(s) if (s.starts_with('/') || s.starts_with("~/")) && !PROGRAM_KEYS.contains(&key) => {
            let p = here.expand(s);
            if p.starts_with(&here.home) && p != here.home && std::fs::symlink_metadata(&p).is_ok() {
                out.push((key.to_string(), p));
            }
        }
        Value::Array(a) => a.iter().for_each(|x| paths_in(here, x, key, out)),
        Value::Object(o) => o.iter().for_each(|(k, x)| paths_in(here, x, k, out)),
        _ => {}
    }
}

fn is_program(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Where a plugin keeps state when its config doesn't say (each plugin's own default).
fn default_state(command: &str, config: &Value) -> Vec<&'static str> {
    let bin = Path::new(command).file_name().and_then(|n| n.to_str()).unwrap_or(command);
    match bin {
        "fleetlayer-claude-auth-manager" if config["role"] == "hub" && config["hub"]["dir"].is_null() => {
            vec!["~/.local/state/fleetlayer/claude-hub"]
        }
        "fleetlayer-scheduler" => {
            let mut v = vec![];
            if config["settings"].is_null() {
                v.push("~/.config/fleetlayer/scheduler.yaml");
            }
            if config["state_dir"].is_null() {
                v.push("~/.local/state/fleetlayer/scheduler");
            }
            v
        }
        "fleetlayer-summaries" if config["state_dir"].is_null() => vec!["~/.local/state/fleetlayer/summaries"],
        "fleetlayer-cookies" => vec!["~/.local/state/fleetlayer/cookies"],
        _ => vec![],
    }
}

/// The route a plugin's service had, when the node can't say (it isn't running).
fn default_route(plugin: &str, config: &Value) -> Option<Route> {
    let name = match plugin {
        "claude-auth-manager" => config["hub"]["service"].as_str().unwrap_or("claude-hub").to_string(),
        "pages" | "signin" => plugin.to_string(),
        _ => return None,
    };
    let hosts = match (plugin, config["apps_domain"].as_str()) {
        ("pages", Some(d)) => d.strip_suffix(".localhost").map(|a| vec![format!("*--*.{a}")]).unwrap_or_default(),
        _ => vec![],
    };
    Some(Route { name, health: Some("/healthz".into()), hosts })
}

/// URL settings that point at this machine's loopback (`listen:` aside: that's rewritten).
fn loopback_urls(v: &Value, key: &str, out: &mut Vec<(String, String)>) {
    match v {
        Value::String(s) if s.contains("://127.0.0.1") || s.contains("://localhost") || s.contains("://[::1]") => {
            out.push((key.to_string(), s.clone()))
        }
        Value::Array(a) => a.iter().for_each(|x| loopback_urls(x, key, out)),
        Value::Object(o) => o.iter().for_each(|(k, x)| loopback_urls(x, k, out)),
        _ => {}
    }
}

pub fn default_port(plugin: &str) -> Option<u16> {
    match plugin {
        "signin" => Some(7650),
        "claude-auth-manager" => Some(7660),
        "pages" => Some(7670),
        _ => None,
    }
}

fn plugin_config(node_yaml: &Value, name: &str) -> Option<Value> {
    node_yaml["plugins"].as_array()?.iter().find(|p| p["name"] == name).cloned()
}

pub fn inspect_plugin(here: &Here, name: &str, announced: &[Value]) -> anyhow::Result<Inspected> {
    let text = here
        .node_yaml_text()
        .ok_or_else(|| anyhow::anyhow!("there's no node.yaml here ({})", here.layout.node_yaml().display()))?;
    let v = yaml(&text)?;
    let plugin = plugin_config(&v, name).ok_or_else(|| {
        anyhow::anyhow!(
            "no plugin called {name} in {} (and it isn't control, node or gateway)",
            here.layout.node_yaml().display()
        )
    })?;
    let block = yaml_text::plugin_text(&text, name).ok_or_else(|| {
        anyhow::anyhow!(
            "the {name} plugin is in {} but not as a `- name: {name}` item under `plugins:`; move it by hand",
            here.layout.node_yaml().display()
        )
    })?;
    let config = plugin["config"].clone();
    let mut i = Inspected { plugin_block: Some(block.clone()), ..Default::default() };
    let mut found = vec![];
    paths_in(here, &config, "", &mut found);
    for (key, p) in found {
        if is_program(&p) {
            continue;
        }
        if name == "pages" && matches!(key.as_str(), "dir" | "apps") {
            i.content.push(p);
        } else {
            i.state.push(p);
        }
    }
    for d in default_state(plugin["command"].as_str().unwrap_or(""), &config) {
        let p = here.expand(d);
        if p.exists() && !i.state.contains(&p) {
            i.state.push(p);
        }
    }
    i.state.sort();
    i.state.dedup();
    i.listens = block.lines().any(|l| l.trim_start().starts_with("listen:"));
    i.port = block
        .lines()
        .find_map(|l| l.trim_start().strip_prefix("listen:"))
        .and_then(|l| port_of(l.split('#').next().unwrap_or("").trim()));
    let mine: Vec<&Value> = announced.iter().filter(|s| s["plugin"] == name).collect();
    i.route = match mine.first() {
        Some(s) => Some(Route {
            name: s["name"].as_str().unwrap_or(name).to_string(),
            health: s["health"].as_str().map(String::from),
            hosts: s["hosts"].as_array().into_iter().flatten().filter_map(|h| h.as_str().map(String::from)).collect(),
        }),
        None => default_route(name, &config),
    };
    let mut loopback = vec![];
    loopback_urls(&config, "", &mut loopback);
    for (key, url) in loopback {
        i.notes.push(format!(
            "Its setting {key} is {url}, this machine's loopback: on the new machine that's the new machine itself. Change it there to an address it can reach, unless that service moves too."
        ));
    }
    if name == "pages" {
        i.notes.push(
            "Agents here write pages into the space folders; once pages runs elsewhere they use `fleetlayer pages push`, or the folders are shared with the new machine."
                .into(),
        );
    }
    if name == "signin" {
        i.notes.push(
            "Passkeys belong to its passkey domain (by default the issuer's parent domain), so they keep working on the new machine and at any address under that domain; a new issuer address still means repointing every service that signs in through it (docs/sso.md, \"Moving it\")."
                .into(),
        );
    }
    Ok(i)
}

pub fn inspect(here: &Here, kind: &Kind, announced: &[Value]) -> anyhow::Result<Inspected> {
    match kind {
        Kind::Plugin(name) => inspect_plugin(here, name, announced),
        Kind::Control => {
            let file = here.layout.control_yaml();
            let text = std::fs::read_to_string(&file).map_err(|_| {
                anyhow::anyhow!(
                    "no control plane settings at {} (it's written by `fleetlayer up` from v0.3.13; run `fleetlayer up` once on this machine first)",
                    file.display()
                )
            })?;
            let v = yaml(&text)?;
            let db = v["db"]
                .as_str()
                .map(|s| here.expand(s))
                .ok_or_else(|| anyhow::anyhow!("{} names no db", file.display()))?;
            anyhow::ensure!(db.exists(), "the control plane's database {} isn't there", db.display());
            let dir = db.parent().expect("a database file has a folder").to_path_buf();
            let mut i = Inspected { settings_file: Some((file, text.clone())), listens: true, ..Default::default() };
            i.state.push(dir.clone());
            for key in ["admin_token_file", "audit_key_file", "audit_anchor_dir"] {
                if let Some(p) = v[key].as_str().map(|s| here.expand(s))
                    && !p.starts_with(&dir)
                    && p.exists()
                {
                    i.state.push(p);
                }
            }
            for p in v["policy_files"].as_array().into_iter().flatten().filter_map(|p| p.as_str()) {
                let path = p.split_once('=').filter(|(t, _)| !t.contains('/')).map_or(p, |(_, path)| path);
                let path = here.expand(path);
                if path.exists() && path.starts_with(&here.home) {
                    i.state.push(path);
                }
            }
            i.port = v["listen"].as_str().and_then(port_of);
            i.route = Some(Route { name: "control".into(), health: Some("/healthz".into()), hosts: vec![] });
            Ok(i)
        }
        Kind::Gateway => {
            let file = here.layout.gateway_yaml();
            let text = std::fs::read_to_string(&file)
                .map_err(|_| anyhow::anyhow!("no gateway here ({} isn't there)", file.display()))?;
            let v = yaml(&text)?;
            let state = v["state_dir"]
                .as_str()
                .map(|s| here.expand(s))
                .unwrap_or_else(|| here.home.join(".local/state/fleetlayer/gateway"));
            let mut i = Inspected { settings_file: Some((file, text)), listens: true, ..Default::default() };
            if state.exists() {
                i.state.push(state);
            }
            i.port = v["listen"].as_str().and_then(port_of).or(Some(7680));
            for (name, s) in v["services"].as_object().into_iter().flatten() {
                if s["url"].as_str().is_some_and(|u| u.contains("://127.0.0.1") || u.contains("://localhost")) {
                    i.notes.push(format!(
                        "gateway.yaml routes {name} to this machine's loopback ({}); there it would reach the new machine's own: change it to an address the new machine can reach.",
                        s["url"].as_str().unwrap_or("")
                    ));
                }
            }
            Ok(i)
        }
        Kind::Node => {
            let file = here.layout.node_yaml();
            let text = std::fs::read_to_string(&file)
                .map_err(|_| anyhow::anyhow!("no node here ({} isn't there)", file.display()))?;
            let v = yaml(&text)?;
            let mut i = Inspected { settings_file: Some((file, text)), ..Default::default() };
            let key = here.layout.config.join("node-key");
            anyhow::ensure!(key.exists(), "this node has no key yet ({}); start it once first", key.display());
            i.state.push(key);
            if let Some(t) = v["control"]["token_file"].as_str().map(|s| here.expand(s)).filter(|p| p.exists()) {
                i.state.push(t);
            }
            let st = here.home.join(".local/state/fleetlayer");
            for f in ["known-keys.json", "applied.json", "tools"] {
                if st.join(f).exists() {
                    i.state.push(st.join(f));
                }
            }
            for p in v["plugins"].as_array().into_iter().flatten() {
                let Some(name) = p["name"].as_str() else { continue };
                if p["enabled"] == false {
                    continue;
                }
                if let Ok(pi) = inspect_plugin(here, name, announced) {
                    i.state.extend(pi.state);
                    i.content.extend(pi.content);
                }
            }
            i.state.sort();
            i.state.dedup();
            if let Some(url) = v["control"]["url"].as_str()
                && (url.contains("://127.0.0.1") || url.contains("://localhost") || url.contains("://[::1]"))
            {
                i.notes.push(format!(
                    "This node links to a control plane on this machine's loopback ({url}): move the control plane first (`fleetlayer move control …`), or set control.url on the new machine."
                ));
            }
            for p in v["plugins"].as_array().into_iter().flatten() {
                for key in ["program", "claude"] {
                    if let Some(prog) = p["config"][key].as_str() {
                        i.notes.push(format!(
                            "Plugin {} runs {prog}: it must be at that path on the new machine too.",
                            p["name"].as_str().unwrap_or("?")
                        ));
                    }
                }
            }
            Ok(i)
        }
    }
}

/// Every file under the given files and folders (folders walked; symbolic links, sockets and the like left out, each
/// with a note).
pub fn files_under(paths: &[PathBuf], notes: &mut Vec<String>) -> anyhow::Result<Vec<PathBuf>> {
    let mut out = vec![];
    for p in paths {
        walk(p, &mut out, notes)?;
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn walk(p: &Path, out: &mut Vec<PathBuf>, notes: &mut Vec<String>) -> anyhow::Result<()> {
    // Something still running can remove a file between listing and reading it (a temporary file renamed into
    // place): it's simply not there.
    let meta = match std::fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if meta.is_symlink() {
        notes.push(format!("Left out {} (a symbolic link).", p.display()));
    } else if meta.is_dir() {
        let Ok(entries) = std::fs::read_dir(p) else { return Ok(()) };
        for e in entries {
            walk(&e?.path(), out, notes)?;
        }
    } else if meta.is_file() {
        if name != super::settings_moved_to() && !name.ends_with(".moving") {
            out.push(p.to_path_buf());
        }
    } else {
        notes.push(format!("Left out {} (not a regular file).", p.display()));
    }
    Ok(())
}

/// A path under `from` as it will be under `to`, as text: settings name absolute paths.
pub fn rehome(text: &str, from: &Path, to: &Path) -> String {
    let (f, t) = (format!("{}/", from.display()), format!("{}/", to.display()));
    if f == t { text.to_string() } else { text.replace(&f, &t) }
}

/// Paths relative to the home folder.
pub fn rel(home: &Path, p: &Path) -> anyhow::Result<String> {
    Ok(p.strip_prefix(home)
        .map_err(|_| {
            anyhow::anyhow!(
                "{} isn't under the home folder {}, so it can't be placed on the other machine",
                p.display(),
                home.display()
            )
        })?
        .display()
        .to_string())
}

/// The settings the destination writes, rewritten for its home and its address.
pub fn settings_for(
    kind: &Kind,
    i: &Inspected,
    here: &Here,
    dest_home: &Path,
    listen: Option<&str>,
    node_name: Option<&str>,
) -> anyhow::Result<Vec<Setting>> {
    let mut out = vec![];
    if let Some((file, text)) = &i.settings_file {
        let mut text = rehome(text, &here.home, dest_home);
        match kind {
            Kind::Control => {
                let l = listen.expect("the control plane listens");
                text = yaml_text::set_top(&text, "listen", l);
                if let Some((host, port)) = l.rsplit_once(':')
                    && let Ok(port) = port.parse::<u16>()
                {
                    text = yaml_text::set_top(&text, "hooks_listen", &format!("{host}:{}", port.saturating_add(1)));
                }
            }
            Kind::Gateway => text = yaml_text::set_top(&text, "listen", listen.expect("the gateway listens")),
            Kind::Node => {
                if let Some(n) = node_name {
                    text = yaml_text::set_top(&text, "name", n);
                }
            }
            Kind::Plugin(_) => {}
        }
        out.push(Setting::File { path: rel(&here.home, file)?, text });
    }
    if let (Kind::Plugin(name), Some(block)) = (kind, &i.plugin_block) {
        let mut text = rehome(block, &here.home, dest_home);
        if let Some(l) = listen {
            text = set_listen(&text, l);
        }
        out.push(Setting::Plugin { name: name.clone(), text });
    }
    Ok(out)
}

/// The block's first `listen:` set to `listen`.
pub fn set_listen(block: &str, listen: &str) -> String {
    let mut done = false;
    block
        .lines()
        .map(|l| {
            let t = l.trim_start();
            if !done && t.starts_with("listen:") {
                done = true;
                let pad = &l[..l.len() - t.len()];
                format!("{pad}listen: {}\n", yaml_text::quote(listen))
            } else {
                format!("{l}\n")
            }
        })
        .collect()
}

/// Manifest entries for every file.
pub fn entries(home: &Path, files: &[PathBuf]) -> anyhow::Result<Vec<archive::FileEntry>> {
    files.iter().map(|f| archive::entry(home, f)).collect()
}

/// Entries for the files still there: for looking before it stops, while it may still change its files.
pub fn entries_now(home: &Path, files: &[PathBuf]) -> Vec<archive::FileEntry> {
    files.iter().filter_map(|f| archive::entry(home, f).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_people_use_find_the_component() {
        assert_eq!(Kind::parse("hub"), Kind::Plugin("claude-auth-manager".into()));
        assert_eq!(Kind::parse("apps"), Kind::Plugin("pages".into()));
        assert_eq!(Kind::parse("sign-in"), Kind::Plugin("signin".into()));
        assert_eq!(Kind::parse("control"), Kind::Control);
        assert_eq!(Kind::parse("plugin:scheduler"), Kind::Plugin("scheduler".into()));
        for k in [Kind::Control, Kind::Node, Kind::Gateway, Kind::Plugin("pages".into())] {
            assert_eq!(Kind::from_id(&k.id()).unwrap(), k);
        }
    }

    #[test]
    fn settings_are_rehomed_and_readdressed() {
        let t =
            rehome("dir: /Users/a/.local/state/x\nother: /Users/ab/y\n", Path::new("/Users/a"), Path::new("/home/a"));
        assert_eq!(t, "dir: /home/a/.local/state/x\nother: /Users/ab/y\n");
        let b = set_listen("  - name: pages\n    config:\n      listen: 127.0.0.1:0\n      x: 1\n", "10.0.0.5:7670");
        assert_eq!(b, "  - name: pages\n    config:\n      listen: \"10.0.0.5:7670\"\n      x: 1\n");
        assert_eq!(port_of("127.0.0.1:7600"), Some(7600));
        assert_eq!(port_of("127.0.0.1:0"), None);
    }

    #[test]
    fn a_plugins_state_is_what_its_config_names_but_not_its_programs() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        let cfg = home.join(".config/fleetlayer");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::create_dir_all(home.join(".local/state/fleetlayer/claude-hub")).unwrap();
        std::fs::write(home.join(".local/state/fleetlayer/claude-hub/store.json"), "{}").unwrap();
        std::fs::write(home.join(".local/state/fleetlayer/claude-hub-login"), "k").unwrap();
        std::fs::create_dir_all(home.join("Fleetlayer/personal/pages")).unwrap();
        std::fs::create_dir_all(home.join("bin")).unwrap();
        std::fs::write(home.join("bin/claude"), "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(home.join("bin/claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let h = home.display();
        std::fs::write(
            cfg.join("node.yaml"),
            format!("name: laptop\nplugins:\n  - name: claude-auth-manager\n    command: fleetlayer-claude-auth-manager\n    config:\n      role: hub\n      hub:\n        claude: {h}/bin/claude\n        listen: 127.0.0.1:0\n        login_file: {h}/.local/state/fleetlayer/claude-hub-login\n  - name: pages\n    command: fleetlayer-pages\n    config:\n      listen: 127.0.0.1:0\n      apps_domain: apps.localhost\n      spaces:\n        personal: {{ dir: {h}/Fleetlayer/personal/pages }}\n"),
        )
        .unwrap();
        let here = Here {
            home: home.to_path_buf(),
            layout: up::Layout { config: cfg.clone(), state: home.join(".local/state/fleetlayer") },
        };
        let hub = inspect_plugin(&here, "claude-auth-manager", &[]).unwrap();
        assert_eq!(
            hub.state,
            vec![
                home.join(".local/state/fleetlayer/claude-hub"),
                home.join(".local/state/fleetlayer/claude-hub-login")
            ]
        );
        assert!(hub.listens);
        assert_eq!(hub.route.as_ref().unwrap().name, "claude-hub");
        let pages = inspect_plugin(&here, "pages", &[]).unwrap();
        assert!(pages.state.is_empty());
        assert_eq!(pages.content, vec![home.join("Fleetlayer/personal/pages")]);
        assert_eq!(pages.route.unwrap().hosts, vec!["*--*.apps"]);
        assert!(inspect_plugin(&here, "nope", &[]).is_err());
    }
}
