//! fleetlayer-dashboard: Fleetlayer's web dashboard (`docs/dashboard.md`). Machines and their plugins, sessions, live
//! events, approvals and admin windows, readings, and a graph of the fleet, behind Fleetlayer's sign-in. It reads and
//! acts only through the control plane's API, with a token of its own: anything it shows or does, the CLI and the API
//! can do too.
//!
//! ```yaml
//! - name: dashboard
//!   command: fleetlayer-dashboard
//!   grants: [access.check]
//!   config:
//!     listen: 127.0.0.1:0
//!     base_url: https://dashboard.example.com
//!     control:
//!       url: https://control.example.com
//!       token_file: ~/.config/fleetlayer/dashboard/control-token
//!     signin:
//!       issuer: https://signin.example.com
//!       secret_file: ~/.config/fleetlayer/signin-clients/dashboard
//! ```
//!
//! Run by a node, it serves as long as the node keeps it running. `fleetlayer-dashboard serve <config.json>` runs the
//! same service on its own; `fleetlayer-dashboard demo` serves a made-up fleet on this machine, for trying it out and
//! for screenshots.

mod admin;
mod auth;
mod control;
mod demo;
mod graph;
mod icons;
mod model;
mod services;
mod views;
mod web;

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fleetlayer_plugin::{Handler, Peer, answer, params};
use fleetlayer_protocol::acp::{self, Capabilities, Implementation, InitializeRequest, InitializeResponse};
use fleetlayer_protocol::ext::{self, NodeMeta, PluginMeta, Roles, meta_key};
use fleetlayer_protocol::rpc::RpcError;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    listen: SocketAddr,
    /// The address people open (`https://dashboard.example.com`). Required with `signin`.
    #[serde(default)]
    base_url: Option<String>,
    /// The control plane this dashboard shows.
    control: ControlConfig,
    /// Sign people in through Fleetlayer's sign-in service (`docs/plugins/signin.md`).
    #[serde(default)]
    signin: Option<Signin>,
    /// A login key's file (at least 32 random characters), for a dashboard without the sign-in service. Whoever has
    /// the key may do everything the dashboard's token allows.
    #[serde(default)]
    login_file: Option<PathBuf>,
    #[serde(default)]
    trusted_proxies: Vec<IpAddr>,
    #[serde(default)]
    heartbeat: Option<Heartbeat>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlConfig {
    /// The control plane's API address (`https://control.example.com`, or `http://127.0.0.1:7600` on one machine).
    url: String,
    /// A token from `fleetlayer-control token add dashboard --scope delegate`, so each signed-in person decides as
    /// themselves (or `admin` for a login key only, `read` to only look), in a file only this Unix user reads.
    token_file: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signin {
    issuer: String,
    /// The client's name in the sign-in service's `clients`, and `service:<name>` in the policy.
    #[serde(default = "dashboard_client")]
    client: String,
    secret_file: PathBuf,
}

fn dashboard_client() -> String {
    "dashboard".into()
}

/// Pushed every minute (Gatus external-endpoint style), failing when the control plane can't be read with the
/// dashboard's token: the dashboard would show nothing then, though it still answers.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Heartbeat {
    url: String,
    token_file: PathBuf,
}

fn expand(home: &Path, p: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => p.to_path_buf(),
    }
}

fn read_secret(home: &Path, f: &Path, what: &str) -> anyhow::Result<String> {
    let f = expand(home, f);
    let s = std::fs::read_to_string(&f).map_err(|e| anyhow::anyhow!("reading {what} {}: {e}", f.display()))?;
    let s = s.trim().to_string();
    anyhow::ensure!(!s.is_empty(), "{what} {} is empty", f.display());
    Ok(s)
}

fn check_base_url(base: Option<&str>) -> anyhow::Result<()> {
    if let Some(b) = base.and_then(|b| b.strip_prefix("http://")) {
        let host = b.split(['/', ':']).next().unwrap_or_default();
        anyhow::ensure!(
            host == "localhost" || host.ends_with(".localhost") || host == "127.0.0.1",
            "base_url uses plain http, which is only allowed on this machine (localhost); use https behind a proxy"
        );
    }
    Ok(())
}

fn build(
    cfg: Config,
    home: &Path,
    peer: Option<&Peer>,
) -> anyhow::Result<(Arc<web::App>, SocketAddr, Option<Heartbeat>)> {
    check_base_url(cfg.base_url.as_deref())?;
    let token = read_secret(home, &cfg.control.token_file, "the control plane token")?;
    let source = model::Source::Control(control::Control::new(&cfg.control.url, &token)?);
    let key = match &cfg.login_file {
        Some(f) => Some(auth::LoginKey::new(read_secret(home, f, "the login file")?)?),
        None => None,
    };
    let oidc = match &cfg.signin {
        Some(s) => {
            let base = cfg.base_url.as_deref().ok_or_else(|| anyhow::anyhow!("signin needs base_url"))?;
            let secret = read_secret(home, &s.secret_file, "the sign-in client secret")?;
            Some(fleetlayer_oidc::Oidc::new(&s.issuer, &s.client, &secret, base)?)
        }
        None => None,
    };
    let checker = peer.map(|p| auth::Checker::Node(p.clone()));
    let client = cfg.signin.as_ref().map(|s| s.client.clone()).unwrap_or_else(dashboard_client);
    let mut app = web::App::new(
        source,
        auth::Auth::new(key, oidc, checker, client)?,
        cfg.base_url.map(|b| b.trim_end_matches('/').to_string()),
        cfg.trusted_proxies,
    );
    app.issuer = cfg.signin.as_ref().map(|s| s.issuer.trim_end_matches('/').to_string());
    let heartbeat = cfg.heartbeat.map(|h| Heartbeat { token_file: expand(home, &h.token_file), ..h });
    Ok((Arc::new(app), cfg.listen, heartbeat))
}

async fn beat(hb: &Heartbeat, problem: Option<&str>) {
    let token = match std::fs::read_to_string(&hb.token_file) {
        Ok(t) => t,
        Err(e) => return tracing::warn!("reading the heartbeat token {}: {e}", hb.token_file.display()),
    };
    let mut url = format!("{}{}success={}", hb.url, if hb.url.contains('?') { '&' } else { '?' }, problem.is_none());
    if let Some(p) = problem {
        url.push_str("&error=");
        url.push_str(&form_urlencoded::byte_serialize(p.as_bytes()).collect::<String>());
    }
    let res = reqwest::Client::new().post(&url).bearer_auth(token.trim()).timeout(Duration::from_secs(20)).send().await;
    if let Err(e) = res.and_then(|r| r.error_for_status()) {
        tracing::warn!("the heartbeat: {e}");
    }
}

fn loopback_if_any(a: SocketAddr) -> SocketAddr {
    if a.ip().is_unspecified() { SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), a.port()) } else { a }
}

/// The web service and its heartbeat, until the web service stops. Under a node, it tells the node where it listens,
/// so a gateway routes `dashboard.<domain>` to it.
async fn run(
    app: Arc<web::App>,
    listen: SocketAddr,
    heartbeat: Option<Heartbeat>,
    peer: Option<Peer>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let bound = loopback_if_any(listener.local_addr()?);
    tracing::info!(%bound, "dashboard listening");
    if let Some(peer) = &peer {
        peer.notify(
            ext::method::SERVICE_LISTENING,
            &ext::ServiceListening {
                name: "dashboard".into(),
                url: format!("http://{bound}"),
                health: Some("/healthz".into()),
                hosts: vec![],
            },
        );
    }
    let a = app.clone();
    tokio::spawn(async move {
        let mut last: Option<String> = None;
        loop {
            a.refresh_scope().await;
            let problem = a.problem().await;
            if problem != last {
                match &problem {
                    Some(p) => tracing::warn!("{p}"),
                    None if last.is_some() => tracing::info!("the control plane answers again"),
                    None => {}
                }
                last = problem.clone();
            }
            // Every minute, so the node notices if this loop stops (it says so after a silence).
            if let Some(peer) = &peer {
                peer.notify(
                    ext::method::PLUGIN_STATUS,
                    &ext::PluginStatusReport { problem: problem.clone(), detail: json!({}) },
                );
            }
            if let Some(hb) = &heartbeat {
                beat(hb, problem.as_deref()).await;
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
    web::serve(app, listener).await
}

#[derive(Default)]
struct Plugin {
    started: Mutex<bool>,
}

struct Shared(Arc<Plugin>);

#[async_trait]
impl Handler for Shared {
    async fn request(&self, peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
        match method {
            acp::method::INITIALIZE => {
                let req: InitializeRequest = params(p)?;
                let node: NodeMeta = ext::get_meta(req.client_capabilities.meta.as_ref(), meta_key::NODE)
                    .map_err(RpcError::invalid_params)?
                    .ok_or_else(|| RpcError::refused("fleetlayer-dashboard runs under a Fleetlayer node"))?;
                let cfg: Config = serde_json::from_value(node.config.clone()).map_err(RpcError::invalid_params)?;
                let (app, listen, heartbeat) =
                    build(cfg, Path::new(&node.home), Some(peer)).map_err(|e| RpcError::refused(e.to_string()))?;
                let mut started = self.0.started.lock().await;
                if !*started {
                    *started = true;
                    let peer = peer.clone();
                    tokio::spawn(async move {
                        if let Err(e) = run(app, listen, heartbeat, Some(peer)).await {
                            // Exiting makes the node restart the plugin, with backoff, and report it.
                            tracing::error!("the web service stopped: {e}");
                            std::process::exit(1);
                        }
                    });
                }
                answer(&InitializeResponse {
                    protocol_version: fleetlayer_protocol::ACP_PROTOCOL,
                    agent_capabilities: Capabilities {
                        meta: Some(ext::meta_with(
                            meta_key::PLUGIN,
                            &PluginMeta { protocol: fleetlayer_protocol::PROTOCOL, roles: Roles::default() },
                        )),
                        ..Default::default()
                    },
                    agent_info: Some(Implementation {
                        name: "fleetlayer-dashboard".into(),
                        title: Some("Dashboard".into()),
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

const USAGE: &str = "usage: fleetlayer-dashboard [serve <config.json> | demo [--listen <addr>]]";

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FLEETLAYER_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let result = match args.first().map(String::as_str) {
        Some("serve") => match args.get(1) {
            Some(path) => {
                async {
                    let cfg: Config = serde_json::from_slice(&std::fs::read(path)?)?;
                    let (app, listen, heartbeat) = build(cfg, &home, None)?;
                    run(app, listen, heartbeat, None).await
                }
                .await
            }
            None => {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
        },
        Some("demo") => {
            let listen = match args.iter().position(|a| a == "--listen") {
                Some(i) => match args.get(i + 1).map(|a| a.parse::<SocketAddr>()) {
                    Some(Ok(a)) => a,
                    _ => {
                        eprintln!("{USAGE}");
                        std::process::exit(2);
                    }
                },
                None => SocketAddr::from(([127, 0, 0, 1], 7690)),
            };
            if !listen.ip().is_loopback() {
                eprintln!("fleetlayer-dashboard demo serves without sign-in, so only on loopback (127.0.0.1)");
                std::process::exit(2);
            }
            let app = Arc::new(web::App::new(
                model::Source::Demo(demo::Demo::new()),
                auth::Auth::open(),
                Some(format!("http://{listen}")),
                vec![],
            ));
            println!("A made-up fleet at http://{listen}/");
            run(app, listen, None, None).await
        }
        Some("--version") => {
            println!("fleetlayer-dashboard {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => {
            eprintln!("fleetlayer-dashboard: unknown command {other:?}\n{USAGE}");
            std::process::exit(2);
        }
        None => {
            fleetlayer_plugin::serve_stdio(Arc::new(Shared(Arc::new(Plugin::default())))).await;
            Ok(())
        }
    };
    if let Err(e) = result {
        eprintln!("fleetlayer-dashboard: {e}");
        std::process::exit(1);
    }
}
