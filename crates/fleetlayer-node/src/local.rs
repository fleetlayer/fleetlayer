//! The node's local socket (mode 0600, in a 0700 directory): the `fleetlayer` CLI talks to the node here, with the
//! same JSON-RPC as plugins use.
//!
//! Methods:
//! - `node/status` → the node's name, machine, version and plugins.
//! - `node/sessions` → every session the node knows.
//! - `node/services` → `{services: [{plugin, name, url, health?, hosts}]}`: the HTTP services its plugins announced.
//! - `node/readings` `{kinds}` → `{readings}`: the newest of each kind from every node this one may read.
//! - `node/transcript_search` `{text, limit?, perSession?, on?, caller?}` → `TranscriptHits`: every transcript here,
//!   or on `on` through the control plane, searched for `text`.
//! - `node/session_handoff` `{from?, to, note?, tab?, cwd?, harness?, prompt?, model?, effort?, why?, caller?}` →
//!   `HandoffStarted`: a new session on `to` takes over `from`'s work (the caller when absent), through the control
//!   plane, which keeps the note; `node/handoff_get` `{id, why?, caller?}` → `Handoff`: one, with its note.
//! - `node/session_open` `{tab?, cwd?, harness?, prompt?, resume?, model?, effort?, on?, why?, caller?}` → `SessionStarted`: start or
//!   resume a session here or on node `on` (`crate::start`).
//! - `node/session_close` `{to, why?, caller?}`, `node/session_answer` `{to, prompt, answers, why?, caller?}` and
//!   `node/session_command` `{to, command, why?, caller?}` → `SessionActed`: close a session, answer the prompt it's
//!   blocked on, or type an allowlisted slash command into it, here or on another node (`crate::act`).
//! - `node/role_activate` `{role, for, why?, caller?}`, `node/role_deactivate` `{role, caller?}` → `RoleActivated`;
//!   `node/role_list` `{caller?}` → `RoleWindows`: role windows, through the control plane (`crate::window`).
//! - `node/host_ask` `{to, why}` → `AccessDecision`: ask the control plane whether this node (`-@<node>`) may connect
//!   to `host:<name>[:<port>]`, filing an approval request in its own name (`fleetlayer-egress ask`, `docs/egress.md`).
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

/// Serve the node's socket, already claimed (`socket::claim`).
pub async fn serve(node: Arc<Node>, listener: std::os::unix::net::UnixListener, path: &Path) -> anyhow::Result<()> {
    let listener = UnixListener::from_std(listener)?;
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
    #[serde(default)]
    why: Option<String>,
    caller: Option<Caller>,
    /// Who's sending, when the caller isn't one of this node's sessions (a service on the machine): shown as
    /// "<from> (<node>)". Access control still sees `-@<node>`.
    #[serde(default)]
    from: Option<String>,
    /// The text is outside data (an email, a web page): the recipient gets it marked untrusted.
    #[serde(default)]
    untrusted: bool,
    #[serde(default)]
    subkind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SessionAct {
    to: String,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    answers: Vec<ext::PromptAnswer>,
    #[serde(default)]
    command: Option<String>,
    why: Option<String>,
    caller: Option<Caller>,
}

#[derive(Debug, Deserialize)]
struct SessionOpen {
    #[serde(flatten)]
    spec: l::StartSpec,
    on: Option<String>,
    why: Option<String>,
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
                    "skills": node.skills,
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
                    crate::me::announce(node).await;
                    return answer(&json!({"tool": name, "enabled": false}));
                }
                let minutes = p["minutes"].as_u64().unwrap_or(120).clamp(1, 24 * 60);
                node.opt_ins.enable(name, std::time::Duration::from_secs(minutes * 60)).await;
                tracing::warn!(tool = name, minutes, networks = ?tool.networks, "opted in: offered on any network");
                crate::me::announce(node).await;
                // Say so again when it lapses.
                let n = node.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(minutes * 60 + 1)).await;
                    crate::me::announce(&n).await;
                });
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
                            Some(t) => not_a_plugin(t)?,
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
            "node/watch" | "node/unwatch" | "node/watches" => {
                let peer =
                    node.link.peer().await.ok_or_else(|| {
                        RpcError::not_here(format!("{} isn't linked to a control plane now", node.name))
                    })?;
                match method {
                    "node/watch" => {
                        // Always for the calling session: a watch's note is its own words, never another's.
                        let c = self
                            .caller(p.get("caller").and_then(|c| serde_json::from_value(c.clone()).ok()).as_ref())
                            .await
                            .ok_or_else(|| {
                                RpcError::refused("run this from a session: a watch tells the session that set it")
                            })?;
                        let mut w: l::WatchRequest = serde_json::from_value(p).map_err(RpcError::invalid_params)?;
                        w.target = format!("{}@{}", c.tab().unwrap_or(&c.session_id), node.name);
                        peer.request::<_, Value>(l::method::WATCH, &w).await
                    }
                    "node/unwatch" => peer.request::<_, Value>(l::method::UNWATCH, &json!({"id": p["id"]})).await,
                    _ => peer.request::<_, Value>(l::method::WATCHES, &json!({})).await,
                }
            }
            "node/events" => {
                let peer =
                    node.link.peer().await.ok_or_else(|| {
                        RpcError::not_here(format!("{} isn't linked to a control plane now", node.name))
                    })?;
                let target = match p.get("target").and_then(Value::as_str) {
                    Some(t) => not_a_plugin(t.to_string())?,
                    None => {
                        let c = self
                            .caller(p.get("caller").and_then(|c| serde_json::from_value(c.clone()).ok()).as_ref())
                            .await
                            .ok_or_else(|| {
                                RpcError::refused("run this from a session, or say which one with --target")
                            })?;
                        format!("{}@{}", c.tab().unwrap_or(&c.session_id), node.name)
                    }
                };
                let req = l::EventsRequest {
                    target,
                    channel: p.get("channel").and_then(Value::as_str).map(str::to_string),
                    all: p.get("all").and_then(Value::as_bool).unwrap_or(false),
                    peek: p.get("peek").and_then(Value::as_bool).unwrap_or(false),
                    limit: p.get("limit").and_then(Value::as_u64).map(|n| n.min(500) as u32),
                };
                peer.request::<_, Value>(l::method::EVENTS, &req).await
            }
            "node/readings" => crate::readings::latest(node, &p).await,
            "node/secrets_forget" => match crate::sync::forget_all(node).await {
                Ok(n) => answer(&json!({"forgotten": n})),
                Err(e) => Err(RpcError::refused(e.to_string())),
            },
            "node/me" => {
                let c =
                    self.caller(p.get("caller").and_then(|c| serde_json::from_value(c.clone()).ok()).as_ref()).await;
                let caller = c.as_ref().map(|s| s.tab().unwrap_or(&s.session_id).to_string()).unwrap_or("-".into());
                let mut local = json!({"node": node.name, "caller": caller, "address": format!("{caller}@{}", node.name),
                    "harness": c.as_ref().and_then(|s| s.meta.harness.clone())});
                match node.link.peer().await {
                    Some(peer) => {
                        let me: Value = peer.request(l::method::ME, &l::MeRequest { caller }).await?;
                        local["me"] = me;
                    }
                    None => local["offline"] = json!(true),
                }
                answer(&local)
            }
            "node/services" => {
                let mut services = vec![];
                for p in &node.plugins {
                    for s in p.services().await {
                        let mut v = serde_json::to_value(&s).map_err(|e| RpcError::internal(e.to_string()))?;
                        v["plugin"] = json!(p.name());
                        v["hosts"] = json!(s.hosts);
                        services.push(v);
                    }
                }
                answer(&json!({"services": services}))
            }
            "node/session_open" => {
                let o: SessionOpen = params(p)?;
                let caller = self.caller(o.caller.as_ref()).await;
                answer(&crate::start::open(node, o.spec, o.on.as_deref(), o.why, caller.as_ref()).await?)
            }
            "node/session_handoff" => {
                let c =
                    self.caller(p.get("caller").and_then(|c| serde_json::from_value(c.clone()).ok()).as_ref()).await;
                let ask: crate::handoffs::Ask = serde_json::from_value(p.clone()).map_err(RpcError::invalid_params)?;
                answer(&crate::handoffs::start(node, ask, c.as_ref()).await?)
            }
            "node/handoff_get" => {
                let c =
                    self.caller(p.get("caller").and_then(|c| serde_json::from_value(c.clone()).ok()).as_ref()).await;
                let id = p["id"].as_str().ok_or_else(|| RpcError::invalid_params("say which handoff"))?;
                let why = p["why"].as_str().map(str::to_string);
                answer(&crate::handoffs::get(node, id, c.as_ref(), why).await?)
            }
            "node/transcript_search" => {
                let c =
                    self.caller(p.get("caller").and_then(|c| serde_json::from_value(c.clone()).ok()).as_ref()).await;
                let caller = c.as_ref().map(|s| s.tab().unwrap_or(&s.session_id).to_string()).unwrap_or("-".into());
                let query: l::TranscriptQuery = serde_json::from_value(p.clone()).map_err(RpcError::invalid_params)?;
                answer(&crate::transcripts::search(node, p["on"].as_str(), query, caller).await?)
            }
            "node/transcript_read" => {
                let c =
                    self.caller(p.get("caller").and_then(|c| serde_json::from_value(c.clone()).ok()).as_ref()).await;
                let caller = c.as_ref().map(|s| s.tab().unwrap_or(&s.session_id).to_string()).unwrap_or("-".into());
                let to = p["to"].as_str().ok_or_else(|| RpcError::invalid_params("say which session"))?.to_string();
                let why = p["why"].as_str().map(str::to_string);
                let range: l::TranscriptRange = serde_json::from_value(p.clone()).map_err(RpcError::invalid_params)?;
                answer(&crate::transcripts::read(node, &to, range, caller, why).await?)
            }
            "node/session_close" | "node/session_answer" | "node/session_command" => {
                let a: SessionAct = params(p)?;
                let caller = self.caller(a.caller.as_ref()).await;
                let name = caller.as_ref().and_then(|c| c.tab().map(str::to_string)).unwrap_or_else(|| "-".into());
                let action = match method {
                    "node/session_close" => l::SessionAction::Close,
                    "node/session_command" => l::SessionAction::Command {
                        command: a.command.ok_or_else(|| RpcError::invalid_params("say which command"))?,
                    },
                    _ => {
                        let prompt =
                            a.prompt.ok_or_else(|| RpcError::invalid_params("say which prompt with --prompt"))?;
                        l::SessionAction::Answer { prompt, answers: a.answers }
                    }
                };
                answer(&crate::act::act(node, &a.to, action, a.why, name).await?)
            }
            "node/role_activate" | "node/role_deactivate" | "node/role_list" => {
                let c =
                    self.caller(p.get("caller").and_then(|c| serde_json::from_value(c.clone()).ok()).as_ref()).await;
                let caller = c.as_ref().map(|s| s.tab().unwrap_or(&s.session_id).to_string()).unwrap_or("-".into());
                let peer = node.link.peer().await.ok_or_else(|| {
                    RpcError::not_here(format!(
                        "{} isn't linked to a control plane now, which holds role windows",
                        node.name
                    ))
                })?;
                let role = || p["role"].as_str().map(str::to_string).ok_or_else(|| RpcError::invalid_params("role"));
                let older = |e: RpcError| {
                    if e.code == fleetlayer_protocol::rpc::code::METHOD_NOT_FOUND {
                        RpcError::refused(
                            "the control plane is older than this node and has no role windows: update it",
                        )
                    } else {
                        e
                    }
                };
                match method {
                    "node/role_activate" => {
                        let req = l::RoleActivate {
                            role: role()?,
                            hold: p["for"].as_str().unwrap_or("1h").to_string(),
                            caller,
                            justification: p["why"].as_str().map(str::to_string),
                        };
                        peer.request::<_, Value>(l::method::ROLE_ACTIVATE, &req).await.map_err(older)
                    }
                    "node/role_deactivate" => {
                        let req = l::RoleDeactivate { role: role()?, caller };
                        peer.request::<_, Value>(l::method::ROLE_DEACTIVATE, &req).await.map_err(older)
                    }
                    _ => peer
                        .request::<_, Value>(l::method::ROLE_WINDOWS, &l::RoleWindowsRequest { caller })
                        .await
                        .map_err(older),
                }
            }
            "node/host_ask" => {
                // Always in this node's own name: the control plane files requests only for the node that asks.
                let to = p["to"].as_str().unwrap_or_default().to_string();
                if !to.starts_with("host:") {
                    return Err(RpcError::invalid_params("to: host:<name>[:<port>]"));
                }
                let req = l::AccessCheck {
                    tenant: String::new(),
                    who: format!("-@{}", node.name),
                    can: "connect".into(),
                    to,
                    why: p["why"].as_str().map(str::to_string),
                    peek: false,
                };
                let peer = node.link.peer().await.ok_or_else(|| {
                    RpcError::not_here(format!("{} isn't linked to a control plane now, so it can't ask", node.name))
                })?;
                peer.request_within::<_, Value>(l::method::ACCESS_CHECK, &req, std::time::Duration::from_secs(15)).await
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
                    None => {
                        let label = match s.from.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
                            Some(l) => format!("{} ({})", l.chars().take(60).collect::<String>(), node.name),
                            None => format!("a shell on {}", node.name),
                        };
                        From { label, ..Default::default() }
                    }
                };
                let marks = crate::deliver::Marks { untrusted: s.untrusted, subkind: s.subkind.clone() };
                answer(&crate::deliver::send(node, &s.to, &s.text, from, caller.as_ref(), s.why.clone(), marks).await?)
            }
            _ => Err(RpcError::method_not_found(method)),
        }
    }
}

/// A `--target` that isn't a plugin: a plugin subscribes and reads what it missed itself, through its own connection
/// (`_fleetlayer.dev/event/*`), never through a session's command.
fn not_a_plugin(target: String) -> Result<String, RpcError> {
    if target.starts_with("plugin:") {
        return Err(RpcError::refused(format!(
            "{target} is a plugin: a plugin subscribes to channels and reads the events it missed itself"
        )));
    }
    Ok(target)
}
