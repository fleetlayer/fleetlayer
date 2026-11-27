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

mod apps;
mod auth;
mod oidc;
mod receive;
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
    base_url: Option<String>,
    /// The domain apps live under (`apps.home.example`): app `recipes` of space `personal` is
    /// `recipes--personal.apps.home.example`. No apps when absent.
    #[serde(default)]
    apps_domain: Option<String>,
    spaces: BTreeMap<String, SpaceConfig>,
    /// The login key's file (at least 32 random characters). Optional once `signin` is set: drop it to turn the key off.
    #[serde(default)]
    login_file: Option<PathBuf>,
    /// Sign people in through Fleetlayer's sign-in service (`docs/plugins/signin.md`).
    #[serde(default)]
    signin: Option<Signin>,
    #[serde(default)]
    trusted_proxies: Vec<IpAddr>,
    #[serde(default)]
    login_hint: Option<String>,
    #[serde(default)]
    heartbeat: Option<Heartbeat>,
}

/// The sign-in service, and this site's client there.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signin {
    /// Its address (`https://signin.example`).
    issuer: String,
    /// The client's name in the sign-in service's `clients` (and `service:<name>` in the policy).
    #[serde(default = "pages_client")]
    client: String,
    /// The client's secret, in a file only this user reads.
    secret_file: PathBuf,
}

fn pages_client() -> String {
    "pages".into()
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

fn build(
    mut cfg: Config,
    home: &Path,
    peer: Option<&Peer>,
) -> anyhow::Result<(Arc<web::App>, SocketAddr, Option<Heartbeat>)> {
    anyhow::ensure!(!cfg.spaces.is_empty(), "no spaces configured");
    for (name, s) in cfg.spaces.iter_mut() {
        anyhow::ensure!(
            space::valid_name(name) && name != "healthz",
            "space name {name:?}: use lower-case letters, digits and hyphens"
        );
        s.dir = expand(home, &s.dir);
        s.apps = s.apps.as_ref().map(|a| expand(home, a));
        if s.apps.is_some() {
            anyhow::ensure!(apps::valid_name(name), "space {name:?} has apps, so its name can't contain \"--\"");
            anyhow::ensure!(cfg.apps_domain.is_some(), "space {name:?} has apps: set apps_domain");
        }
    }
    if let Some(b) = cfg.base_url.as_deref().and_then(|b| b.strip_prefix("http://")) {
        let host = b.split(['/', ':']).next().unwrap_or_default();
        anyhow::ensure!(
            host == "localhost" || host.ends_with(".localhost") || host == "127.0.0.1",
            "base_url uses plain http, which is only allowed on this machine (localhost); use https behind a proxy"
        );
    }
    if cfg.apps_domain.is_some() {
        anyhow::ensure!(
            cfg.base_url.as_deref().is_some_and(|b| b.starts_with("https://") || b.starts_with("http://")),
            "apps need base_url (the pages site's own address, such as https://pages.example)"
        );
    }
    let key = match &cfg.login_file {
        Some(f) => {
            let f = expand(home, f);
            let key = std::fs::read_to_string(&f)
                .map_err(|e| anyhow::anyhow!("reading the login file {}: {e}", f.display()))?;
            Some(auth::LoginKey::new(key)?)
        }
        None => None,
    };
    let oidc = match &cfg.signin {
        Some(s) => {
            let base = cfg
                .base_url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("signin needs base_url (the pages site's own address)"))?;
            let f = expand(home, &s.secret_file);
            let secret = std::fs::read_to_string(&f)
                .map_err(|e| anyhow::anyhow!("reading the sign-in client secret {}: {e}", f.display()))?;
            Some(oidc::Oidc::new(&s.issuer, &s.client, &secret, base)?)
        }
        None => None,
    };
    let checker = peer.map(|p| auth::Checker::Node(p.clone()));
    let app = web::App {
        spaces: cfg.spaces,
        auth: auth::Auth::new(key, oidc, checker)?,
        trusted_proxies: cfg.trusted_proxies,
        login_hint: cfg.login_hint,
        cache: Cache::default(),
        base_url: cfg.base_url.map(|b| b.trim_end_matches('/').to_string()),
        apps_domain: cfg.apps_domain.map(|d| d.trim_matches('.').to_ascii_lowercase()),
        broker: apps::Broker::default(),
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

fn loopback_if_any(a: SocketAddr) -> SocketAddr {
    if a.ip().is_unspecified() { SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), a.port()) } else { a }
}

/// The web service and its heartbeat, until the web service stops. Under a node, it tells the node where it listens
/// once bound (`listen` may use port 0), and which hosts it serves, so a gateway can route to it.
async fn run(
    app: Arc<web::App>,
    listen: SocketAddr,
    heartbeat: Option<Heartbeat>,
    peer: Option<Peer>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let bound = loopback_if_any(listener.local_addr()?);
    tracing::info!(%bound, "pages listening");
    if let Some(peer) = peer {
        let hosts = app.apps_host_pattern().into_iter().collect();
        peer.notify(
            ext::method::SERVICE_LISTENING,
            &ext::ServiceListening {
                name: "pages".into(),
                url: format!("http://{bound}"),
                health: Some("/healthz".into()),
                hosts,
            },
        );
        // So `fleetlayer pages push` from other machines finds these spaces.
        let spaces: Vec<&String> = app.spaces.keys().collect();
        peer.notify(ext::method::PAGES_SPACES, &json!({ "spaces": spaces }));
    }
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
                    .ok_or_else(|| RpcError::refused("fleetlayer-pages runs under a Fleetlayer node"))?;
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

/// `fleetlayer-pages receive --space <space> --name <name> [--new] [--date YYYY-MM-DD] [--caller <tab@node>]`: one push on stdin and
/// stdout (`receive.rs`). The node that hosts the space starts it for `link/pages_serve`, with the plugin's config as
/// JSON in `FLEETLAYER_PAGES_CONFIG`.
fn receive_main(args: &[String]) -> i32 {
    let mut space = None;
    let mut name = None;
    let mut caller = String::from("-");
    let mut new = false;
    let mut date: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--space" => space = it.next().cloned(),
            "--name" => name = it.next().cloned(),
            "--caller" => caller = it.next().cloned().unwrap_or_default(),
            "--date" => date = it.next().cloned(),
            "--new" => new = true,
            other => {
                eprintln!("fleetlayer-pages receive: unknown argument {other:?}");
                return 2;
            }
        }
    }
    let (Some(space), Some(name)) = (space, name) else {
        eprintln!(
            "usage: fleetlayer-pages receive --space <space> --name <name> [--new] [--date YYYY-MM-DD] [--caller <tab@node>]"
        );
        return 2;
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let fail = |out: &mut dyn std::io::Write, msg: String| {
        let _ = out.write_all(format!("{}\n", json!({ "error": msg })).as_bytes());
        eprintln!("fleetlayer-pages receive: {msg}");
        1
    };
    let cfg: Config = match std::env::var("FLEETLAYER_PAGES_CONFIG")
        .map_err(|_| "FLEETLAYER_PAGES_CONFIG isn't set".to_string())
        .and_then(|c| serde_json::from_str(&c).map_err(|e| format!("the pages config: {e}")))
    {
        Ok(c) => c,
        Err(e) => return fail(&mut out, e),
    };
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let Some(mut sc) = cfg.spaces.get(&space).cloned() else {
        return fail(&mut out, format!("this pages service has no space {space:?}"));
    };
    sc.dir = expand(&home, &sc.dir);
    // The pushing machine's own date when it sent one, so a page dates from the pusher's day, else today in UTC.
    let today = match date {
        Some(d) if space::folder_date(&d) == Some(d.as_str()) => d,
        Some(d) => return fail(&mut out, format!("--date {d:?}: use YYYY-MM-DD")),
        None => space::day(
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        ),
    };
    let req = receive::Request {
        space_name: &space,
        space: &sc,
        base_url: cfg.base_url.as_deref(),
        name: &name,
        new,
        today: &today,
    };
    let stdin = std::io::stdin();
    match receive::run(&req, &mut stdin.lock(), &mut out) {
        Ok(done) => {
            eprintln!(
                "fleetlayer-pages receive: {caller} pushed {space}/{} ({} written, {} kept, {} removed)",
                done["folder"].as_str().unwrap_or_default(),
                done["written"],
                done["kept"],
                done["removed"]
            );
            0
        }
        Err(e) => {
            eprintln!("fleetlayer-pages receive: {caller} → {space}/{name}: {e}");
            1
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
                let (app, listen, heartbeat) = build(cfg, &home, None)?;
                run(app, listen, heartbeat, None).await
            }
            .await;
            if let Err(e) = result {
                eprintln!("fleetlayer-pages: {e}");
                std::process::exit(1);
            }
        }
        Some("receive") => std::process::exit(receive_main(&args[1..])),
        Some("--version") => println!("fleetlayer-pages {}", env!("CARGO_PKG_VERSION")),
        Some(other) => {
            eprintln!(
                "fleetlayer-pages: unknown command {other:?} (run by a Fleetlayer node, `serve <config.json>` or `receive`)"
            );
            std::process::exit(2);
        }
        None => fleetlayer_plugin::serve_stdio(Arc::new(Shared(Arc::new(Plugin::default())))).await,
    }
}
