//! fleetlayer-herdr-fleet: every herdr in the fleet as one herdr (`docs/plugins/herdr-fleet.md`).
//!
//! A herdr client made for one machine (herdr's phone relay, a dashboard, a script) points at this plugin's socket
//! and HERDR_BIN shim instead of herdr's, unchanged, and sees this machine's herdr plus every other machine's sessions
//! that this node may see, each machine as a workspace of its own. This machine's panes keep everything herdr does;
//! the other machines' sessions can be listed, read, prompted, typed into, watched and closed, under the access
//! policy.
//!
//! Run with no arguments, it's a node plugin. Run through its `herdr` link (`<state_dir>/bin/herdr`), it's the HERDR_BIN
//! shim (`shim.rs`). Its config section in `node.yaml` (all optional):
//! - `state_dir`: its socket, shim link and IDs (default `~/.local/state/fleetlayer/herdr-fleet`).
//! - `herdr`: the real herdr (default: `herdr` on the node's PATH).
//! - `local_socket`: this machine's herdr socket (default: what `herdr status server` reports).
//! - `poll_seconds`: how often the fleet listing is read (default 3).

mod fleet;
mod serve;
mod shim;
mod term;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use fleetlayer_plugin::{Handler, Peer, answer, params};
use fleetlayer_protocol::acp::{self, Capabilities, Implementation, InitializeRequest, InitializeResponse};
use fleetlayer_protocol::ext::{self, NodeMeta, PluginMeta, PluginStatusReport, Roles, meta_key};
use fleetlayer_protocol::rpc::RpcError;
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock, broadcast};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    state_dir: Option<PathBuf>,
    herdr: Option<PathBuf>,
    local_socket: Option<PathBuf>,
    poll_seconds: Option<u64>,
}

fn expand(home: &Path, p: PathBuf) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => p,
    }
}

/// The real herdr: configured, or the first `herdr` on PATH.
fn find_herdr(config: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = config {
        return Some(p);
    }
    std::env::var("PATH").ok()?.split(':').map(|d| Path::new(d).join("herdr")).find(|p| p.is_file())
}

/// This machine's herdr socket, as `herdr status server` reports it, or herdr's default.
async fn local_socket(herdr: &Path, home: &Path) -> PathBuf {
    let out = tokio::process::Command::new(herdr).args(["status", "server"]).env_remove("HERDR_SOCKET_PATH").output();
    if let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(5), out).await {
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(p) = text.lines().find_map(|l| l.strip_prefix("socket:").map(|p| PathBuf::from(p.trim()))) {
            return p;
        }
    }
    home.join(".config/herdr/herdr.sock")
}

/// The pieces the status report reads.
struct Health {
    fleet_ok_at: Mutex<Option<Timestamp>>,
    fleet_error: Mutex<Option<(Timestamp, String)>>,
    clients: AtomicUsize,
    self_check: Mutex<Option<String>>,
}

struct NodeCloser(Peer);

#[async_trait]
impl serve::Closer for NodeCloser {
    async fn close(&self, address: &str) -> Result<(), term::Failure> {
        let p = json!({"to": address, "why": "closed from a herdr client through herdr-fleet"});
        self.0
            .request_within::<_, Value>(ext::method::SESSION_CLOSE, &p, Duration::from_secs(60))
            .await
            .map(|_| ())
            .map_err(|e| term::Failure::new("tab_close_failed", e.message))
    }
}

#[derive(Default)]
struct Plugin {
    started: Mutex<bool>,
}

struct Shared(Arc<Plugin>);

struct Paths {
    state: PathBuf,
    socket: PathBuf,
}

fn paths(home: &Path, config: &Config) -> Paths {
    let state =
        config.state_dir.clone().map_or_else(|| home.join(".local/state/fleetlayer/herdr-fleet"), |p| expand(home, p));
    Paths { socket: state.join("herdr.sock"), state }
}

/// Make the state directory (0700), the shim link and `shim.json`, and claim the socket.
fn prepare(p: &Paths, herdr: &Path, local_socket: &Path) -> anyhow::Result<std::os::unix::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(p.state.join("bin"))?;
    std::fs::set_permissions(&p.state, std::fs::Permissions::from_mode(0o700))?;
    let link = p.state.join("bin/herdr");
    let exe = std::env::current_exe()?;
    if std::fs::read_link(&link).ok().as_deref() != Some(exe.as_path()) {
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&exe, &link)?;
    }
    let cfg = shim::ShimConfig { herdr: herdr.into(), socket: p.socket.clone(), local_socket: local_socket.into() };
    std::fs::write(p.state.join("shim.json"), serde_json::to_vec_pretty(&cfg)?)?;
    // Another copy still answering means two plugins share the folder: refuse rather than steal its socket.
    if std::os::unix::net::UnixStream::connect(&p.socket).is_ok() {
        anyhow::bail!("{} is already served by another process", p.socket.display());
    }
    // A Unix socket's path has room for about 100 bytes.
    if p.socket.as_os_str().len() > 100 {
        anyhow::bail!(
            "the socket path {} is too long for a Unix socket: give the plugin a shorter `state_dir`",
            p.socket.display()
        );
    }
    let _ = std::fs::remove_file(&p.socket);
    let listener = std::os::unix::net::UnixListener::bind(&p.socket)?;
    std::fs::set_permissions(&p.socket, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

async fn run(peer: Peer, node: NodeMeta, config: Config) -> anyhow::Result<()> {
    let home = PathBuf::from(&node.home);
    let p = paths(&home, &config);
    let herdr = find_herdr(config.herdr.clone().map(|h| expand(&home, h)))
        .ok_or_else(|| anyhow::anyhow!("no herdr on the node's PATH: give the plugin `config: {{ herdr: <path> }}`"))?;
    let local = match config.local_socket.clone() {
        Some(s) => expand(&home, s),
        None => local_socket(&herdr, &home).await,
    };
    let listener = tokio::net::UnixListener::from_std(prepare(&p, &herdr, &local)?)?;
    let tools_socket = fleetlayer_protocol::local::default_node_socket().with_extension("tools.sock");
    let (events, _) = broadcast::channel(1024);
    let ctx = Arc::new(serve::Ctx {
        model: Arc::new(RwLock::new(fleet::Model::open(p.state.join("ids.json")))),
        events,
        local_socket: local.clone(),
        pool: term::Pool::new(tools_socket.clone()),
        closer: Arc::new(NodeCloser(peer.clone())),
        tools_socket,
    });
    let health = Arc::new(Health {
        fleet_ok_at: Mutex::new(None),
        fleet_error: Mutex::new(None),
        clients: AtomicUsize::new(0),
        self_check: Mutex::new(None),
    });
    tracing::info!(socket = %p.socket.display(), herdr = %herdr.display(), local = %local.display(), "serving");

    // Clients.
    let (c, h) = (ctx.clone(), health.clone());
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (c, h) = (c.clone(), h.clone());
            tokio::spawn(async move {
                h.clients.fetch_add(1, Ordering::Relaxed);
                serve::connection(c, stream).await;
                h.clients.fetch_sub(1, Ordering::Relaxed);
            });
        }
    });

    // The fleet listing.
    let (c, h, me, pr) = (ctx.clone(), health.clone(), node.node.clone(), peer.clone());
    let every = Duration::from_secs(config.poll_seconds.unwrap_or(3).clamp(1, 60));
    tokio::spawn(async move {
        loop {
            match pr.request_within::<_, Value>(ext::method::FLEET_QUERY, &json!({}), Duration::from_secs(20)).await {
                Ok(f) => {
                    let events = c.model.write().await.apply(fleet::parse_fleet(&f, &me));
                    for e in events {
                        let _ = c.events.send(e);
                    }
                    *h.fleet_ok_at.lock().await = Some(Timestamp::now());
                    *h.fleet_error.lock().await = None;
                }
                Err(e) => {
                    let mut err = h.fleet_error.lock().await;
                    if err.is_none() {
                        tracing::warn!("can't read the fleet: {}", e.message);
                        *err = Some((Timestamp::now(), e.message));
                    }
                }
            }
            if pr.is_closed() {
                break;
            }
            tokio::time::sleep(every).await;
        }
    });

    // Does the socket answer like herdr, end to end? Then report.
    loop {
        let check = self_check(&p.socket).await;
        *health.self_check.lock().await = check.err();
        let (problem, detail) = report(&ctx, &health).await;
        peer.notify(ext::method::PLUGIN_STATUS, &PluginStatusReport { problem, detail });
        if peer.is_closed() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

/// Ask the merged socket for `agent.list`, as a client would.
async fn self_check(socket: &Path) -> Result<(), String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let run = async {
        let s = tokio::net::UnixStream::connect(socket).await.map_err(|e| format!("connect: {e}"))?;
        let (r, mut w) = s.into_split();
        w.write_all(b"{\"id\":\"self-check\",\"method\":\"agent.list\",\"params\":{}}\n")
            .await
            .map_err(|e| e.to_string())?;
        let line = BufReader::new(r).lines().next_line().await.map_err(|e| e.to_string())?.unwrap_or_default();
        let v: Value = serde_json::from_str(&line).map_err(|e| format!("answer: {e}"))?;
        if v["result"]["agents"].is_array() { Ok(()) } else { Err(format!("answered {line}")) }
    };
    tokio::time::timeout(Duration::from_secs(10), run).await.unwrap_or_else(|_| Err("no answer in 10 seconds".into()))
}

/// The plugin's problem (None when it does its job) and its numbers.
async fn report(ctx: &serve::Ctx, h: &Health) -> (Option<String>, Value) {
    let ok_at = *h.fleet_ok_at.lock().await;
    let err = h.fleet_error.lock().await.clone();
    let check = h.self_check.lock().await.clone();
    let problem = if let Some(c) = &check {
        Some(format!("the merged herdr socket doesn't answer: {c}"))
    } else if let Some((since, e)) = &err
        && Timestamp::now().duration_since(*since).as_secs() >= 120
    {
        Some(format!("can't read the fleet's sessions since {}: {e}", since.strftime("%Y-%m-%d %H:%M UTC")))
    } else {
        None
    };
    let detail = json!({
        "remoteSessions": ctx.model.read().await.len(),
        "lastFleetAt": ok_at.map(|t| t.to_string()),
        "clients": h.clients.load(Ordering::Relaxed),
        "openStreams": ctx.pool.open_count().await,
    });
    (problem, detail)
}

#[async_trait]
impl Handler for Shared {
    async fn request(&self, peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
        match method {
            acp::method::INITIALIZE => {
                let req: InitializeRequest = params(p)?;
                let node: NodeMeta = ext::get_meta(req.client_capabilities.meta.as_ref(), meta_key::NODE)
                    .map_err(RpcError::invalid_params)?
                    .ok_or_else(|| RpcError::refused("fleetlayer-herdr-fleet runs under a Fleetlayer node"))?;
                let config: Config =
                    serde_json::from_value(if node.config.is_null() { json!({}) } else { node.config.clone() })
                        .map_err(RpcError::invalid_params)?;
                let mut started = self.0.started.lock().await;
                if !*started {
                    *started = true;
                    let peer2 = peer.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        if let Err(e) = run(peer2.clone(), node, config).await {
                            tracing::error!("herdr-fleet stopped: {e}");
                            peer2.notify(
                                ext::method::PLUGIN_STATUS,
                                &PluginStatusReport { problem: Some(e.to_string()), detail: json!({}) },
                            );
                            // Exit so the node restarts it, with backoff.
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            std::process::exit(1);
                        }
                    });
                }
                let meta = PluginMeta { protocol: fleetlayer_protocol::PROTOCOL, roles: Roles::default() };
                answer(&InitializeResponse {
                    protocol_version: fleetlayer_protocol::ACP_PROTOCOL,
                    agent_capabilities: Capabilities {
                        meta: Some(ext::meta_with(meta_key::PLUGIN, &meta)),
                        ..Default::default()
                    },
                    agent_info: Some(Implementation {
                        name: "fleetlayer-herdr-fleet".into(),
                        title: Some("Every herdr as one".into()),
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

fn main() {
    let mut argv = std::env::args();
    let argv0 = PathBuf::from(argv.next().unwrap_or_default());
    let args: Vec<String> = argv.collect();
    if argv0.file_name().and_then(|n| n.to_str()) == Some("herdr") {
        shim::run(&argv0, args);
    }
    if args.first().is_some_and(|a| a == "--version" || a == "-V") {
        println!("fleetlayer-herdr-fleet {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let rt = tokio::runtime::Runtime::new().expect("a runtime");
    rt.block_on(fleetlayer_plugin::serve_stdio(Arc::new(Shared(Arc::new(Plugin::default())))));
}
