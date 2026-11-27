//! fleetlayer-gateway: one address per service on a laptop (`http://<name>.localhost:7680`, no TLS), a status page
//! that alerts, and the same routes as Traefik and Gatus config for a server.
//!
//! - `serve` (the default): the proxy, the status page at `fleetlayer.<domain>`, and the monitor.
//! - `emit traefik|gatus`: config for a server's proxy and monitor, from the same registry.
//! - `services`: the registry, one line each.
//!
//! It runs as its own user service beside the node and the control plane (`fleetlayer up`), not as a node plugin,
//! because it watches the node. Who watches it: launchd/systemd restart it, and the node's heartbeat checks its
//! `/healthz` when node.yaml names it.

mod config;
mod emit;
mod monitor;
mod node;
mod notify;
mod proxy;
mod registry;
mod status;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{Method, Request, header};
use axum::response::{IntoResponse, Response};
use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::registry::{Registry, Target};

#[derive(Parser)]
#[command(
    name = "fleetlayer-gateway",
    version,
    about = "One address per service, a status page that alerts, and config for a server's proxy"
)]
struct Cli {
    /// The settings file. Default `~/.config/fleetlayer/gateway.yaml`.
    #[arg(long, global = true, env = "FLEETLAYER_GATEWAY_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the proxy, the status page and the monitor.
    Serve,
    /// Every service: its address, backend and where it came from.
    Services,
    /// Config for a server's proxy or monitor, from the same registry.
    Emit {
        #[command(subcommand)]
        what: Emit,
    },
}

#[derive(clap::Args)]
struct EmitCommon {
    /// Write here instead of standard output (atomically).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Keep running and rewrite --out whenever the routes change.
    #[arg(long, requires = "out")]
    watch: bool,
}

#[derive(Subcommand)]
enum Emit {
    /// A Traefik file-provider file (routers and services).
    Traefik {
        /// The server's domain: services become `<name>.<domain>`.
        #[arg(long)]
        domain: String,
        /// Traefik entry points (`websecure`).
        #[arg(long = "entrypoint")]
        entrypoints: Vec<String>,
        /// A certificate resolver for TLS (`le`).
        #[arg(long)]
        cert_resolver: Option<String>,
        /// Middlewares for every router (`auth@file`).
        #[arg(long = "middleware")]
        middlewares: Vec<String>,
        #[command(flatten)]
        common: EmitCommon,
    },
    /// Gatus endpoints (health checks) and external endpoints (heartbeats).
    Gatus {
        #[arg(long, default_value = "fleetlayer")]
        group: String,
        /// Gatus alert types for every endpoint (`matrix`).
        #[arg(long = "alert")]
        alerts: Vec<String>,
        #[command(flatten)]
        common: EmitCommon,
    },
}

pub struct App {
    pub reg: Arc<Registry>,
    pub monitor: Arc<monitor::Monitor>,
    pub client: proxy::HttpClient,
    pub config_path: PathBuf,
}

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

pub fn error_chain(e: &dyn std::error::Error) -> String {
    let mut msg = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        msg = format!("{msg}: {s}");
        src = s.source();
    }
    msg
}

async fn handle(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request<Body>,
) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .or_else(|| req.uri().host())
        .unwrap_or("")
        .to_string();
    let routes = app.reg.routes().await;
    match registry::resolve(&routes, &app.reg.cfg.domain, &host) {
        Target::Status | Target::Foreign
            if host.is_empty() || host.starts_with("127.0.0.1") || host.starts_with("[::1]") =>
        {
            status_routes(&app, req).await
        }
        Target::Status => status_routes(&app, req).await,
        Target::Unknown(name) => status::page(&app, Some(&name)).await,
        Target::Foreign => {
            (axum::http::StatusCode::MISDIRECTED_REQUEST, "not a host this gateway serves\n").into_response()
        }
        Target::Route(r) => match proxy::forward(&app.client, &r.url, peer, req).await {
            Ok(resp) => resp,
            Err(e) => status::backend_down(&app, &r.name, &e),
        },
    }
}

async fn status_routes(app: &App, req: Request<Body>) -> Response {
    let path = req.uri().path().to_string();
    let query: HashMap<String, String> = req.uri().query().map(form_pairs).unwrap_or_default();
    match (req.method(), path.as_str()) {
        (&Method::GET, "/healthz") => status::healthz(app).await,
        (&Method::GET, "/api/status") => status::api_status(app).await,
        (&Method::POST, p) if p.starts_with("/api/v1/endpoints/") && p.ends_with("/external") => {
            let key = &p["/api/v1/endpoints/".len()..p.len() - "/external".len()];
            status::heartbeat(app, key, &query, req.headers()).await
        }
        (&Method::GET | &Method::HEAD, "/") => status::page(app, None).await,
        _ => (axum::http::StatusCode::NOT_FOUND, "not found\n").into_response(),
    }
}

/// `a=1&b=x%20y` → pairs, percent- and plus-decoded.
fn form_pairs(q: &str) -> HashMap<String, String> {
    fn decode(s: &str) -> String {
        let s = s.replace('+', " ");
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%'
                && i + 2 < b.len()
                && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
            {
                out.push(v);
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect()
}

pub fn router(app: Arc<App>) -> Router {
    Router::new().fallback(handle).with_state(app)
}

async fn serve(cfg: Config, config_path: PathBuf) -> anyhow::Result<()> {
    let listen = cfg.listen;
    if !listen.ip().is_loopback() {
        tracing::warn!(%listen, "listening beyond this machine: every service is reachable from the network, without TLS");
    }
    let reg = Registry::new(cfg);
    let monitor = monitor::Monitor::new(reg.clone());
    let app = Arc::new(App { reg: reg.clone(), monitor: monitor.clone(), client: proxy::client(), config_path });
    tokio::spawn(reg.clone().follow_node());
    tokio::spawn(monitor.run());
    let listener = tokio::net::TcpListener::bind(listen).await.map_err(|e| {
        anyhow::anyhow!(
            "can't listen on {listen} ({e}); another program may have the port: set listen: in gateway.yaml"
        )
    })?;
    tracing::info!(%listen, status = reg.cfg.address(registry::STATUS), "gateway listening");
    axum::serve(listener, router(app).into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

fn write_atomic(path: &std::path::Path, text: &str) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

async fn emit(cfg: Config, what: Emit) -> anyhow::Result<()> {
    let reg = Registry::new(cfg);
    let render = |routes: &[registry::Route]| -> String {
        match &what {
            Emit::Traefik { domain, entrypoints, cert_resolver, middlewares, .. } => emit::traefik(
                routes,
                &emit::TraefikOptions {
                    domain: domain.clone(),
                    entrypoints: entrypoints.clone(),
                    cert_resolver: cert_resolver.clone(),
                    middlewares: middlewares.clone(),
                },
            ),
            Emit::Gatus { group, alerts, .. } => {
                let hbs: Vec<(String, u64)> = reg.cfg.heartbeats.iter().map(|(k, h)| (k.clone(), h.every)).collect();
                emit::gatus(routes, &hbs, &emit::GatusOptions { group: group.clone(), alerts: alerts.clone() })
            }
        }
    };
    let common = match &what {
        Emit::Traefik { common, .. } | Emit::Gatus { common, .. } => common,
    };
    let refresh = async || {
        if reg.cfg.node {
            match node::services(&reg.cfg.node_socket()).await {
                Ok(list) => reg.set_announced(list).await,
                Err(e) => eprintln!("(the node didn't answer, so its plugins' services are as last seen: {e})"),
            }
        }
    };
    refresh().await;
    let Some(out) = &common.out else {
        print!("{}", render(&reg.routes().await));
        return Ok(());
    };
    let mut last = String::new();
    loop {
        let text = render(&reg.routes().await);
        if text != last {
            write_atomic(out, &text)?;
            eprintln!("Wrote {}.", out.display());
            last = text;
        }
        if !common.watch {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        refresh().await;
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "fleetlayer_gateway=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let path = cli.config.unwrap_or_else(config::default_path);
    let res = async {
        let cfg = Config::load(&path)?;
        match cli.command.unwrap_or(Command::Serve) {
            Command::Serve => serve(cfg, path).await,
            Command::Emit { what } => emit(cfg, what).await,
            Command::Services => {
                let reg = Registry::new(cfg);
                if reg.cfg.node {
                    match node::services(&reg.cfg.node_socket()).await {
                        Ok(list) => reg.set_announced(list).await,
                        Err(e) => eprintln!("(the node didn't answer: {e})"),
                    }
                }
                println!("Status page: {}", reg.cfg.address(registry::STATUS));
                for r in reg.routes().await {
                    let more = r.hosts.iter().map(|h| format!(" + {h}.{}", reg.cfg.domain)).collect::<String>();
                    println!("{:<12} {}{more}  →  {}  ({})", r.name, reg.cfg.address(&r.name), r.url, r.source);
                }
                Ok(())
            }
        }
    }
    .await;
    if let Err(e) = res {
        eprintln!("fleetlayer-gateway: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests;
