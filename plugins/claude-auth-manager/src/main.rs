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
//!   is never touched.

mod slots;

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
    /// Hub: the tokens file.
    tokens_file: Option<PathBuf>,
    /// Hub: the nodes to send them to.
    #[serde(default)]
    targets: Vec<String>,
}

#[derive(Default)]
struct Plugin {
    layout: Mutex<Option<slots::Layout>>,
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
                    *self.0.layout.lock().await = Some(slots::Layout {
                        pattern: expand(&home, dir),
                        default_dir: home.join(".claude"),
                        default_slot: config.default_slot,
                    });
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
                let item = a
                    .items
                    .iter()
                    .find(|i| i.name == "accounts.json")
                    .ok_or_else(|| RpcError::invalid_params("no accounts.json"))?;
                let accounts: Vec<slots::Account> =
                    serde_json::from_str(&item.content).map_err(RpcError::invalid_params)?;
                match slots::apply(&layout, &accounts) {
                    Ok(r) => {
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
