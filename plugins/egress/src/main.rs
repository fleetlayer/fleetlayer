//! fleetlayer-egress: network access per role (`docs/egress.md`). A restricted role's Unix users reach the internet
//! only through this proxy, which reads the host each connection asks for (CONNECT, the TLS server name, or the HTTP
//! `Host` header) without decrypting anything, allows what's on the role's list, logs every connection, and asks
//! Fleetlayer's approvals for hosts an agent needs. The firewall rules that send the role's traffic here come from
//! `fleetlayer-egress firewall`.
//!
//! Run with no arguments, it's a node plugin, on a node whose Unix user isn't in any restricted role:
//!
//! ```yaml
//! - name: egress
//!   command: fleetlayer-egress
//!   grants: [access.check]
//!   config:                          # all optional
//!     settings: ~/.config/fleetlayer/egress.yaml
//!     state_dir: ~/.local/state/fleetlayer/egress
//! ```
//!
//! With `config: {probe: {...}}` on a restricted role's own node, it checks instead that the rules hold for that role.

mod audit;
mod dns;
mod engine;
mod firewall;
mod hosts;
mod probe;
mod procnet;
mod proxy;
mod settings;
mod sni;
mod state;
#[cfg(test)]
mod tests;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use clap::{Parser, Subcommand};
use fleetlayer_plugin::{Handler, Peer, answer, params};
use fleetlayer_protocol::acp::{self, Capabilities, Implementation, InitializeRequest, InitializeResponse};
use fleetlayer_protocol::ext::{self, NodeMeta, PluginMeta, PluginStatusReport, Roles, meta_key};
use fleetlayer_protocol::link::{AccessCheck, AccessDecision};
use fleetlayer_protocol::rpc::RpcError;
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::engine::{Answer, Asker, Engine};

#[derive(Parser, Debug)]
#[command(name = "fleetlayer-egress", version, about = "Network access per role: a host-name proxy, DNS and firewall")]
struct Args {
    /// The proxy, for the commands an agent runs (status, ask, why).
    #[arg(long, global = true, env = "FLEETLAYER_EGRESS_PROXY", default_value = "127.0.0.1:3128")]
    proxy: String,
    /// The settings file (default ~/.config/fleetlayer/egress.yaml).
    #[arg(long, global = true)]
    settings: Option<PathBuf>,
    /// The state directory with the audit log (default ~/.local/state/fleetlayer/egress).
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Your role, its mode, and the hosts it may reach (asks the proxy).
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Ask for a host your work needs. A person approves it, and it stays allowed.
    Ask {
        /// The host: a name (443 and 80), name:port, or an IP address.
        host: String,
        /// What it's for, in one line (16 to 1000 characters): the person reads it.
        #[arg(long)]
        why: String,
        /// Wait up to this many minutes for the answer.
        #[arg(long, default_value_t = 0)]
        wait: u64,
    },
    /// Why your connections to a host were allowed or refused, newest first.
    Why { host: Option<String> },
    /// The environment lines that point command-line tools at the proxy.
    Env,
    /// The audit log (on the proxy's machine, as its user).
    Log {
        #[arg(long, default_value_t = 1)]
        days: u32,
        #[arg(long)]
        role: Option<String>,
        /// Only refusals (and what log mode would refuse).
        #[arg(long)]
        refused: bool,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// The hosts each role would need added, from the audit log, as settings to review and merge.
    Suggest {
        #[arg(long, default_value_t = 7)]
        days: u32,
        #[arg(long)]
        role: Option<String>,
    },
    /// The nftables rules for the roles in the settings: print them, apply them (as root), or remove them.
    Firewall {
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        remove: bool,
        /// Print a systemd unit that applies the rules at boot.
        #[arg(long)]
        unit: bool,
    },
    /// Read a settings file and say what it allows.
    Check { file: Option<PathBuf> },
    /// The built-in presets and their hosts.
    Presets,
    /// Run the proxy in the foreground without a node, to try it out: asking for hosts needs the plugin.
    Serve,
    /// Run the probe's checks once, as this user, and print them.
    Probe {
        #[arg(long, default_value = "api.anthropic.com")]
        allowed: String,
        #[arg(long, default_value = "example.com")]
        blocked: String,
        #[arg(long, default_value = "1.1.1.1:853")]
        direct: String,
    },
}

/// The same rules as the control plane's justifications: one line of plain text, 16 to 1000 characters.
pub fn check_why(text: &str) -> Result<(), String> {
    if text.chars().any(|c| c.is_control()) {
        return Err("--why is one line of plain text".into());
    }
    let n = text.trim().chars().count();
    if n < 16 {
        return Err(format!("--why is too short ({n} characters, at least 16): say what the host is for"));
    }
    if n > 1000 {
        return Err(format!("--why is too long ({n} characters, at most 1000)"));
    }
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    settings: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    probe: Option<probe::ProbeConfig>,
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn expand(home: &Path, p: PathBuf) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => p,
    }
}

fn default_settings(home: &Path) -> PathBuf {
    home.join(".config/fleetlayer/egress.yaml")
}

fn default_state_dir(home: &Path) -> PathBuf {
    home.join(".local/state/fleetlayer/egress")
}

struct NodeAsker(Peer);

#[async_trait]
impl Asker for NodeAsker {
    async fn ask(&self, node: &str, target: &str, why: &str) -> Result<Answer, String> {
        let req = AccessCheck {
            tenant: String::new(),
            who: format!("-@{node}"),
            can: "connect".into(),
            to: target.to_string(),
            why: Some(why.to_string()),
        };
        self.0
            .request_within::<_, AccessDecision>(ext::method::ACCESS_CHECK, &req, Duration::from_secs(15))
            .await
            .map(|d| engine::answer_from(&d))
            .map_err(|e| e.message)
    }
}

#[derive(Default)]
struct Plugin {
    started: Mutex<bool>,
}

struct Shared(Arc<Plugin>);

fn read_settings(path: &Path) -> Result<(String, settings::Settings), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let s = settings::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((text, s))
}

/// Bind one port on 127.0.0.1 and, where the machine has IPv6, on ::1 (redirected IPv6 connections arrive there).
async fn bind_tcp(port: u16) -> anyhow::Result<Vec<tokio::net::TcpListener>> {
    let v4 = tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
        .await
        .map_err(|e| anyhow::anyhow!("can't listen on 127.0.0.1:{port}: {e}"))?;
    let mut out = vec![v4];
    if let Ok(v6) = tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port)).await {
        out.push(v6);
    }
    Ok(out)
}

async fn bind_udp(port: u16) -> anyhow::Result<Vec<tokio::net::UdpSocket>> {
    let v4 = tokio::net::UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
        .await
        .map_err(|e| anyhow::anyhow!("can't listen on 127.0.0.1:{port} (udp): {e}"))?;
    let mut out = vec![v4];
    if let Ok(v6) = tokio::net::UdpSocket::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port)).await {
        out.push(v6);
    }
    Ok(out)
}

/// Start everything: the listeners, the resolver, the settings watch and the rechecks. Returns once bound.
pub async fn start(engine: Arc<Engine>, settings_path: PathBuf, peer: Option<Peer>) -> anyhow::Result<()> {
    let s = engine.settings().await;
    let ports = s.ports.clone();
    let proxy_l = bind_tcp(ports.proxy).await?;
    let tls_l = bind_tcp(ports.tls).await?;
    let http_l = bind_tcp(ports.http).await?;
    let dns_tcp = bind_tcp(ports.dns).await?;
    let dns_udp = bind_udp(ports.dns).await?;
    let own: Vec<SocketAddr> =
        proxy_l.iter().chain(&tls_l).chain(&http_l).filter_map(|l| l.local_addr().ok()).collect();
    let server = proxy::Server { engine: engine.clone(), who: proxy::proc_who(engine.clone()), own: Arc::new(own) };
    for (ls, kind) in [(proxy_l, proxy::Kind::Proxy), (tls_l, proxy::Kind::Tls), (http_l, proxy::Kind::Http)] {
        for l in ls {
            tokio::spawn(server.clone().serve(l, kind));
        }
    }
    let users = engine.clone();
    let users_tcp = engine.clone();
    let resolver = dns::Resolver {
        engine: engine.clone(),
        who: Arc::new(move |peer| {
            let uid = procnet::udp(peer)?;
            Some(users.users.lock().ok()?.name(uid).unwrap_or(format!("uid {uid}")))
        }),
        who_tcp: Arc::new(move |peer| {
            let o = procnet::tcp(peer)?;
            Some(users_tcp.users.lock().ok()?.name(o.uid).unwrap_or(format!("uid {}", o.uid)))
        }),
    };
    for l in dns_tcp {
        tokio::spawn(resolver.clone().serve_tcp(l));
    }
    for u in dns_udp {
        tokio::spawn(resolver.clone().serve_udp(u));
    }
    // The settings file, by content every few seconds; the rechecks every minute; the status every minute.
    let e = engine.clone();
    tokio::spawn(async move {
        let mut last = std::fs::read_to_string(&settings_path).unwrap_or_default();
        let mut tick = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            tick += 1;
            let text = std::fs::read_to_string(&settings_path).unwrap_or_default();
            if text != last {
                last = text.clone();
                match read_settings(&settings_path) {
                    Ok((_, s)) => {
                        let ports_changed = s.ports.proxy != ports.proxy
                            || s.ports.tls != ports.tls
                            || s.ports.http != ports.http
                            || s.ports.dns != ports.dns;
                        e.log.lock().await.set_keep_days(s.keep_days);
                        *e.settings.write().await = Arc::new(s);
                        *e.settings_problem.lock().await = ports_changed
                            .then(|| "the ports changed: restart the egress plugin to listen on them".to_string());
                        tracing::info!("read {} again", settings_path.display());
                    }
                    Err(err) => {
                        tracing::warn!("{err}");
                        *e.settings_problem.lock().await = Some(format!(
                            "the settings file has a mistake, so the last good settings still decide: {err}"
                        ));
                    }
                }
                e.save_state().await;
            }
            if tick.is_multiple_of(12) {
                e.recheck().await;
                e.save_state().await;
                if let Some(p) = &peer {
                    report(&e, p).await;
                }
            }
        }
    });
    engine.save_state().await;
    Ok(())
}

async fn report(e: &Engine, peer: &Peer) {
    let mut problem = e.settings_problem.lock().await.clone();
    let failures = *e.dns_failures.lock().await;
    if problem.is_none() && failures >= 5 {
        problem = Some(format!("{failures} DNS questions in a row got no answer from the upstream resolvers"));
    }
    let settings = e.settings().await;
    if problem.is_none()
        && let Ok(me) = std::env::var("USER")
        && let Some(r) = settings.role_of_user(&me)
    {
        problem = Some(format!(
            "the egress plugin runs as {me}, a user of role {}: run it as a user outside every restricted role",
            r.name
        ));
    }
    let state = e.state.lock().await;
    let pending: usize = state.roles.values().map(|r| r.pending.len()).sum();
    let detail = json!({
        "roles": settings.roles.iter().map(|r| json!({"name": r.name, "mode": r.mode.word(), "users": r.users})).collect::<Vec<_>>(),
        "pending_asks": pending,
    });
    peer.notify(ext::method::PLUGIN_STATUS, &PluginStatusReport { problem, detail });
}

async fn run_probe(c: probe::ProbeConfig, peer: Peer) {
    tokio::time::sleep(Duration::from_secs(5)).await;
    loop {
        if peer.is_closed() {
            return;
        }
        let r = probe::run_once(&c).await;
        let problem = r.problem();
        if let Some(p) = &problem {
            tracing::warn!("{p}");
        }
        let detail = serde_json::to_value(&r).unwrap_or_default();
        peer.notify(ext::method::PLUGIN_STATUS, &PluginStatusReport { problem, detail });
        tokio::time::sleep(Duration::from_secs(c.every.max(30))).await;
    }
}

#[async_trait]
impl Handler for Shared {
    async fn request(&self, peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
        match method {
            acp::method::INITIALIZE => {
                let req: InitializeRequest = params(p)?;
                let node: NodeMeta = ext::get_meta(req.client_capabilities.meta.as_ref(), meta_key::NODE)
                    .map_err(RpcError::invalid_params)?
                    .ok_or_else(|| RpcError::refused("fleetlayer-egress runs under a Fleetlayer node"))?;
                let home = PathBuf::from(&node.home);
                let config: Config = serde_json::from_value(node.config.clone()).map_err(RpcError::invalid_params)?;
                let mut started = self.0.started.lock().await;
                if !*started {
                    if let Some(pc) = config.probe {
                        tokio::spawn(run_probe(pc, peer.clone()));
                    } else {
                        let settings_path =
                            config.settings.map_or_else(|| default_settings(&home), |p| expand(&home, p));
                        let state_dir = config.state_dir.map_or_else(|| default_state_dir(&home), |p| expand(&home, p));
                        let (_, s) = read_settings(&settings_path).map_err(RpcError::invalid_params)?;
                        let engine = Engine::new(s, state_dir);
                        *engine.asker.write().await = Arc::new(NodeAsker(peer.clone()));
                        start(engine.clone(), settings_path, Some(peer.clone())).await.map_err(RpcError::internal)?;
                        let (e, p2) = (engine, peer.clone());
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            report(&e, &p2).await;
                        });
                    }
                    *started = true;
                }
                let meta = PluginMeta { protocol: fleetlayer_protocol::PROTOCOL, roles: Roles::default() };
                answer(&InitializeResponse {
                    protocol_version: fleetlayer_protocol::ACP_PROTOCOL,
                    agent_capabilities: Capabilities {
                        meta: Some(ext::meta_with(meta_key::PLUGIN, &meta)),
                        ..Default::default()
                    },
                    agent_info: Some(Implementation {
                        name: "fleetlayer-egress".into(),
                        title: Some("Egress".into()),
                        version: env!("CARGO_PKG_VERSION").into(),
                    }),
                    auth_methods: vec![],
                })
            }
            ext::method::PING => Ok(json!({})),
            _ => Err(RpcError::method_not_found(method)),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let home = home();
    let settings_path = args.settings.clone().map_or_else(|| default_settings(&home), |p| expand(&home, p));
    let state_dir = args.state_dir.clone().map_or_else(|| default_state_dir(&home), |p| expand(&home, p));
    match args.command {
        None => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
                )
                .init();
            fleetlayer_plugin::serve_stdio(Arc::new(Shared(Arc::new(Plugin::default())))).await;
            Ok(())
        }
        Some(Command::Serve) => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
                )
                .init();
            let (_, s) = read_settings(&settings_path).map_err(anyhow::Error::msg)?;
            let p = s.ports.clone();
            let engine = Engine::new(s, state_dir);
            start(engine, settings_path, None).await?;
            eprintln!(
                "fleetlayer-egress: proxy on 127.0.0.1:{}, TLS {}, HTTP {}, DNS {} (no node: asking for hosts is off)",
                p.proxy, p.tls, p.http, p.dns
            );
            std::future::pending::<()>().await;
            Ok(())
        }
        Some(Command::Status { json }) => status(&args.proxy, json).await,
        Some(Command::Ask { host, why, wait }) => ask(&args.proxy, &host, &why, wait).await,
        Some(Command::Why { host }) => why(&args.proxy, host.as_deref()).await,
        Some(Command::Env) => {
            let p = &args.proxy;
            println!(
                "export HTTPS_PROXY=http://{p} HTTP_PROXY=http://{p} https_proxy=http://{p} http_proxy=http://{p}"
            );
            println!("export NO_PROXY=localhost,127.0.0.1,::1 no_proxy=localhost,127.0.0.1,::1");
            Ok(())
        }
        Some(Command::Log { days, role, refused, limit, json }) => {
            log(&state_dir, days, role.as_deref(), refused, limit, json)
        }
        Some(Command::Suggest { days, role }) => {
            let (_, s) = read_settings(&settings_path).map_err(anyhow::Error::msg)?;
            let lines = audit::read(&state_dir.join("audit"), days, Timestamp::now());
            if lines.is_empty() {
                eprintln!(
                    "No audit log under {} for the last {days} days: run this as the proxy's user, on its machine.",
                    state_dir.join("audit").display()
                );
            }
            print!("{}", audit::suggest(&lines, &s, role.as_deref()));
            Ok(())
        }
        Some(Command::Firewall { apply, remove, unit }) => firewall_cmd(&settings_path, apply, remove, unit),
        Some(Command::Check { file }) => check(&file.unwrap_or(settings_path)),
        Some(Command::Presets) => {
            for (name, about, hosts) in settings::PRESETS {
                println!("{name}: {about}\n  {}", hosts.join(", "));
            }
            Ok(())
        }
        Some(Command::Probe { allowed, blocked, direct }) => {
            let c = probe::ProbeConfig { proxy: args.proxy.clone(), allowed, blocked, direct, every: 300 };
            let r = probe::run_once(&c).await;
            println!("role: {}, mode: {}", r.role.as_deref().unwrap_or("none"), r.mode.as_deref().unwrap_or("unknown"));
            for (name, ok, detail) in &r.checks {
                println!("  {} {name}: {detail}", if *ok { "ok  " } else { "FAIL" });
            }
            if r.problem().is_some() {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

async fn api_get(proxy: &str, path: &str) -> anyhow::Result<Value> {
    let req = format!("GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", proxy::API_HOST);
    let (code, body) = probe::http(proxy, &req).await.map_err(|e| {
        anyhow::anyhow!("the egress proxy at {proxy} isn't answering ({e}): is egress set up on this machine?")
    })?;
    anyhow::ensure!(code == 200, "the egress proxy answered {code}: {}", body.trim());
    Ok(serde_json::from_str(&body)?)
}

async fn status(proxy: &str, as_json: bool) -> anyhow::Result<()> {
    let v = api_get(proxy, "/status").await?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    let Some(role) = v["role"].as_str() else {
        println!("{} isn't in a restricted role: its connections aren't filtered.", v["user"].as_str().unwrap_or("?"));
        return Ok(());
    };
    println!(
        "user {} is in role {role}, mode {}",
        v["user"].as_str().unwrap_or("?"),
        v["mode"].as_str().unwrap_or("?")
    );
    if v["mode"] == "log" {
        println!("(log mode: nothing is refused yet; what would be is recorded)");
    }
    let list = |k: &str| v[k].as_array().cloned().unwrap_or_default();
    let presets: Vec<String> = list("presets").iter().filter_map(|p| p.as_str().map(str::to_string)).collect();
    if !presets.is_empty() {
        println!("presets: {}", presets.join(", "));
    }
    let listed: Vec<String> = list("hosts")
        .iter()
        .filter(|h| h["from"] == "listed")
        .filter_map(|h| h["host"].as_str().map(str::to_string))
        .collect();
    if !listed.is_empty() {
        println!("hosts: {}", listed.join(", "));
    }
    let approved: Vec<String> = list("approved").iter().filter_map(|h| h.as_str().map(str::to_string)).collect();
    if !approved.is_empty() {
        println!("approved: {}", approved.join(", "));
    }
    for p in list("pending") {
        println!(
            "waiting for approval: {}{}",
            p["target"].as_str().unwrap_or("?"),
            p["request"].as_str().map(|r| format!(" (request {r})")).unwrap_or_default()
        );
    }
    if v["node"].is_null() {
        println!("This role has no node in the egress settings, so `fleetlayer-egress ask` can't reach a person.");
    }
    Ok(())
}

async fn ask(proxy: &str, host: &str, why: &str, wait: u64) -> anyhow::Result<()> {
    check_why(why).map_err(anyhow::Error::msg)?;
    let body = serde_json::to_string(&json!({"host": host, "why": why}))?;
    let req = format!(
        "POST /ask HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        proxy::API_HOST,
        body.len()
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(wait * 60);
    loop {
        let (code, text) = probe::http(proxy, &req)
            .await
            .map_err(|e| anyhow::anyhow!("the egress proxy at {proxy} isn't answering ({e})"))?;
        anyhow::ensure!(code == 200, "the egress proxy answered {code}: {}", text.trim());
        let v: Value = serde_json::from_str(&text)?;
        let st = v["status"].as_str().unwrap_or("");
        if st != "waiting" || std::time::Instant::now() >= deadline {
            println!("{}", v["message"].as_str().unwrap_or(""));
            return match st {
                "allowed" | "already" => Ok(()),
                "waiting" => std::process::exit(3),
                _ => std::process::exit(1),
            };
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

async fn why(proxy: &str, host: Option<&str>) -> anyhow::Result<()> {
    let path = match host {
        Some(h) => format!("/why?host={}", h.replace(['&', ' ', '#', '?'], "")),
        None => "/why".into(),
    };
    let v = api_get(proxy, &path).await?;
    let lines = v.as_array().cloned().unwrap_or_default();
    if lines.is_empty() {
        println!("No recent decisions{} for your user.", host.map(|h| format!(" about {h}")).unwrap_or_default());
    }
    for l in lines {
        let port = l["port"].as_u64().map(|p| format!(":{p}")).unwrap_or_default();
        println!(
            "{} {} {}{} via {}: {}",
            l["at"].as_str().unwrap_or(""),
            l["decision"].as_str().unwrap_or(""),
            l["host"].as_str().unwrap_or(""),
            port,
            l["via"].as_str().unwrap_or(""),
            l["reason"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

fn log(dir: &Path, days: u32, role: Option<&str>, refused: bool, limit: usize, as_json: bool) -> anyhow::Result<()> {
    let state = state::State::load(&dir.join("state.json"));
    if let Some(at) = state.updated_at
        && Timestamp::now().duration_since(at).as_secs() > 180
    {
        eprintln!("The egress plugin last saved its state at {at}: it may not be running.");
    }
    if let Some(p) = &state.problem {
        eprintln!("Problem: {p}");
    }
    let lines: Vec<audit::Line> = audit::read(&dir.join("audit"), days, Timestamp::now())
        .into_iter()
        .filter(|l| role.is_none_or(|r| l.role.as_deref() == Some(r)) && (!refused || l.refused()))
        .collect();
    let skip = lines.len().saturating_sub(limit);
    for l in &lines[skip..] {
        if as_json {
            println!("{}", serde_json::to_string(l)?);
            continue;
        }
        let port = l.port.map(|p| format!(":{p}")).unwrap_or_default();
        let bytes =
            if l.up + l.down > 0 { format!(" ({} up, {} down, {} ms)", l.up, l.down, l.ms) } else { String::new() };
        println!(
            "{} {} {} [{}] {} {}{} via {}: {}{bytes}",
            l.at.strftime("%Y-%m-%d %H:%M:%S"),
            l.decision,
            l.user,
            l.role.as_deref().unwrap_or("-"),
            if l.via == "dns" { "resolve" } else { "connect" },
            l.host,
            port,
            l.via,
            l.reason
        );
    }
    Ok(())
}

fn firewall_cmd(settings_path: &Path, apply: bool, remove: bool, unit: bool) -> anyhow::Result<()> {
    if unit {
        let bin = std::env::current_exe()?.display().to_string();
        print!("{}", firewall::unit(&bin, &settings_path.display().to_string()));
        return Ok(());
    }
    if remove {
        let st = std::process::Command::new("nft").args(["delete", "table", "inet", firewall::TABLE]).status()?;
        anyhow::ensure!(st.success(), "nft delete table failed (run as root; maybe it isn't there)");
        println!("Removed the {} table.", firewall::TABLE);
        return Ok(());
    }
    let (_, s) = read_settings(settings_path).map_err(anyhow::Error::msg)?;
    let script = firewall::script(&s, &settings_path.display().to_string());
    if !apply {
        print!("{script}");
        return Ok(());
    }
    let mut child = std::process::Command::new("nft").args(["-f", "-"]).stdin(std::process::Stdio::piped()).spawn()?;
    {
        use std::io::Write;
        child.stdin.take().expect("stdin").write_all(script.as_bytes())?;
    }
    anyhow::ensure!(
        child.wait()?.success(),
        "nft refused the rules (run as root; `fleetlayer-egress firewall` prints them)"
    );
    let roles: Vec<String> = s.roles.iter().map(|r| format!("{} ({})", r.name, r.mode.word())).collect();
    println!("Applied the {} table for {}.", firewall::TABLE, roles.join(", "));
    Ok(())
}

fn check(file: &Path) -> anyhow::Result<()> {
    let (text, s) = read_settings(file).map_err(anyhow::Error::msg)?;
    if text.trim().is_empty() {
        println!("{}: empty or missing: no roles, so nothing is restricted.", file.display());
        return Ok(());
    }
    println!(
        "{}: mode {}, proxy port {}, TLS {}, HTTP {}, DNS {}; audit kept {} days",
        file.display(),
        s.mode.word(),
        s.ports.proxy,
        s.ports.tls,
        s.ports.http,
        s.ports.dns,
        s.keep_days
    );
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    for r in &s.roles {
        println!();
        println!("role {} ({})", r.name, r.mode.word());
        println!("  users:    {}", r.users.join(", "));
        for u in &r.users {
            if procnet::Users::uid_of(&passwd, u).is_none() {
                println!("  warning:  there's no Unix user {u} on this machine");
            }
        }
        match &r.node {
            Some(n) => println!("  node:     {n} (asks for hosts as -@{n})"),
            None => println!("  node:     none: its agents can't ask for hosts"),
        }
        if !r.presets.is_empty() {
            println!("  presets:  {}", r.presets.join(", "));
        }
        let listed: Vec<String> =
            r.patterns.iter().filter(|(_, s)| *s == settings::Source::Listed).map(|(p, _)| p.to_string()).collect();
        println!("  hosts:    {}", if listed.is_empty() { "none".into() } else { listed.join(", ") });
        if !r.networks.is_empty() {
            let n: Vec<String> = r.networks.iter().map(|c| format!("{}/{}", c.addr, c.prefix)).collect();
            println!("  networks: {}", n.join(", "));
        }
        println!("  loopback: {}", if r.loopback { "allowed" } else { "only the proxy" });
    }
    if let Ok(me) = std::env::var("USER")
        && let Some(r) = s.role_of_user(&me)
    {
        println!("\nwarning: you ({me}) are in role {}: the proxy must run as a user outside every role.", r.name);
    }
    Ok(())
}
