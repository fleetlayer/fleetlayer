//! The destination's side of a move: `fleetlayer move-receive <step>`, run over SSH (or with another HOME, for
//! `local:`). Each step prints one JSON answer on stdout; progress and errors go to stderr. A move's working files
//! live in `~/.local/state/fleetlayer/moves/<id>/` (0700): the key the source seals to, the staging folder, and what
//! to undo.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use age::secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::archive::{self, Manifest, Setting};
use super::component::{self, Kind};
use super::services;
use crate::up;
use fleetlayer_protocol::yaml_text;

#[derive(Debug, Serialize, Deserialize)]
pub struct ProbeRequest {
    pub component: String,
    /// Files the move would write, relative to the home folder.
    pub files: Vec<String>,
    /// Content folders it would copy when they aren't there.
    pub content: Vec<String>,
    /// Settings files it would write.
    pub settings: Vec<String>,
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProbeAnswer {
    pub home: String,
    pub version: String,
    /// This move's ID and the key to seal to (none on a dry run).
    pub session: Option<String>,
    pub recipient: Option<String>,
    /// The address this machine was reached on over SSH ($SSH_CONNECTION), or 127.0.0.1 for `local:`.
    pub bind_ip: String,
    /// Files already there, with their SHA-256.
    pub existing: BTreeMap<String, String>,
    pub content_present: Vec<String>,
    /// Settings files already there.
    pub settings_present: Vec<String>,
    /// The destination's node.yaml, when it has one.
    pub node_yaml: Option<String>,
    /// What the destination's node calls itself.
    pub node_name: Option<String>,
    /// Fleetlayer programs missing there.
    pub missing: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Undo {
    component: String,
    files: Vec<String>,
    /// node.yaml as it was before a plugin was added, to put back.
    node_yaml_before: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HealthRequest {
    /// Where it listens (control plane, gateway).
    pub listen: Option<String>,
    /// The node's name and key it must have (node move).
    pub node_name: Option<String>,
    pub public_key: Option<String>,
    /// The node must be linked to its control plane.
    pub linked: bool,
    /// The plugin that must be running (plugin move), and whether it serves a web page to check.
    pub plugin: Option<String>,
    pub timeout_secs: u64,
}

fn moves_dir() -> PathBuf {
    up::state_dir().join("moves")
}

fn session_dir(id: &str) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()), "bad move ID {id:?}");
    Ok(moves_dir().join(id))
}

fn print(v: &impl Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string(v)?);
    Ok(())
}

fn bind_ip() -> String {
    if let Ok(c) = std::env::var("SSH_CONNECTION")
        && let Some(ip) = c.split_whitespace().nth(2)
        && let Ok(ip) = ip.parse::<std::net::IpAddr>()
    {
        return ip.to_string();
    }
    "127.0.0.1".into()
}

pub fn probe() -> anyhow::Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let req: ProbeRequest = serde_json::from_str(&input)?;
    let home = component::home();
    let mut existing = BTreeMap::new();
    for f in &req.files {
        let p = home.join(archive::safe_rel(f)?);
        if p.is_file() {
            existing.insert(f.clone(), archive::sha256_file(&p)?);
        } else if p.exists() {
            existing.insert(f.clone(), "not a file".into());
        }
    }
    let content_present =
        req.content.iter().filter(|c| archive::safe_rel(c).is_ok_and(|r| home.join(r).exists())).cloned().collect();
    let settings_present =
        req.settings.iter().filter(|c| archive::safe_rel(c).is_ok_and(|r| home.join(r).exists())).cloned().collect();
    let layout = up::Layout { config: up::config_dir(), state: up::state_dir() };
    let node_yaml = std::fs::read_to_string(layout.node_yaml()).ok();
    let node_name = up::find_bin("fleetlayer-node").ok().and_then(|bin| {
        let out = std::process::Command::new(bin).arg("config").output().ok()?;
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|l| l.strip_prefix("name: ").map(|s| s.trim().to_string()))
    });
    let mut missing = vec![];
    let kind = Kind::from_id(&req.component)?;
    let needed: &[&str] = match &kind {
        Kind::Control => &["fleetlayer-control"],
        Kind::Gateway => &["fleetlayer-gateway"],
        Kind::Node => &["fleetlayer-node"],
        Kind::Plugin(_) => &["fleetlayer-node"],
    };
    for b in needed {
        if up::find_bin(b).is_err() {
            missing.push(b.to_string());
        }
    }
    let (session, recipient) = if req.dry_run {
        (None, None)
    } else {
        let id = up::random_hex(16)?;
        let dir = session_dir(&id)?;
        up::private_dir(&dir)?;
        let key = age::x25519::Identity::generate();
        up::create(&dir.join("key"), key.to_string().expose_secret(), 0o600)?;
        (Some(id), Some(key.to_public().to_string()))
    };
    print(&ProbeAnswer {
        home: home.display().to_string(),
        version: env!("CARGO_PKG_VERSION").into(),
        session,
        recipient,
        bind_ip: bind_ip(),
        existing,
        content_present,
        settings_present,
        node_yaml,
        node_name,
        missing,
    })
}

fn write_private(path: &Path, text: &str) -> anyhow::Result<()> {
    if let Some(d) = path.parent() {
        up::private_dir(d)?;
    }
    Ok(fleetlayer_protocol::local::write_atomic(path, text, 0o600)?)
}

pub fn unpack(id: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = session_dir(id)?;
    let key = std::fs::read_to_string(dir.join("key")).map_err(|_| anyhow::anyhow!("no move {id} here"))?;
    let identity = age::x25519::Identity::from_str(key.trim()).map_err(|e| anyhow::anyhow!("the move's key: {e}"))?;
    let staging = dir.join("staging");
    std::fs::create_dir(&staging).map_err(|e| anyhow::anyhow!("{}: {e} (unpacked already?)", staging.display()))?;
    let manifest: Manifest = archive::read_sealed(&identity, std::io::stdin().lock(), &staging)?;
    let home = component::home();
    let kind = Kind::from_id(&manifest.component)?;

    // Check everything before moving anything into place.
    let mut to_write = vec![];
    let mut skipped = 0;
    for f in &manifest.files {
        let dest = home.join(archive::safe_rel(&f.path)?);
        if dest.exists() {
            anyhow::ensure!(
                dest.is_file() && archive::sha256_file(&dest)? == f.sha256,
                "{} is already there with other content; nothing was changed",
                dest.display()
            );
            skipped += 1;
        } else {
            to_write.push(f);
        }
    }
    let node_yaml_path = up::Layout { config: up::config_dir(), state: up::state_dir() }.node_yaml();
    for s in &manifest.settings {
        match s {
            Setting::File { path, text } => {
                let dest = home.join(archive::safe_rel(path)?);
                if dest.exists() {
                    anyhow::ensure!(
                        std::fs::read_to_string(&dest)? == *text,
                        "{} is already there; move it away first (nothing was changed)",
                        dest.display()
                    );
                }
            }
            Setting::Plugin { name, text } => {
                let current = std::fs::read_to_string(&node_yaml_path).map_err(|_| {
                    anyhow::anyhow!(
                        "there's no node here ({}): run `fleetlayer up --join …` first",
                        node_yaml_path.display()
                    )
                })?;
                anyhow::ensure!(
                    yaml_text::add_plugin(&current, name, text).is_some(),
                    "{} already has a plugin called {name}, or its plugins: list can't be added to as text",
                    node_yaml_path.display()
                );
            }
        }
    }

    let mut undo = Undo { component: manifest.component.clone(), ..Default::default() };
    let save_undo = |u: &Undo| std::fs::write(dir.join("undo.json"), serde_json::to_vec(u).unwrap_or_default());
    for f in to_write {
        let rel = archive::safe_rel(&f.path)?;
        let dest = home.join(&rel);
        if let Some(d) = dest.parent() {
            up::private_dir(d)?;
        }
        std::fs::rename(staging.join(&rel), &dest)?;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(f.mode & 0o777))?;
        undo.files.push(f.path.clone());
        save_undo(&undo)?;
    }
    for s in &manifest.settings {
        match s {
            Setting::File { path, text } => {
                let dest = home.join(archive::safe_rel(path)?);
                if !dest.exists() {
                    write_private(&dest, text)?;
                    undo.files.push(path.clone());
                }
            }
            Setting::Plugin { name, text } => {
                let current = std::fs::read_to_string(&node_yaml_path)?;
                let out = yaml_text::add_plugin(&current, name, text).expect("checked above");
                undo.node_yaml_before = Some(current);
                write_private(&node_yaml_path, &out)?;
            }
        }
        save_undo(&undo)?;
    }
    std::fs::write(dir.join("component"), kind.id())?;
    let _ = std::fs::remove_dir_all(&staging);
    print(&json!({"written": undo.files.len(), "skipped": skipped}))
}

fn session_kind(id: &str) -> anyhow::Result<Kind> {
    Kind::from_id(std::fs::read_to_string(session_dir(id)?.join("component"))?.trim())
}

pub fn start(id: &str) -> anyhow::Result<()> {
    services::start(&session_kind(id)?)?;
    print(&json!({"started": true}))
}

/// For the tests: make a step fail, as a lost connection or a broken machine would.
fn test_fail(step: &str) -> anyhow::Result<()> {
    if std::env::var("FLEETLAYER_MOVE_TEST_FAIL").is_ok_and(|v| v == step) {
        anyhow::bail!("(test) {step} failed");
    }
    Ok(())
}

/// Stop it here and remove what the move wrote, then prove it's stopped: the machine it came from starts its copy
/// again only after this answers.
pub async fn undo(id: &str) -> anyhow::Result<()> {
    test_fail("undo")?;
    let dir = session_dir(id)?;
    let Ok(text) = std::fs::read(dir.join("undo.json")) else {
        let _ = std::fs::remove_dir_all(&dir);
        return print(&json!({"undone": 0}));
    };
    let u: Undo = serde_json::from_slice(&text)?;
    let kind = Kind::from_id(&u.component)?;
    let home = component::home();
    match &kind {
        Kind::Plugin(_) => {}
        k => services::stop(k)?,
    }
    for f in &u.files {
        let _ = std::fs::remove_file(home.join(archive::safe_rel(f)?));
    }
    if let Some(before) = &u.node_yaml_before {
        write_private(&up::Layout { config: up::config_dir(), state: up::state_dir() }.node_yaml(), before)?;
        if services::running(&kind) {
            services::start(&kind)?;
        }
    }
    match &kind {
        Kind::Plugin(name) => {
            // The node restarted without it; a node that doesn't answer runs nothing.
            let until = Instant::now() + Duration::from_secs(20);
            loop {
                let loaded = match node_call("node/status").await {
                    Ok(s) => s["plugins"].as_array().into_iter().flatten().any(|p| p["name"] == name.as_str()),
                    Err(_) => false,
                };
                if !loaded {
                    break;
                }
                anyhow::ensure!(Instant::now() < until, "the node here still runs {name}");
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
        k => anyhow::ensure!(!services::running(k), "{} is still running here", k.describe()),
    }
    let _ = std::fs::remove_dir_all(&dir);
    print(&json!({"undone": u.files.len(), "stopped": true}))
}

/// The move passed its health check: mark the control plane's move proof confirmed, so its nodes save the new
/// address.
pub fn confirm(id: &str) -> anyhow::Result<()> {
    let dir = session_dir(id)?;
    let u: Undo = serde_json::from_slice(&std::fs::read(dir.join("undo.json"))?)?;
    let home = component::home();
    let mut n = 0;
    for f in u.files.iter().filter(|f| f.ends_with("/move-proof")) {
        let path = home.join(archive::safe_rel(f)?);
        let mut v: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        v["confirmed"] = json!(true);
        write_private(&path, &v.to_string())?;
        n += 1;
    }
    print(&json!({"confirmed": n}))
}

pub fn done(id: &str) -> anyhow::Result<()> {
    let _ = std::fs::remove_dir_all(session_dir(id)?);
    print(&json!({"done": true}))
}

/// A GET over plain TCP, without an HTTP client: the status code and body.
pub fn http_get(addr: &str, host: &str, path: &str) -> Option<(u16, String)> {
    use std::io::Write;
    use std::net::ToSocketAddrs;
    let addr = addr.replace("0.0.0.0:", "127.0.0.1:").replace("[::]:", "[::1]:");
    let sa = addr.to_socket_addrs().ok()?.next()?;
    let mut s = std::net::TcpStream::connect_timeout(&sa, Duration::from_secs(3)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    s.write_all(format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes()).ok()?;
    let mut out = vec![];
    s.read_to_end(&mut out).ok()?;
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n")?;
    let code = head.split(' ').nth(1)?.parse().ok()?;
    Some((code, body.to_string()))
}

/// Ask the node here over its socket.
pub async fn node_call(method: &str) -> anyhow::Result<Value> {
    let socket = fleetlayer_protocol::local::default_node_socket();
    let stream = tokio::time::timeout(Duration::from_secs(3), tokio::net::UnixStream::connect(&socket))
        .await
        .map_err(|_| anyhow::anyhow!("the node's socket didn't answer"))??;
    let (r, w) = stream.into_split();
    let (peer, task) = fleetlayer_plugin::connect(r, w, std::sync::Arc::new(super::NoRequests));
    let v = peer.request_within::<_, Value>(method, &json!({}), Duration::from_secs(10)).await;
    task.abort();
    v.map_err(|e| anyhow::anyhow!("{method}: {}", e.message))
}

/// Why it isn't healthy yet, or None when it is.
async fn unhealthy(kind: &Kind, req: &HealthRequest) -> Option<String> {
    match kind {
        Kind::Control | Kind::Gateway => {
            let listen = req.listen.as_deref()?;
            let host = if *kind == Kind::Gateway { "fleetlayer.localhost" } else { "localhost" };
            match tokio::task::block_in_place(|| http_get(listen, host, "/healthz")) {
                Some((c, _)) if (200..300).contains(&c) => None,
                Some((c, body)) => Some(format!("/healthz on {listen} answered {c}: {}", body.trim())),
                None => Some(format!("nothing answers on {listen} yet")),
            }
        }
        Kind::Node => {
            let s = match node_call("node/status").await {
                Ok(s) => s,
                Err(e) => return Some(format!("the node isn't answering yet ({e})")),
            };
            if let Some(n) = &req.node_name
                && s["node"] != n.as_str()
            {
                return Some(format!("the node calls itself {} here, not {n}", s["node"]));
            }
            if let Some(k) = &req.public_key
                && s["publicKey"] != k.as_str()
            {
                return Some(format!("the node's key is {}, not the one it had ({k})", s["publicKey"]));
            }
            if req.linked && s["control"]["connected"] != true {
                return Some(format!(
                    "the node isn't linked to the control plane yet ({})",
                    s["control"]["lastError"].as_str().unwrap_or("connecting")
                ));
            }
            None
        }
        Kind::Plugin(name) => {
            let s = match node_call("node/status").await {
                Ok(s) => s,
                Err(e) => return Some(format!("the node isn't answering yet ({e})")),
            };
            let p = s["plugins"].as_array().into_iter().flatten().find(|p| p["name"] == name.as_str());
            let Some(p) = p else { return Some(format!("the node here hasn't loaded {name}")) };
            let state = p["state"]["state"].as_str().or(p["state"].as_str()).unwrap_or("");
            if state != "running" {
                return Some(format!(
                    "{name} is {state} here{}",
                    p["problem"].as_str().map(|x| format!(": {x}")).unwrap_or_default()
                ));
            }
            let listen = req.listen.as_deref()?;
            let services = node_call("node/services").await.unwrap_or_default();
            let svc = services["services"].as_array().into_iter().flatten().find(|x| x["plugin"] == name.as_str());
            let Some(svc) = svc else { return Some(format!("{name} hasn't announced its web service yet")) };
            let health = svc["health"].as_str().unwrap_or("/");
            match tokio::task::block_in_place(|| http_get(listen, "localhost", health)) {
                Some((c, _)) if (200..400).contains(&c) => None,
                Some((c, _)) => Some(format!("{health} on {listen} answered {c}")),
                None => Some(format!("{name} doesn't answer on {listen} yet")),
            }
        }
    }
}

pub async fn health(id: &str) -> anyhow::Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let req: HealthRequest = serde_json::from_str(&input)?;
    let kind = session_kind(id)?;
    let until = Instant::now() + Duration::from_secs(req.timeout_secs.max(1));
    loop {
        match unhealthy(&kind, &req).await {
            None => {
                if std::env::var("FLEETLAYER_MOVE_TEST_FAIL").is_ok_and(|v| v == "health") {
                    // Give linked nodes time to reach the new copy first, the case undoing it must survive.
                    tokio::time::sleep(Duration::from_secs(12)).await;
                }
                test_fail("health")?;
                return print(&json!({"healthy": true}));
            }
            Some(why) if Instant::now() >= until => anyhow::bail!("not healthy after {}s: {why}", req.timeout_secs),
            Some(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

/// `move-receive service start|stop|running <component>`: the same service handling, on its own.
pub fn service(action: &str, component: &str) -> anyhow::Result<()> {
    let kind = Kind::from_id(component).or_else(|_| Ok::<_, anyhow::Error>(Kind::parse(component)))?;
    match action {
        "start" => services::start(&kind)?,
        "stop" => services::stop(&kind)?,
        "running" => return print(&json!({"running": services::running(&kind)})),
        _ => anyhow::bail!("service start|stop|running <component>"),
    }
    print(&json!({"ok": true}))
}
