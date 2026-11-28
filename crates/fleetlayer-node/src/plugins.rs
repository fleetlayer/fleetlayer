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
/// How often a plugin whose sessions can't be read is asked again.
const RETRY_FAILING: Duration = Duration::from_secs(30);
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
    /// Why its sessions can't be read now (`session/list` failed): it runs but can't do its job. Cleared by the next
    /// list that works, and when it restarts.
    failing: Option<String>,
    /// What it says is wrong (`_fleetlayer.dev/plugin/status`); cleared when it restarts.
    reported: Option<String>,
    /// When it last sent a status report: one that reported once and then goes quiet is failing.
    last_report: Option<(Instant, Timestamp)>,
    /// HTTP services it announced (`_fleetlayer.dev/service/listening`), by name; cleared when it exits.
    services: std::collections::BTreeMap<String, ext::ServiceListening>,
    /// Pages spaces it hosts (`_fleetlayer.dev/pages/spaces`); cleared when it exits.
    pages_spaces: Vec<String>,
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
    /// Running, but its sessions can't be read: why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    pub stderr: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<ext::ServiceListening>,
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
                failing: None,
                reported: None,
                last_report: None,
                services: Default::default(),
                pages_spaces: vec![],
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
            problem: problem(&i, Instant::now()),
            stderr: i.stderr.iter().cloned().collect(),
            services: i.services.values().cloned().collect(),
        }
    }

    /// The pages spaces it hosts.
    pub async fn pages_spaces(&self) -> Vec<String> {
        self.inner.lock().await.pages_spaces.clone()
    }

    /// The HTTP services it announced.
    pub async fn services(&self) -> Vec<ext::ServiceListening> {
        self.inner.lock().await.services.values().cloned().collect()
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
                {
                    let mut i = plugin.inner.lock().await;
                    i.services.clear();
                    i.pages_spaces.clear();
                }
                crate::pages::announce(&node).await;
                node.registry.plugin_gone(plugin.name()).await;
                return;
            }
        };
        {
            let mut i = plugin.inner.lock().await;
            i.peer = None;
            i.restarts += 1;
            i.services.clear();
            i.pages_spaces.clear();
        }
        crate::pages::announce(&node).await;
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

    // Its stderr is its log: keep the last lines for `fleetlayer status`, and pass them to the node's log.
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
                    // A plugin with no `config:` section gets `{}`, so every setting takes its default (null would
                    // fail a plugin that reads its section as a struct).
                    config: if cfg.config.is_null() { json!({}) } else { cfg.config.clone() },
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
        i.failing = None;
        i.reported = None;
        i.last_report = None;
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
    // A plugin whose sessions can't be read is asked again until they can (herdr installed, or back on PATH).
    let mut retry = tokio::time::interval(RETRY_FAILING);
    retry.tick().await;
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
            _ = retry.tick(), if !polls => {
                if plugin.inner.lock().await.failing.is_some() {
                    relist(node, plugin).await;
                }
            }
        }
    }
}

/// Read a plugin's sessions again.
pub async fn relist(node: &Arc<Node>, plugin: &Arc<PluginHandle>) {
    let Some(peer) = plugin.peer().await else { return };
    let roles = plugin.roles().await;
    let role = if roles.session_manager.is_some() { Role::Manager } else { Role::Harness };
    match peer.request::<_, ListSessionsResponse>(acp::method::SESSION_LIST, &ListSessionsRequest::default()).await {
        Ok(list) => {
            if plugin.inner.lock().await.failing.take().is_some() {
                tracing::info!(plugin = plugin.name(), "session/list works again");
            }
            node.registry.replace(plugin.name(), role, list.sessions).await
        }
        Err(e) => {
            let mut i = plugin.inner.lock().await;
            if i.failing.is_none() {
                tracing::warn!(plugin = plugin.name(), "session/list failed: {e}");
            }
            i.failing = Some(format!("its sessions can't be read: {}", e.message));
        }
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

/// A plugin that sent a status report and then nothing for this long is failing: it may be up but stuck.
const REPORT_SILENCE: Duration = Duration::from_secs(5 * 60);

/// What's wrong with a running plugin, if anything: its sessions can't be read, it says so, or it stopped reporting.
fn problem(i: &Inner, now: Instant) -> Option<String> {
    if let Some(f) = &i.failing {
        return Some(f.clone());
    }
    if let Some((at, wall)) = i.last_report
        && now.saturating_duration_since(at) > REPORT_SILENCE
    {
        return Some(format!("it stopped reporting at {}", wall.round(jiff::Unit::Second).unwrap_or(wall)));
    }
    i.reported.clone()
}

/// The `plugin.status` reading for a plugin's report: keyed by the plugin's own name, so a plugin can only report
/// itself, with `detail` capped.
fn status_reading(plugin: &str, r: ext::PluginStatusReport) -> Value {
    let detail = match serde_json::to_vec(&r.detail) {
        Ok(b) if b.len() <= ext::PLUGIN_STATUS_DETAIL_MAX => r.detail,
        _ => json!({"truncated": true}),
    };
    json!({"readings": [{
        "kind": "plugin.status",
        "key": plugin,
        "at": Timestamp::now(),
        "data": {"plugin": plugin, "problem": r.problem, "detail": detail},
    }]})
}

/// How long the control plane gets to store and deliver a plugin's event.
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(20);

/// A plugin's event (protocol section 6), checked and passed to the control plane as `link/publish` from
/// `plugin:<name>`, so the policy decides `publish` on its channel for the plugin itself. Answers the control plane's
/// result (`{duplicate, deliveries}`); an error while this node isn't linked, so the plugin keeps it and tries again.
async fn publish_event(
    node: &Node,
    plugin: &str,
    p: Value,
) -> Result<fleetlayer_protocol::link::PublishResult, RpcError> {
    let e: ext::Event = params(p)?;
    if let Some(why) = event_problem(&e) {
        return Err(RpcError::invalid_params(why));
    }
    let peer = node.link.peer().await.ok_or_else(|| {
        RpcError::not_here(format!("{} isn't linked to a control plane now; try again later", node.name))
    })?;
    let mut v = serde_json::to_value(&e).map_err(RpcError::internal)?;
    v["caller"] = json!(format!("plugin:{plugin}"));
    peer.request_within(fleetlayer_protocol::link::method::PUBLISH, &v, PUBLISH_TIMEOUT).await
}

/// How long a plugin has to take an event.
const EVENT_DELIVER_TIMEOUT: Duration = Duration::from_secs(5);

/// Hand an event to one of this node's plugins, which subscribed to its channel: only a running plugin that still has
/// the `event.subscribe` grant takes it.
pub async fn deliver_event(node: &Node, plugin: &str, d: &ext::EventDelivery) -> Result<(), RpcError> {
    let p = node.plugin(plugin).ok_or_else(|| RpcError::not_here(format!("plugin {plugin} isn't configured")))?;
    if !p.config.granted("event.subscribe") {
        return Err(RpcError::refused(format!("event.subscribe isn't granted to plugin {plugin} in node.yaml")));
    }
    let peer = p.peer().await.ok_or_else(|| RpcError::not_here(format!("plugin {plugin} isn't running")))?;
    peer.request_within::<_, Value>(ext::method::EVENT_DELIVER, d, EVENT_DELIVER_TIMEOUT).await.map(|_| ())
}

/// A plugin's subscribe, unsubscribe or catch-up, as itself (`plugin:<name>@<node>`), through the control plane.
async fn plugin_events(node: &Node, plugin: &str, method: &str, p: Value) -> Result<Value, RpcError> {
    use fleetlayer_protocol::link as l;
    let peer = node.link.peer().await.ok_or_else(|| {
        RpcError::not_here(format!("{} isn't linked to a control plane now; try again later", node.name))
    })?;
    let target = format!("plugin:{plugin}@{}", node.name);
    let channel = || p["channel"].as_str().filter(|c| !c.is_empty()).map(str::to_string);
    match method {
        ext::method::EVENT_MISSED => {
            let r = l::EventsRequest {
                target,
                channel: channel(),
                all: p["all"].as_bool().unwrap_or(false),
                peek: p["peek"].as_bool().unwrap_or(false),
                limit: p["limit"].as_u64().map(|n| n.min(500) as u32),
            };
            peer.request_within(l::method::EVENTS, &r, PUBLISH_TIMEOUT).await
        }
        _ => {
            let channel = channel().ok_or_else(|| RpcError::invalid_params("channel"))?;
            let s = json!({"channel": channel, "subject": p["subject"].as_str(), "target": target});
            let m = if method == ext::method::EVENT_SUBSCRIBE { l::method::SUBSCRIBE } else { l::method::UNSUBSCRIBE };
            peer.request_within(m, &s, PUBLISH_TIMEOUT).await
        }
    }
}

/// A plugin starts or resumes a session, here or on another node. Always through the control plane, even here, so
/// the policy decides it as the plugin (`start` for `plugin:<name>@<node>`, with its start limits) and it's audited;
/// a node without a control plane can't. The first prompt arrives as a message from the plugin.
async fn plugin_start(
    node: &Node,
    plugin: &str,
    p: Value,
) -> Result<fleetlayer_protocol::link::SessionStarted, RpcError> {
    use fleetlayer_protocol::link as l;
    let mut spec: l::StartSpec = serde_json::from_value(p.clone()).map_err(RpcError::invalid_params)?;
    let on = p["on"].as_str().filter(|n| !n.is_empty()).unwrap_or(&node.name).to_string();
    let why = p["why"].as_str().map(str::to_string);
    if let Some(text) = spec.prompt.take() {
        let from = ext::From {
            label: format!("plugin {plugin} ({})", node.name),
            name: Some(format!("plugin:{plugin}")),
            node: Some(node.name.clone()),
            session_id: None,
        };
        spec.prompt = Some(crate::deliver::render_message(&from, &text, None));
    }
    let peer = node.link.peer().await.ok_or_else(|| {
        RpcError::not_here(format!(
            "{} isn't linked to a control plane now, and a plugin's start is always decided there",
            node.name
        ))
    })?;
    let req = l::SessionStart { node: on, spec, caller: format!("plugin:{plugin}"), justification: why };
    peer.request_within(l::method::SESSION_START, &req, crate::start::REMOTE_TIMEOUT).await
}

/// What's wrong with a plugin's event, if anything.
fn event_problem(e: &ext::Event) -> Option<String> {
    let name_ok =
        |s: &str| !s.is_empty() && s.len() <= 100 && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c));
    if !name_ok(&e.channel) {
        return Some(format!("channel {:?}: 1 to 100 letters, digits and - _ . :", e.channel));
    }
    if !name_ok(&e.kind) {
        return Some(format!("kind {:?}: 1 to 100 letters, digits and - _ . :", e.kind));
    }
    if e.summary.trim().is_empty() {
        return Some("an event needs a summary".into());
    }
    if e.subject.as_deref().is_some_and(|s| s.is_empty() || s.len() > 500) {
        return Some("subject: 1 to 500 bytes".into());
    }
    if e.dedupe_key.as_deref().is_some_and(|k| k.is_empty() || k.len() > 500) {
        return Some("dedupeKey: 1 to 500 bytes".into());
    }
    None
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
            ext::method::READING_SUBSCRIBE => {
                self.grant("reading.subscribe")?;
                crate::readings::subscribe(&self.node, self.plugin.name(), &p).await
            }
            ext::method::READING_PUBLISH => {
                // Accepted as a request too.
                self.grant("reading.publish")?;
                crate::readings::publish(&self.node, &p).await;
                Ok(json!({}))
            }
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
            ext::method::TRANSCRIPT_READ => {
                self.grant("transcript.read")?;
                let to = p["to"].as_str().ok_or_else(|| RpcError::invalid_params("to"))?.to_string();
                let why = p["why"].as_str().map(str::to_string);
                let range: fleetlayer_protocol::link::TranscriptRange =
                    serde_json::from_value(p.clone()).map_err(RpcError::invalid_params)?;
                let caller = format!("plugin:{}", self.plugin.name());
                answer(&crate::transcripts::read(&self.node, &to, range, caller, why).await?)
            }
            ext::method::FLEET_QUERY => {
                self.grant("fleet.query")?;
                let peer = self.node.link.peer().await.ok_or_else(|| {
                    RpcError::not_here(format!("{} isn't linked to a control plane now", self.node.name))
                })?;
                peer.request::<_, Value>(fleetlayer_protocol::link::method::FLEET, &json!({"summaries": false})).await
            }
            ext::method::SESSION_ANSWER | ext::method::SESSION_CLOSE | ext::method::SESSION_COMMAND => {
                use fleetlayer_protocol::link::SessionAction;
                self.grant(match method {
                    ext::method::SESSION_CLOSE => "session.close",
                    ext::method::SESSION_COMMAND => "session.command",
                    _ => "session.answer",
                })?;
                let to = p["to"].as_str().ok_or_else(|| RpcError::invalid_params("to"))?.to_string();
                let why = p["why"].as_str().map(str::to_string);
                let action = match method {
                    ext::method::SESSION_CLOSE => SessionAction::Close,
                    ext::method::SESSION_COMMAND => SessionAction::Command {
                        command: p["command"].as_str().ok_or_else(|| RpcError::invalid_params("command"))?.to_string(),
                    },
                    _ => {
                        let prompt =
                            p["prompt"].as_str().ok_or_else(|| RpcError::invalid_params("prompt"))?.to_string();
                        let answers = serde_json::from_value(p["answers"].clone()).map_err(RpcError::invalid_params)?;
                        SessionAction::Answer { prompt, answers }
                    }
                };
                let caller = format!("plugin:{}", self.plugin.name());
                answer(&crate::act::act(&self.node, &to, action, why, caller).await?)
            }
            ext::method::SESSION_START => {
                self.grant("session.start")?;
                answer(&plugin_start(&self.node, self.plugin.name(), p).await?)
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
                let marks = crate::deliver::Marks { untrusted: m.untrusted, subkind: m.subkind.clone() };
                let d = crate::deliver::send(&self.node, &m.to, &m.text, from, None, None, marks).await?;
                answer(&d)
            }
            ext::method::SYNC_PUBLISH => {
                self.grant("sync.publish")?;
                crate::sync::publish(&self.node, self.plugin.name(), params(p)?).await
            }
            ext::method::ACCESS_CHECK => {
                // A service (pages, the hub) asking whether someone may do something; the control plane decides.
                self.grant("access.check")?;
                let req: fleetlayer_protocol::link::AccessCheck = params(p)?;
                let peer = self.node.link.peer().await.ok_or_else(|| {
                    RpcError::not_here("this node isn't linked to a control plane now, so nothing can be checked")
                })?;
                peer.request_within(fleetlayer_protocol::link::method::ACCESS_CHECK, &req, Duration::from_secs(10))
                    .await
            }
            ext::method::EVENT_PUBLISH => {
                self.grant("event.publish")?;
                answer(&publish_event(&self.node, self.plugin.name(), p).await?)
            }
            ext::method::EVENT_SUBSCRIBE | ext::method::EVENT_UNSUBSCRIBE | ext::method::EVENT_MISSED => {
                self.grant("event.subscribe")?;
                plugin_events(&self.node, self.plugin.name(), method, p).await
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
            ext::method::SERVICE_LISTENING => match serde_json::from_value::<ext::ServiceListening>(p) {
                Ok(s) => match s.problem() {
                    Some(why) => tracing::warn!(plugin = plugin.name(), "ignoring its service: {why}"),
                    None => {
                        tracing::info!(plugin = plugin.name(), service = s.name, url = s.url, "service listening");
                        plugin.inner.lock().await.services.insert(s.name.clone(), s);
                    }
                },
                Err(e) => tracing::warn!(plugin = plugin.name(), "bad {method}: {e}"),
            },
            ext::method::PAGES_SPACES => match serde_json::from_value::<Vec<String>>(p["spaces"].clone()) {
                Ok(spaces) => {
                    tracing::info!(plugin = plugin.name(), ?spaces, "pages spaces");
                    plugin.inner.lock().await.pages_spaces = spaces;
                    crate::pages::announce(&node).await;
                }
                Err(e) => tracing::warn!(plugin = plugin.name(), "bad {method}: {e}"),
            },
            ext::method::PLUGIN_STATUS => match serde_json::from_value::<ext::PluginStatusReport>(p) {
                Ok(r) => {
                    let changed = {
                        let mut i = plugin.inner.lock().await;
                        let changed = i.reported != r.problem;
                        i.reported = r.problem.clone();
                        i.last_report = Some((Instant::now(), Timestamp::now()));
                        changed
                    };
                    if changed {
                        match &r.problem {
                            Some(why) => tracing::warn!(plugin = plugin.name(), "it reports a problem: {why}"),
                            None => tracing::info!(plugin = plugin.name(), "its problem cleared"),
                        }
                    }
                    crate::readings::publish(&node, &status_reading(plugin.name(), r)).await;
                }
                Err(e) => tracing::warn!(plugin = plugin.name(), "bad {method}: {e}"),
            },
            ext::method::READING_PUBLISH => {
                if !plugin.config.granted("reading.publish") {
                    tracing::warn!(plugin = plugin.name(), "reading.publish isn't granted in node.yaml; dropping");
                    return;
                }
                crate::readings::publish(&node, &p).await;
            }
            ext::method::EVENT_PUBLISH => {
                // The notification form: nobody hears the outcome, so a failure is only logged. Plugins that must
                // not lose an event send it as a request and keep it until it's taken.
                if !plugin.config.granted("event.publish") {
                    tracing::warn!(plugin = plugin.name(), "event.publish isn't granted in node.yaml; dropping");
                    return;
                }
                if let Err(e) = publish_event(&node, plugin.name(), p).await {
                    tracing::warn!(plugin = plugin.name(), "publishing its event: {}", e.message);
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plugins_event_is_checked_before_it_leaves() {
        let e = |channel: &str, summary: &str| ext::Event {
            channel: channel.into(),
            kind: "matrix.message".into(),
            subject: None,
            at: Timestamp::now(),
            dedupe_key: Some("matrix:$ev".into()),
            summary: summary.into(),
            body: Value::Null,
        };
        assert_eq!(event_problem(&e("infrastructure-alerts", "disk full")), None);
        assert!(event_problem(&e("", "x")).unwrap().contains("channel"));
        assert!(event_problem(&e("has space", "x")).unwrap().contains("channel"));
        assert!(event_problem(&e("alerts", "  ")).unwrap().contains("summary"));
    }

    #[test]
    fn a_status_report_becomes_a_reading_keyed_by_the_plugin_with_its_detail_capped() {
        let r = ext::PluginStatusReport { problem: Some("stuck".into()), detail: json!({"waiting": 2}) };
        let v = status_reading("summaries", r);
        let reading = &v["readings"][0];
        assert_eq!(reading["kind"], "plugin.status");
        assert_eq!(reading["key"], "summaries");
        assert_eq!(reading["data"], json!({"plugin": "summaries", "problem": "stuck", "detail": {"waiting": 2}}));
        // A report can't name another plugin, and a big detail is cut.
        let big =
            ext::PluginStatusReport { problem: None, detail: json!({"key": "x".repeat(5000), "plugin": "other"}) };
        let v = status_reading("summaries", big);
        assert_eq!(v["readings"][0]["key"], "summaries");
        assert_eq!(v["readings"][0]["data"]["detail"], json!({"truncated": true}));
        assert!(v["readings"][0]["data"]["problem"].is_null());
    }

    #[test]
    fn a_plugin_that_reported_and_went_quiet_is_failing() {
        let mut i = Inner {
            state: State::Starting,
            peer: None,
            roles: Roles::default(),
            info: None,
            restarts: 0,
            stderr: VecDeque::new(),
            relist_pending: false,
            failing: None,
            reported: None,
            last_report: None,
            services: Default::default(),
            pages_spaces: vec![],
        };
        let t0 = Instant::now();
        // Never reported: never "stopped reporting".
        assert_eq!(problem(&i, t0 + Duration::from_secs(3600)), None);
        let wall: Timestamp = "2026-11-27T10:00:00Z".parse().unwrap();
        i.last_report = Some((t0, wall));
        assert_eq!(problem(&i, t0 + Duration::from_secs(299)), None);
        assert_eq!(
            problem(&i, t0 + Duration::from_secs(301)).as_deref(),
            Some("it stopped reporting at 2026-11-27T10:00:00Z")
        );
        // Its own problem shows while it reports; a list that fails comes first.
        i.reported = Some("stuck".into());
        assert_eq!(problem(&i, t0 + Duration::from_secs(10)).as_deref(), Some("stuck"));
        i.failing = Some("its sessions can't be read: x".into());
        assert_eq!(problem(&i, t0 + Duration::from_secs(10)).as_deref(), Some("its sessions can't be read: x"));
    }
}
