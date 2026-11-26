//! The node's local socket (mode 0600, in a 0700 directory): the `fleetlayer` CLI talks to the node here, with the
//! same JSON-RPC as plugins use.
//!
//! Methods:
//! - `node/status` → the node's name, machine, version and plugins.
//! - `node/sessions` → every session the node knows.
//! - `_fleetlayer.dev/message/send` `{to, text, caller?: {sessionId?, address?}}` → `{via}`.
//!
//! Only the node's own Unix user can open the socket, so the caller's claim about which session it runs in
//! (`caller`, from the session's environment) is trusted the way the user's own shell is: any process of that user
//! could claim it.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use fleetlayer_plugin::{Handler, Peer, answer, params};
use fleetlayer_protocol::ext::{self, From};
use fleetlayer_protocol::link as l;
use fleetlayer_protocol::rpc::RpcError;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::UnixListener;

use crate::Node;

pub async fn serve(node: Arc<Node>, path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!(socket = %path.display(), "local socket listening");
    loop {
        let (stream, _) = listener.accept().await?;
        let (r, w) = stream.into_split();
        let (_peer, _task) = fleetlayer_plugin::connect(r, w, Arc::new(Local { node: node.clone() }));
    }
}

struct Local {
    node: Arc<Node>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Caller {
    session_id: Option<String>,
    address: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Send {
    to: String,
    text: String,
    caller: Option<Caller>,
}

#[derive(Debug, Deserialize)]
struct SubscribeArgs {
    channel: String,
    subject: Option<String>,
    /// `<tab>@<node>`; the calling session when absent.
    target: Option<String>,
    caller: Option<Caller>,
}

impl Local {
    /// The session the caller says it runs in, if this node knows it.
    async fn caller(&self, c: Option<&Caller>) -> Option<crate::sessions::Session> {
        let c = c?;
        if let Some(id) = &c.session_id
            && let Some(s) = self.node.registry.get(id).await
        {
            return Some(s);
        }
        self.node.registry.by_address(c.address.as_deref()?).await
    }
}

#[async_trait]
impl Handler for Local {
    async fn request(&self, _peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
        let node = &self.node;
        match method {
            "node/status" => {
                let mut plugins = vec![];
                for p in &node.plugins {
                    plugins.push(p.status().await);
                }
                answer(&json!({
                    "control": node.link.state().await,
                    "health": crate::heartbeat::health(node, node.has_control).await,
                    "node": node.name,
                    "publicKey": node.public_key,
                    "keyFingerprint": crate::keys::fingerprint(&node.public_key),
                    "machine": node.machine,
                    "user": node.user,
                    "version": env!("CARGO_PKG_VERSION"),
                    "plugins": plugins,
                }))
            }
            "node/tools_enable" | "node/tools_disable" => {
                let name = p["tool"].as_str().ok_or_else(|| RpcError::invalid_params("tool"))?;
                let tool = node
                    .tools
                    .iter()
                    .find(|t| t.name == name)
                    .ok_or_else(|| RpcError::not_here(format!("{} shares no tool called {name}", node.name)))?;
                if method == "node/tools_disable" {
                    node.opt_ins.disable(name).await;
                    tracing::info!(tool = name, "opt-in ended by hand");
                    return answer(&json!({"tool": name, "enabled": false}));
                }
                let minutes = p["minutes"].as_u64().unwrap_or(120).clamp(1, 24 * 60);
                node.opt_ins.enable(name, std::time::Duration::from_secs(minutes * 60)).await;
                tracing::warn!(tool = name, minutes, networks = ?tool.networks, "opted in: offered on any network");
                answer(&json!({"tool": name, "enabled": true, "minutes": minutes}))
            }
            "node/summaries_import" => {
                node.ask_observer(ext::method::SUMMARY_IMPORT, &p, std::time::Duration::from_secs(600)).await
            }
            "node/summaries" => {
                let peer =
                    node.link.peer().await.ok_or_else(|| {
                        RpcError::not_here(format!("{} isn't linked to a control plane now", node.name))
                    })?;
                peer.request::<_, Value>(l::method::SUMMARIES_SEARCH, &p).await
            }
            "node/fleet" | "node/subscriptions" | "node/subscribe" | "node/unsubscribe" => {
                let peer =
                    node.link.peer().await.ok_or_else(|| {
                        RpcError::not_here(format!("{} isn't linked to a control plane now", node.name))
                    })?;
                let (m, p) = match method {
                    "node/fleet" => (l::method::FLEET, json!({})),
                    "node/subscriptions" => (l::method::SUBSCRIPTIONS, json!({})),
                    _ => {
                        let s: SubscribeArgs = params(p)?;
                        let target = match s.target {
                            Some(t) => t,
                            None => {
                                let c = self.caller(s.caller.as_ref()).await.ok_or_else(|| {
                                    RpcError::refused("run this from a session, or say which one with --target")
                                })?;
                                format!("{}@{}", c.tab().unwrap_or(&c.session_id), node.name)
                            }
                        };
                        let m = if method == "node/subscribe" { l::method::SUBSCRIBE } else { l::method::UNSUBSCRIBE };
                        (m, json!({"channel": s.channel, "subject": s.subject, "target": target}))
                    }
                };
                peer.request::<_, Value>(m, &p).await
            }
            "node/sessions" => answer(&json!({"node": node.name, "sessions": node.registry.all().await})),
            ext::method::MESSAGE_SEND => {
                let s: Send = params(p)?;
                let caller = self.caller(s.caller.as_ref()).await;
                let from = match &caller {
                    Some(c) => From {
                        label: format!("{} ({})", c.tab().unwrap_or(&c.session_id), node.name),
                        name: c.tab().map(str::to_string),
                        node: Some(node.name.clone()),
                        session_id: Some(c.session_id.clone()),
                    },
                    None => From { label: format!("a shell on {}", node.name), ..Default::default() },
                };
                answer(&crate::deliver::send(node, &s.to, &s.text, from, caller.as_ref()).await?)
            }
            _ => Err(RpcError::method_not_found(method)),
        }
    }
}
