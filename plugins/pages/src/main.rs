//! fleetlayer-pages: pages for people (QA screenshots, reports, mock-ups), written by any agent as plain folders in a
//! synced vault, hosted with access per space. `docs/plugins/pages.md` describes the format and the service.
//!
//! ```yaml
//! - name: pages
//!   command: fleetlayer-pages
//!   config:
//!     listen: 10.0.0.5:7670
//!     spaces:
//!       personal: { dir: /srv/pages/personal, owner: "person:alice" }
//!     login_file: /etc/fleetlayer-pages/login
//! ```
//!
//! Run by a node, it serves the web app as long as the node keeps it running. `fleetlayer-pages serve <config.json>`
//! runs the same service on its own (the plugin config as JSON), for trying changes locally.

mod auth;
mod render;
mod space;
mod web;

use std::collections::BTreeMap;
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

use crate::space::{Cache, SpaceConfig};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    listen: SocketAddr,
    /// The address people open, for the links the service prints (optional).
    #[serde(default)]
    #[allow(dead_code)]
    base_url: Option<String>,
    spaces: BTreeMap<String, SpaceConfig>,
    login_file: PathBuf,
    #[serde(default)]
    trusted_proxies: Vec<IpAddr>,
    #[serde(default)]
    login_hint: Option<String>,
    #[serde(default)]
    heartbeat: Option<Heartbeat>,
}

/// Pushed every minute (Gatus external-endpoint style: `?success=true|false&error=…`, with a bearer token), failing
/// when a space's folder or marker can't be read, so a share that has dropped alerts.
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

fn build(mut cfg: Config, home: &Path) -> anyhow::Result<(Arc<web::App>, SocketAddr, Option<Heartbeat>)> {
    anyhow::ensure!(!cfg.spaces.is_empty(), "no spaces configured");
    for (name, s) in cfg.spaces.iter_mut() {
        anyhow::ensure!(
            space::valid_name(name) && name != "healthz",
            "space name {name:?}: use lower-case letters, digits and hyphens"
        );
        s.dir = expand(home, &s.dir);
    }
    let login_file = expand(home, &cfg.login_file);
    let key = std::fs::read_to_string(&login_file)
        .map_err(|e| anyhow::anyhow!("reading the login file {}: {e}", login_file.display()))?;
    let app = web::App {
        spaces: cfg.spaces,
        auth: auth::Auth::LoginKey(auth::LoginKey::new(key)?),
        trusted_proxies: cfg.trusted_proxies,
        login_hint: cfg.login_hint,
        cache: Cache::default(),
    };
    let heartbeat = cfg.heartbeat.map(|h| Heartbeat { token_file: expand(home, &h.token_file), ..h });
    Ok((Arc::new(app), cfg.listen, heartbeat))
}

async fn beat(hb: &Heartbeat, problems: &[String]) {
    let token = match std::fs::read_to_string(&hb.token_file) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("reading the heartbeat token {}: {e}", hb.token_file.display());
            return;
        }
    };
    let mut url = format!("{}{}success={}", hb.url, if hb.url.contains('?') { '&' } else { '?' }, problems.is_empty());
    if !problems.is_empty() {
        url.push_str("&error=");
        url.push_str(&form_urlencoded::byte_serialize(problems.join("; ").as_bytes()).collect::<String>());
    }
    let res = reqwest::Client::new().post(&url).bearer_auth(token.trim()).timeout(Duration::from_secs(20)).send().await;
    if let Err(e) = res.and_then(|r| r.error_for_status()) {
        tracing::warn!("the heartbeat: {e}");
    }
}

/// The web service and its heartbeat, until the web service stops.
async fn run(app: Arc<web::App>, listen: SocketAddr, heartbeat: Option<Heartbeat>) -> anyhow::Result<()> {
    let a = app.clone();
    tokio::spawn(async move {
        loop {
            let problems = a.problems();
            for p in &problems {
                tracing::warn!("{p}");
            }
            if let Some(hb) = &heartbeat {
                beat(hb, &problems).await;
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
    web::serve(app, listen).await
}

#[derive(Default)]
struct Plugin {
    started: Mutex<bool>,
}

struct Shared(Arc<Plugin>);

#[async_trait]
impl Handler for Shared {
    async fn request(&self, _peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
        match method {
            acp::method::INITIALIZE => {
                let req: InitializeRequest = params(p)?;
                let node: NodeMeta = ext::get_meta(req.client_capabilities.meta.as_ref(), meta_key::NODE)
                    .map_err(RpcError::invalid_params)?
                    .ok_or_else(|| RpcError::refused("fleetlayer-pages runs under a Fleetlayer node"))?;
                let cfg: Config = serde_json::from_value(node.config.clone()).map_err(RpcError::invalid_params)?;
                let (app, listen, heartbeat) =
                    build(cfg, Path::new(&node.home)).map_err(|e| RpcError::refused(e.to_string()))?;
                let mut started = self.0.started.lock().await;
                if !*started {
                    *started = true;
                    tokio::spawn(async move {
                        if let Err(e) = run(app, listen, heartbeat).await {
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
                        name: "fleetlayer-pages".into(),
                        title: Some("Pages".into()),
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
    match args.first().map(String::as_str) {
        Some("serve") => {
            let Some(path) = args.get(1) else {
                eprintln!("usage: fleetlayer-pages serve <config.json>");
                std::process::exit(2);
            };
            let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
            let result = async {
                let cfg: Config = serde_json::from_slice(&std::fs::read(path)?)?;
                let (app, listen, heartbeat) = build(cfg, &home)?;
                run(app, listen, heartbeat).await
            }
            .await;
            if let Err(e) = result {
                eprintln!("fleetlayer-pages: {e}");
                std::process::exit(1);
            }
        }
        Some("--version") => println!("fleetlayer-pages {}", env!("CARGO_PKG_VERSION")),
        Some(other) => {
            eprintln!(
                "fleetlayer-pages: unknown command {other:?} (run by a Fleetlayer node, or `serve <config.json>`)"
            );
            std::process::exit(2);
        }
        None => fleetlayer_plugin::serve_stdio(Arc::new(Shared(Arc::new(Plugin::default())))).await,
    }
}
