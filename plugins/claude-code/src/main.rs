//! fleetlayer-claude-code: the harness plugin for Claude Code (`docs/protocol.md` section 5). It lists the Claude
//! Code sessions on this machine that can take a message through Claude Code's own messaging socket, delivers through
//! it (so the agent sees who a message is from, and nothing is typed into the person's terminal), and points the node
//! at each session's transcript.
//!
//! Config (all optional): `claude_dir`, Claude Code's directory (default `~/.claude`).
//!
//! A machine whose sessions run in bypass-permissions mode needs `"crossSessionInbound": "accept"` in
//! `~/.claude/settings.json`, or Claude Code holds every message for the person's approval in the tab.

mod socket;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fleetlayer_plugin::{Handler, Peer, answer, params};
use fleetlayer_protocol::acp::{
    self, Capabilities, Implementation, InitializeRequest, InitializeResponse, ListSessionsResponse, SessionInfo,
};
use fleetlayer_protocol::ext::{
    self, Deliver, Delivered, HarnessRole, NodeMeta, PluginMeta, Roles, SessionMeta, Via, meta_key,
};
use fleetlayer_protocol::rpc::{RpcError, code};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

#[derive(Debug, Default, Deserialize)]
struct Config {
    claude_dir: Option<PathBuf>,
}

#[derive(Default)]
struct Plugin {
    claude_dir: Mutex<Option<PathBuf>>,
    watching: Mutex<bool>,
}

impl Plugin {
    async fn dir(&self) -> Result<PathBuf, RpcError> {
        self.claude_dir.lock().await.clone().ok_or_else(|| RpcError::refused("initialize first"))
    }

    /// The sessions that can take a message now.
    async fn deliverable(&self) -> Result<Vec<socket::Registration>, RpcError> {
        let dir = self.dir().await?.join("sessions");
        let mut regs: Vec<_> = socket::registrations(&dir).await.into_iter().filter(socket::usable).collect();
        regs.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        regs.dedup_by(|a, b| a.session_id == b.session_id);
        Ok(regs)
    }
}

struct Shared(Arc<Plugin>);

#[async_trait]
impl Handler for Shared {
    async fn request(&self, peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
        let plugin = &self.0;
        match method {
            acp::method::INITIALIZE => {
                let req: InitializeRequest = params(p)?;
                let node: NodeMeta = ext::get_meta(req.client_capabilities.meta.as_ref(), meta_key::NODE)
                    .map_err(RpcError::invalid_params)?
                    .ok_or_else(|| RpcError::refused("fleetlayer-claude-code runs under a Fleetlayer node"))?;
                let config: Config = serde_json::from_value(node.config).unwrap_or_default();
                *plugin.claude_dir.lock().await =
                    Some(config.claude_dir.unwrap_or_else(|| PathBuf::from(&node.home).join(".claude")));
                let meta = PluginMeta {
                    protocol: fleetlayer_protocol::PROTOCOL,
                    roles: Roles {
                        harness: Some(HarnessRole {
                            harnesses: vec!["claude".into()],
                            deliver: true,
                            transcripts: true,
                        }),
                        ..Default::default()
                    },
                };
                answer(&InitializeResponse {
                    protocol_version: fleetlayer_protocol::ACP_PROTOCOL,
                    agent_capabilities: Capabilities {
                        meta: Some(ext::meta_with(meta_key::PLUGIN, &meta)),
                        ..Default::default()
                    },
                    agent_info: Some(Implementation {
                        name: "fleetlayer-claude-code".into(),
                        title: Some("Claude Code".into()),
                        version: env!("CARGO_PKG_VERSION").into(),
                    }),
                    auth_methods: vec![],
                })
            }
            ext::method::PING => Ok(json!({})),
            acp::method::SESSION_LIST => {
                let regs = plugin.deliverable().await?;
                let mut watching = plugin.watching.lock().await;
                if !*watching {
                    *watching = true;
                    tokio::spawn(watch(plugin.clone(), peer.clone()));
                }
                let sessions = regs
                    .into_iter()
                    .map(|r| SessionInfo {
                        session_id: r.session_id,
                        cwd: r.cwd,
                        title: r.name,
                        updated_at: None,
                        meta: Some(ext::meta_with(
                            meta_key::SESSION,
                            &SessionMeta {
                                harness: Some("claude".into()),
                                deliverable: Some(true),
                                ..Default::default()
                            },
                        )),
                    })
                    .collect();
                answer(&ListSessionsResponse { sessions, next_cursor: None })
            }
            ext::method::SESSION_DELIVER => {
                let d: Deliver = params(p)?;
                let dir = plugin.dir().await?.join("sessions");
                let reg = socket::find(&dir, &d.session_id).await.ok_or_else(|| {
                    RpcError::new(code::NO_CHANNEL, format!("no Claude Code session {} is registered", d.session_id))
                })?;
                match socket::send(&dir, &reg, &d.from.label, &d.text).await {
                    Ok(()) => answer(&Delivered { via: Via::HarnessChannel }),
                    Err(socket::SendError::NoChannel(m)) => Err(RpcError::new(code::NO_CHANNEL, m)),
                    Err(socket::SendError::Gone(m)) => Err(RpcError::not_here(m)),
                    Err(socket::SendError::Failed(m)) => Err(RpcError::refused(m)),
                }
            }
            ext::method::HARNESS_TRANSCRIPT => {
                let id =
                    p.get("sessionId").and_then(Value::as_str).ok_or_else(|| RpcError::invalid_params("sessionId"))?;
                let path = socket::transcript(&plugin.dir().await?, id)
                    .ok_or_else(|| RpcError::new(code::UNKNOWN_SESSION, format!("no transcript for session {id}")))?;
                answer(&json!({"path": path, "format": "claude-jsonl"}))
            }
            _ => Err(RpcError::method_not_found(method)),
        }
    }
}

/// Say when the set of deliverable sessions changes (a session started, ended or was resumed).
async fn watch(plugin: Arc<Plugin>, peer: Peer) {
    let mut last: Option<BTreeSet<String>> = None;
    while !peer.is_closed() {
        if let Ok(regs) = plugin.deliverable().await {
            let now: BTreeSet<String> = regs.into_iter().map(|r| r.session_id).collect();
            if last.as_ref().is_some_and(|l| *l != now) {
                peer.notify(ext::method::SESSIONS_CHANGED, &json!({}));
            }
            last = Some(now);
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("FLEETLAYER_LOG"))
        .init();
    fleetlayer_plugin::serve_stdio(Arc::new(Shared(Arc::new(Plugin::default())))).await;
}
