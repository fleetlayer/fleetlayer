//! fleetlayer-claude-auth-manager: one person's Claude setup tokens (`claude setup-token`: a year long, inference
//! only) on every machine they run agents on, so every parallel agent uses the person's own accounts. The tokens
//! travel sealed to each node's key; the control plane relays them and can't read them (`docs/protocol.md` section 8).
//! Design notes: `docs/plugins/claude-auth-manager.md`.
//!
//! Two roles, set by `role:` in its config (`hub`, `node`, or `both` on the hub's own machine):
//! - **hub** (one per person, on a machine they control): publishes the tokens bundle from `tokens_file` (a 0600
//!   JSON list of `{slot, accountUuid, email, expiresAt, token}`) to `targets` whenever the file changes. Signing in
//!   and polling usage come later.
//! - **node**: applies the bundle: writes each account's slot (`slot_dir`, default `~/.claude/accounts/{slot}`, the
//!   rotator's), marks the slots it manages, and removes managed slots that left the bundle. Account 1 is Claude
//!   Code's default sign-in (`~/.claude`), written only with `default_slot: true`. A slot someone signed into by hand
//!   is never touched. On macOS the tokens go into the login keychain, where Claude Code keeps them there (`store.rs`).
//!   It also checks that each managed slot's token works (`health.rs`), as the `claude.token_health` reading.

mod claude;
mod health;
mod slots;
mod store;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fleetlayer_plugin::{Handler, Peer, answer, params};
use fleetlayer_protocol::acp::{self, Capabilities, Implementation, InitializeRequest, InitializeResponse};
use fleetlayer_protocol::ext::{
    self, NodeMeta, PluginMeta, Roles, SyncApply, SyncItem, SyncPublish, SyncRole, SyncTarget, meta_key,
};
use fleetlayer_protocol::rpc::RpcError;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

pub const BUNDLE: &str = "claude-auth-manager/tokens";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum Role {
    #[default]
    Node,
    Hub,
    Both,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    role: Role,
    /// Where account N's slot is (N of 2 and up), with `{slot}`: the directory the rotator puts in
    /// CLAUDE_SECURESTORAGE_CONFIG_DIR. Default `~/.claude/accounts/{slot}`, the rotator's.
    slot_dir: Option<String>,
    /// Also write account 1 into Claude Code's default sign-in slot (`~/.claude`), so sessions started without the
    /// rotator work on a machine nobody signed into. Never over a hand sign-in.
    #[serde(default)]
    default_slot: bool,
    /// Node: the Claude Code binary for the token checks. Default: `claude` on PATH, then the usual install places
    /// (a LaunchAgent's PATH has none of them).
    claude: Option<String>,
    /// Hub: the tokens file.
    tokens_file: Option<PathBuf>,
    /// Hub: the nodes to send them to.
    #[serde(default)]
    targets: Vec<String>,
}

#[derive(Default)]
struct Plugin {
    layout: Mutex<Option<slots::Layout>>,
    store: Mutex<Option<Arc<dyn store::Store>>>,
    /// Wakes the token checks after a bundle is applied.
    applied: Arc<tokio::sync::Notify>,
}

/// Claude Code: the configured path, else `claude` on PATH, else where its installers put it.
fn find_claude(home: &Path, configured: Option<&str>) -> String {
    if let Some(c) = configured {
        return expand(home, c);
    }
    let on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join("claude")).find(|f| f.is_file()))
        .unwrap_or_default();
    let known = [
        home.join(".local/bin/claude"),
        home.join(".claude/local/claude"),
        "/opt/homebrew/bin/claude".into(),
        "/usr/local/bin/claude".into(),
    ];
    on_path
        .or_else(|| known.into_iter().find(|f| f.is_file()))
        .map_or_else(|| "claude".into(), |f| f.display().to_string())
}

fn expand(home: &Path, p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => home.join(rest).display().to_string(),
        None => p.to_string(),
    }
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
                    .ok_or_else(|| RpcError::refused("fleetlayer-claude-auth-manager runs under a Fleetlayer node"))?;
                let home = PathBuf::from(&node.home);
                let config: Config = serde_json::from_value(node.config.clone()).map_err(RpcError::invalid_params)?;
                let applies = matches!(config.role, Role::Node | Role::Both);
                let publishes = matches!(config.role, Role::Hub | Role::Both);
                if applies {
                    let dir = config.slot_dir.as_deref().unwrap_or("~/.claude/accounts/{slot}");
                    if !dir.contains("{slot}") {
                        return Err(RpcError::refused("slot_dir must contain {slot}"));
                    }
                    let layout = slots::Layout {
                        pattern: expand(&home, dir),
                        default_dir: home.join(".claude"),
                        default_slot: config.default_slot,
                    };
                    let store: Arc<dyn store::Store> = store::native(&node.home, Some(&node.user)).into();
                    *self.0.layout.lock().await = Some(layout.clone());
                    *self.0.store.lock().await = Some(store.clone());
                    let claude = health::Claude { bin: find_claude(&home, config.claude.as_deref()) };
                    tokio::spawn(health_loop(peer.clone(), layout, store, claude, self.0.applied.clone()));
                }
                if publishes {
                    let file = config
                        .tokens_file
                        .clone()
                        .ok_or_else(|| RpcError::refused("tokens_file is needed on the hub"))?;
                    let file = PathBuf::from(expand(&home, &file.display().to_string()));
                    tokio::spawn(publish_loop(peer.clone(), file, config.targets.clone()));
                }
                let roles = Roles {
                    sync_target: applies.then(|| SyncRole { bundles: vec![BUNDLE.into()] }),
                    sync_source: publishes.then(|| SyncRole { bundles: vec![BUNDLE.into()] }),
                    ..Default::default()
                };
                answer(&InitializeResponse {
                    protocol_version: fleetlayer_protocol::ACP_PROTOCOL,
                    agent_capabilities: Capabilities {
                        meta: Some(ext::meta_with(
                            meta_key::PLUGIN,
                            &PluginMeta { protocol: fleetlayer_protocol::PROTOCOL, roles },
                        )),
                        ..Default::default()
                    },
                    agent_info: Some(Implementation {
                        name: "fleetlayer-claude-auth-manager".into(),
                        title: Some("Claude auth manager".into()),
                        version: env!("CARGO_PKG_VERSION").into(),
                    }),
                    auth_methods: vec![],
                })
            }
            ext::method::PING => Ok(json!({})),
            ext::method::SYNC_APPLY => {
                let a: SyncApply = params(p)?;
                if a.bundle != BUNDLE {
                    return Err(RpcError::not_here(format!("this plugin applies only {BUNDLE}")));
                }
                let layout = self.0.layout.lock().await.clone().ok_or_else(|| RpcError::refused("not a node"))?;
                let store = self.0.store.lock().await.clone().ok_or_else(|| RpcError::refused("not a node"))?;
                let item = a
                    .items
                    .iter()
                    .find(|i| i.name == "accounts.json")
                    .ok_or_else(|| RpcError::invalid_params("no accounts.json"))?;
                let accounts: Vec<slots::Account> =
                    serde_json::from_str(&item.content).map_err(RpcError::invalid_params)?;
                match slots::apply(&layout, store.as_ref(), &accounts) {
                    Ok(r) => {
                        self.0.applied.notify_one();
                        tracing::info!(
                            version = a.version,
                            written = r.written,
                            removed = r.removed,
                            skipped = r.skipped.len(),
                            "applied the tokens"
                        );
                        Ok(json!({"applied": true, "written": r.written, "removed": r.removed, "skipped": r.skipped}))
                    }
                    Err(e) => Ok(json!({"applied": false, "error": e.to_string()})),
                }
            }
            _ => Err(RpcError::method_not_found(method)),
        }
    }
}

/// Check each managed slot's token when it's due (`health::due`), and publish a `claude.token_health` reading for each
/// check. Looks every ten minutes, and straight after a bundle is applied.
async fn health_loop(
    peer: Peer,
    layout: slots::Layout,
    store: Arc<dyn store::Store>,
    claude: health::Claude,
    applied: Arc<tokio::sync::Notify>,
) {
    let mut history = health::History::new();
    tokio::time::sleep(Duration::from_secs(20)).await;
    while !peer.is_closed() {
        let managed = layout.managed();
        if !managed.is_empty() {
            let version = claude.version().await;
            let mut readings = vec![];
            for m in &managed {
                let fingerprint = match store.read(&m.slot) {
                    Ok(Some(stored)) => health::fingerprint(&stored),
                    Ok(None) => "none".into(),
                    Err(e) => format!("unreadable: {e}"),
                };
                let now = jiff::Timestamp::now();
                let v = version.clone().unwrap_or_else(|e| format!("unknown ({e})"));
                if !health::due(history.get(&m.slot), now.as_millisecond(), &v, &fingerprint) {
                    continue;
                }
                let outcome = match &version {
                    Err(e) => health::Outcome { ok: false, error: Some(e.clone()) },
                    Ok(_) => claude.check(&m.slot).await,
                };
                if outcome.ok {
                    tracing::info!(account = %m.email, "token check passed");
                } else {
                    tracing::warn!(account = %m.email, error = ?outcome.error, "token check failed");
                }
                readings.push(health::reading(m, &outcome, &v, now));
                history.insert(
                    m.slot.clone(),
                    health::Last { at_ms: now.as_millisecond(), version: v, fingerprint, ok: outcome.ok },
                );
            }
            history.retain(|slot, _| managed.iter().any(|m| &m.slot == slot));
            if !readings.is_empty() {
                peer.notify(ext::method::READING_PUBLISH, &ext::Readings { readings });
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(600)) => {}
            _ = applied.notified() => {}
        }
    }
}

/// Publish the tokens file whenever its contents change.
async fn publish_loop(peer: Peer, file: PathBuf, targets: Vec<String>) {
    let mut last = String::new();
    tokio::time::sleep(Duration::from_secs(3)).await;
    while !peer.is_closed() {
        match std::fs::read_to_string(&file) {
            Ok(text) => {
                let digest = hex::encode(Sha256::digest(text.as_bytes()));
                if digest != last {
                    match serde_json::from_str::<Vec<slots::Account>>(&text) {
                        Err(e) => tracing::error!("{}: {e}", file.display()),
                        Ok(accounts) => {
                            let version = jiff::Timestamp::now().as_millisecond() as u64;
                            let p = SyncPublish {
                                bundle: BUNDLE.into(),
                                version,
                                secret: true,
                                targets: targets.iter().map(|n| SyncTarget { node: n.clone() }).collect(),
                                items: vec![SyncItem {
                                    name: "accounts.json".into(),
                                    content: serde_json::to_string(&accounts).unwrap_or_default(),
                                }],
                            };
                            match peer
                                .request_within::<_, Value>(ext::method::SYNC_PUBLISH, &p, Duration::from_secs(90))
                                .await
                            {
                                Ok(r) => {
                                    tracing::info!(accounts = accounts.len(), version, results = %r["results"], "published the tokens");
                                    last = digest;
                                }
                                Err(e) => tracing::warn!("publishing the tokens: {e}"),
                            }
                        }
                    }
                }
            }
            Err(e) => tracing::warn!("reading {}: {e}", file.display()),
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
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
    fleetlayer_plugin::serve_stdio(Arc::new(Shared(Arc::new(Plugin::default())))).await;
}
