//! Running plugins: each is a child process the node starts, initializes and pings, and restarts with backoff when it
//! exits or stops answering (`docs/protocol.md` sections 3 and 11). A plugin's crash never takes the node down.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use fleetlayer_plugin::{Handler, Peer, answer, params};
use fleetlayer_protocol::acp::{
    self, Capabilities, Implementation, InitializeRequest, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, SessionNotification, SessionUpdate,
};
use fleetlayer_protocol::ext::{
    self, MessageSend, NodeMeta, PluginMeta, Roles, SessionEnded, SessionRenamed, meta_key,
};
use fleetlayer_protocol::rpc::RpcError;
use jiff::Timestamp;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;

use crate::Node;
use crate::config::PluginConfig;
use crate::sessions::Role;

const PING_EVERY: Duration = Duration::from_secs(30);
const PING_TIMEOUT: Duration = Duration::from_secs(10);
const MISSED_PINGS: u32 = 3;
const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// A plugin that ran this long before stopping starts again after the shortest wait.
const HEALTHY_RUN: Duration = Duration::from_secs(300);
const STDERR_LINES: usize = 50;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum State {
    Starting,
    Running {
        since: Timestamp,
    },
    Restarting {
        at: Timestamp,
        reason: String,
    },
    /// It won't be restarted (an unsupported protocol version, or switched off).
    Broken {
        reason: String,
    },
}

pub struct PluginHandle {
    pub config: PluginConfig,
    inner: Mutex<Inner>,
}

struct Inner {
    state: State,
    peer: Option<Peer>,
    roles: Roles,
    info: Option<Implementation>,
    restarts: u32,
    stderr: VecDeque<String>,
    relist_pending: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginStatus {
    pub name: String,
    pub command: String,
    #[serde(flatten)]
    pub state: State,
    pub version: Option<String>,
    pub roles: Vec<&'static str>,
    pub restarts: u32,
    pub stderr: Vec<String>,
}

impl PluginHandle {
    pub fn new(config: PluginConfig) -> Arc<PluginHandle> {
        Arc::new(PluginHandle {
            config,
            inner: Mutex::new(Inner {
                state: State::Starting,
                peer: None,
                roles: Roles::default(),
                info: None,
                restarts: 0,
                stderr: VecDeque::new(),
                relist_pending: false,
            }),
        })
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    pub async fn peer(&self) -> Option<Peer> {
        self.inner.lock().await.peer.clone()
    }

    pub async fn roles(&self) -> Roles {
        self.inner.lock().await.roles.clone()
    }

    pub async fn status(&self) -> PluginStatus {
        let i = self.inner.lock().await;
        let mut roles = vec![];
        let r = &i.roles;
        for (set, name) in [
            (r.session_manager.is_some(), "session_manager"),
            (r.harness.is_some(), "harness"),
            (r.event_source.is_some(), "event_source"),
            (r.action.is_some(), "action"),
            (r.sync_target.is_some(), "sync_target"),
            (r.sync_source.is_some(), "sync_source"),
            (r.observer.is_some(), "observer"),
        ] {
            if set {
                roles.push(name);
            }
        }
        PluginStatus {
            name: self.config.name.clone(),
            command: self.config.command.clone(),
            state: i.state.clone(),
            version: i.info.as_ref().map(|v| v.version.clone()),
            roles,
            restarts: i.restarts,
            stderr: i.stderr.iter().cloned().collect(),
        }
    }

    async fn set_state(&self, state: State) {
        self.inner.lock().await.state = state;
    }
}

/// Keep one plugin running for as long as the node runs.
pub async fn supervise(node: Arc<Node>, plugin: Arc<PluginHandle>) {
    if !plugin.config.enabled {
        plugin.set_state(State::Broken { reason: "switched off in node.yaml".into() }).await;
        return;
    }
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        let reason = match run_once(&node, &plugin).await {
            Ok(reason) => reason,
            Err(Fatal(reason)) => {
                tracing::error!(plugin = plugin.name(), "{reason}; not restarting it");
                plugin.set_state(State::Broken { reason }).await;
                node.registry.plugin_gone(plugin.name()).await;
                return;
            }
        };
        {
            let mut i = plugin.inner.lock().await;
            i.peer = None;
            i.restarts += 1;
        }
        node.registry.plugin_gone(plugin.name()).await;
        if started.elapsed() >= HEALTHY_RUN {
            backoff = Duration::from_secs(1);
        }
        let at = Timestamp::now() + jiff::SignedDuration::try_from(backoff).unwrap_or_default();
        tracing::warn!(plugin = plugin.name(), "{reason}; restarting in {}s", backoff.as_secs());
        plugin.set_state(State::Restarting { at, reason }).await;
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
        plugin.set_state(State::Starting).await;
    }
}

/// A reason never to restart the plugin.
struct Fatal(String);

/// Start the plugin and run it until it stops. Ok holds why it stopped.
async fn run_once(node: &Arc<Node>, plugin: &Arc<PluginHandle>) -> Result<String, Fatal> {
    let cfg = &plugin.config;
    let mut child = match tokio::process::Command::new(&cfg.command)
        .args(&cfg.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Ok(format!("couldn't start {}: {e}", cfg.command)),
    };
    let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Ok("its pipes didn't open".into());
    };

    // Its stderr is its log: keep the last lines for `fleetlayer plugins`, and pass them to the node's log.
    let log_plugin = plugin.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::info!(plugin = log_plugin.name(), "{line}");
            let mut i = log_plugin.inner.lock().await;
            if i.stderr.len() == STDERR_LINES {
                i.stderr.pop_front();
            }
            i.stderr.push_back(line);
        }
    });

    let handler = Arc::new(FromPlugin { node: node.clone(), plugin: plugin.clone() });
    let (peer, reader) = fleetlayer_plugin::connect(stdout, stdin, handler);

    let init = InitializeRequest {
        protocol_version: fleetlayer_protocol::ACP_PROTOCOL,
        client_capabilities: Capabilities {
            meta: Some(ext::meta_with(
                meta_key::NODE,
                &NodeMeta {
                    protocol: fleetlayer_protocol::PROTOCOL,
                    node: node.name.clone(),
                    machine: node.machine.clone(),
                    user: node.user.clone(),
                    home: node.home.display().to_string(),
                    config: cfg.config.clone(),
                },
            )),
            ..Default::default()
        },
        client_info: Some(Implementation {
            name: "fleetlayer-node".into(),
            title: None,
            version: env!("CARGO_PKG_VERSION").into(),
        }),
    };
    let resp: InitializeResponse =
        match peer.request_within(acp::method::INITIALIZE, &init, Duration::from_secs(15)).await {
            Ok(r) => r,
            Err(e) => return Ok(format!("initialize failed: {e}")),
        };
    let meta: Option<PluginMeta> = ext::get_meta(resp.agent_capabilities.meta.as_ref(), meta_key::PLUGIN)
        .map_err(|e| Fatal(format!("its plugin capabilities don't parse: {e}")))?;
    let Some(meta) = meta else {
        return Err(Fatal("it's a plain ACP agent, which this node can't run yet".into()));
    };
    if meta.protocol != fleetlayer_protocol::PROTOCOL {
        return Err(Fatal(format!(
            "it speaks Fleetlayer protocol {}, and this node speaks {}",
            meta.protocol,
            fleetlayer_protocol::PROTOCOL
        )));
    }
    {
        let mut i = plugin.inner.lock().await;
        i.roles = meta.roles.clone();
        i.info = resp.agent_info.clone();
        i.peer = Some(peer.clone());
        i.state = State::Running { since: Timestamp::now() };
    }
    tracing::info!(plugin = plugin.name(), version = ?resp.agent_info.map(|i| i.version), "plugin running");

    if meta.roles.session_manager.is_some() || meta.roles.harness.is_some() {
        relist(node, plugin).await;
    }
    let polls = meta.roles.session_manager.as_ref().is_some_and(|r| !r.events);

    let mut reader = reader;
    let mut missed = 0;
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    let mut poll = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            status = child.wait() => {
                return Ok(match status {
                    Ok(s) => format!("it exited ({s})"),
                    Err(e) => format!("waiting for it failed: {e}"),
                });
            }
            _ = &mut reader => return Ok("it closed its stdout".into()),
            _ = ping.tick() => {
                match peer.request_within::<_, Value>(ext::method::PING, &json!({}), PING_TIMEOUT).await {
                    Ok(_) => missed = 0,
                    Err(_) => {
                        missed += 1;
                        if missed >= MISSED_PINGS {
                            let _ = child.kill().await;
                            return Ok(format!("it missed {MISSED_PINGS} pings in a row"));
                        }
                    }
                }
            }
            _ = poll.tick(), if polls => relist(node, plugin).await,
        }
    }
}

/// Read a plugin's sessions again.
pub async fn relist(node: &Arc<Node>, plugin: &Arc<PluginHandle>) {
    let Some(peer) = plugin.peer().await else { return };
    let roles = plugin.roles().await;
    let role = if roles.session_manager.is_some() { Role::Manager } else { Role::Harness };
    match peer.request::<_, ListSessionsResponse>(acp::method::SESSION_LIST, &ListSessionsRequest::default()).await {
        Ok(list) => node.registry.replace(plugin.name(), role, list.sessions).await,
        Err(e) => tracing::warn!(plugin = plugin.name(), "session/list failed: {e}"),
    }
}

/// Read a plugin's sessions again soon, once however many times it asks within the wait.
async fn relist_soon(node: Arc<Node>, plugin: Arc<PluginHandle>) {
    {
        let mut i = plugin.inner.lock().await;
        if i.relist_pending {
            return;
        }
        i.relist_pending = true;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    plugin.inner.lock().await.relist_pending = false;
    relist(&node, &plugin).await;
}

/// What a plugin sends the node.
struct FromPlugin {
    node: Arc<Node>,
    plugin: Arc<PluginHandle>,
}

impl FromPlugin {
    fn grant(&self, what: &str) -> Result<(), RpcError> {
        if self.plugin.config.granted(what) {
            Ok(())
        } else {
            Err(RpcError::refused(format!("{what} isn't granted to plugin {} in node.yaml", self.plugin.name())))
        }
    }
}

#[async_trait]
impl Handler for FromPlugin {
    async fn request(&self, peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
        match method {
            ext::method::PING => Ok(json!({})),
            ext::method::SESSIONS_SUBSCRIBE => {
                self.grant("sessions.watch")?;
                let mut changes = self.node.registry.subscribe();
                let (node, peer) = (self.node.clone(), peer.clone());
                tokio::spawn(async move {
                    use tokio::sync::broadcast::error::RecvError;
                    while !peer.is_closed() {
                        let update = match changes.recv().await {
                            Ok(crate::sessions::Change::Reset) | Err(RecvError::Lagged(_)) => {
                                json!({"change": "reset", "sessions": node.registry.all().await})
                            }
                            Ok(c) => serde_json::to_value(&c).unwrap_or_default(),
                            Err(RecvError::Closed) => return,
                        };
                        peer.notify(ext::method::SESSIONS_UPDATE, &update);
                    }
                });
                // Messages session managers type in by themselves (herdr's auto-continue), which aren't the person's.
                let mut own = vec![];
                for p in &self.node.plugins {
                    if let Some(sm) = p.roles().await.session_manager {
                        own.extend(sm.own_messages);
                    }
                }
                answer(&json!({"sessions": self.node.registry.all().await, "ownMessages": own}))
            }
            ext::method::LOG => {
                tracing::info!(
                    plugin = self.plugin.name(),
                    "{}",
                    p.get("message").and_then(serde_json::Value::as_str).unwrap_or("")
                );
                Ok(json!({}))
            }
            ext::method::MESSAGE_SEND => {
                self.grant("message.send")?;
                let m: MessageSend = params(p)?;
                // A plugin is its own sender, `plugin:<name>@<node>` to access control: rules can give a plugin that
                // runs only approved actions (a runner) more reach than the agent on the same node.
                let from = ext::From {
                    label: format!("plugin {} ({})", self.plugin.name(), self.node.name),
                    name: Some(format!("plugin:{}", self.plugin.name())),
                    node: Some(self.node.name.clone()),
                    session_id: None,
                };
                let d = crate::deliver::send(&self.node, &m.to, &m.text, from, None).await?;
                answer(&d)
            }
            ext::method::SYNC_PUBLISH => {
                self.grant("sync.publish")?;
                crate::sync::publish(&self.node, self.plugin.name(), params(p)?).await
            }
            ext::method::SESSIONS_QUERY => {
                self.grant("sessions.query")?;
                answer(&json!({"sessions": self.node.registry.all().await}))
            }
            _ => Err(RpcError::method_not_found(method)),
        }
    }

    async fn notification(&self, _peer: &Peer, method: &str, p: Value) {
        let (node, plugin) = (self.node.clone(), self.plugin.clone());
        match method {
            acp::method::SESSION_UPDATE => {
                let Ok(n) = serde_json::from_value::<SessionNotification>(p) else { return };
                if let SessionUpdate::Info(u) = &n.update
                    && !node.registry.update(plugin.name(), &n.session_id, u).await
                {
                    relist_soon(node, plugin).await;
                }
            }
            ext::method::SESSION_ENDED => {
                if let Ok(e) = serde_json::from_value::<SessionEnded>(p) {
                    node.registry.ended(&e).await;
                }
            }
            ext::method::SESSION_RENAMED => {
                if let Ok(r) = serde_json::from_value::<SessionRenamed>(p) {
                    node.registry.renamed(&r.from, &r.to).await;
                }
            }
            ext::method::SESSIONS_CHANGED => relist_soon(node, plugin).await,
            ext::method::SUMMARY_PUBLISH => {
                if !plugin.config.granted("summary.publish") {
                    tracing::warn!(plugin = plugin.name(), "summary.publish isn't granted in node.yaml; dropping");
                    return;
                }
                if let Some(event) = p.get("event") {
                    node.link.send_or_hold(fleetlayer_protocol::link::method::SUMMARY, json!({"event": event})).await;
                }
            }
            ext::method::LOG => {
                tracing::info!(
                    plugin = plugin.name(),
                    "{}",
                    p.get("message").and_then(serde_json::Value::as_str).unwrap_or("")
                );
            }
            // Events, readings and sync go to the control plane, which this node doesn't link to yet.
            _ => tracing::debug!(plugin = plugin.name(), method, "notification not handled yet"),
        }
    }
}
