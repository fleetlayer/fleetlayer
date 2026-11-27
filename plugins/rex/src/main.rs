//! fleetlayer-rex: the Fleetlayer adapter for Rex, Superlogical's terminal and session manager
//! (https://www.superlogical.com/rex/docs). It takes the session-manager role (`docs/protocol.md` section 4): lists
//! the agents running in Rex terminals as sessions, with their status from program status reports (OSC 7501),
//! streams changes from `rex events`, opens and closes sessions, interrupts and types messages in, and streams
//! terminals for `fleetlayer attach`. It uses only Rex's public command line and server API, and never takes a
//! session over: the person keeps using Rex's apps as before.
//!
//! A session's address is its terminal block's ID (`block:<uuid>`).
//!
//! Config (its section of the node's config, all optional):
//! - `program`: the rex binary (default `rex`).
//! - `server`: the Rex server to use, as `rex -S` takes it (default: this user's own server).
//! - `claude_dir`: Claude Code's directory, where its session registry is (default `CLAUDE_CONFIG_DIR` or
//!   `~/.claude`).

mod rex;
mod terminal;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fleetlayer_plugin::{Handler, Peer, answer, params};
use fleetlayer_protocol::acp::{
    self, Capabilities, Implementation, InitializeRequest, InitializeResponse, ListSessionsResponse, SessionInfo,
    SessionInfoTag, SessionInfoUpdate, SessionNotification, SessionRef, SessionUpdate,
};
use fleetlayer_protocol::ext::{
    self, Deliver, Delivered, How, NodeMeta, OpenSession, OpenedSession, PluginMeta, Roles, SessionEnded,
    SessionManagerRole, SessionMeta, SessionRenamed, Status, Via, meta_key, unnamed_session_id,
};
use fleetlayer_protocol::rpc::{RpcError, code};
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::rex::{Agent, Event, Failure, Rex};

#[derive(Debug, Default, Deserialize)]
struct Config {
    program: Option<String>,
    server: Option<String>,
    claude_dir: Option<PathBuf>,
}

struct Settings {
    rex: Rex,
    claude_sessions: PathBuf,
}

/// What the plugin last knew about a terminal that runs an agent.
#[derive(Debug, Clone)]
struct Known {
    session: String,
    session_id: String,
    harness: String,
    status: Status,
}

#[derive(Default)]
struct Plugin {
    settings: Mutex<Option<Arc<Settings>>>,
    /// Each agent terminal's block ID → what the node knows it by.
    blocks: Mutex<HashMap<String, Known>>,
    events_started: Mutex<bool>,
}

fn session_id(a: &Agent) -> String {
    a.session_id.clone().unwrap_or_else(|| unnamed_session_id(&a.block))
}

fn session_info(a: &Agent) -> SessionInfo {
    let meta = SessionMeta {
        harness: Some(a.harness.clone()),
        address: Some(a.block.clone()),
        named: Some(a.session_id.is_some()),
        workspace: a.session_label.clone(),
        tab: a.tab.clone(),
        status: Some(a.status),
        status_changed_at: Some(Timestamp::now()),
        running: Some(true),
        human_attached: a.human_attached,
        opens_in: Some("Rex".into()),
        ..Default::default()
    };
    SessionInfo {
        session_id: session_id(a),
        cwd: a.cwd.clone(),
        title: a.tab.clone(),
        updated_at: Some(Timestamp::now()),
        meta: Some(ext::meta_with(meta_key::SESSION, &meta)),
    }
}

fn failure(e: Failure) -> RpcError {
    match e {
        Failure::NotHere(m) => RpcError::not_here(m),
        Failure::Other(m) => RpcError::refused(m),
    }
}

fn not_running(session_id: &str) -> RpcError {
    RpcError::not_here(format!("session {session_id} isn't running in Rex on this machine"))
}

impl Plugin {
    async fn settings(&self) -> Result<Arc<Settings>, RpcError> {
        self.settings.lock().await.clone().ok_or_else(|| RpcError::refused("initialize first"))
    }

    async fn snapshot(&self) -> Result<Vec<Agent>, RpcError> {
        let s = self.settings().await?;
        let agents = s.rex.snapshot(&s.claude_sessions).await.map_err(failure)?;
        let mut blocks = self.blocks.lock().await;
        blocks.clear();
        for a in &agents {
            blocks.insert(
                a.block.clone(),
                Known {
                    session: a.session.clone(),
                    session_id: session_id(a),
                    harness: a.harness.clone(),
                    status: a.status,
                },
            );
        }
        Ok(agents)
    }

    /// The Rex session and block a Fleetlayer session ID names (`@<block>` or a named session), taking a fresh
    /// snapshot when the last one doesn't have it.
    async fn locate(&self, session_id: &str) -> Result<(String, String), RpcError> {
        let find = |blocks: &HashMap<String, Known>| {
            blocks
                .iter()
                .find(|(b, k)| k.session_id == session_id || session_id.strip_prefix('@') == Some(b.as_str()))
                .map(|(b, k)| (k.session.clone(), b.clone()))
        };
        if let Some(found) = find(&*self.blocks.lock().await) {
            return Ok(found);
        }
        self.snapshot().await?;
        find(&*self.blocks.lock().await).ok_or_else(|| not_running(session_id))
    }

    /// The Rex session of a block, from the last snapshot or a fresh one.
    async fn session_of_block(&self, block: &str) -> Result<String, RpcError> {
        if let Some(k) = self.blocks.lock().await.get(block) {
            return Ok(k.session.clone());
        }
        self.snapshot().await?;
        self.blocks
            .lock()
            .await
            .get(block)
            .map(|k| k.session.clone())
            .ok_or_else(|| not_running(&unnamed_session_id(block)))
    }

    async fn ensure_events(self: &Arc<Self>, peer: &Peer) {
        let mut started = self.events_started.lock().await;
        if !*started {
            *started = true;
            tokio::spawn(events(self.clone(), peer.clone()));
        }
    }
}

/// The node runs this plugin's own program as the terminal helper (`terminal.rs`).
fn terminal_command(
    rex: &Rex,
    session: &str,
    block: &str,
    mode: ext::TerminalMode,
    cols: u16,
    rows: u16,
) -> ext::TerminalCommand {
    let helper = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "fleetlayer-rex".into());
    let mode = match mode {
        ext::TerminalMode::Observe => "observe",
        ext::TerminalMode::Control => "control",
    };
    let mut args: Vec<String> = vec![
        "terminal".into(),
        mode.into(),
        session.into(),
        block.into(),
        cols.max(20).to_string(),
        rows.max(5).to_string(),
        rex.program.clone(),
    ];
    args.extend(rex.server.clone());
    ext::TerminalCommand { command: helper, args, ..Default::default() }
}

/// Wait until a new agent takes input: its status says it's at rest, or (for an agent that doesn't report a status)
/// a few seconds have passed since Claude Code registered it. Up to a minute.
async fn wait_ready(s: &Settings, session: &str, block: &str, harness: &str) -> Result<Option<String>, String> {
    let start = tokio::time::Instant::now();
    let mut registered_at = None;
    while start.elapsed() < Duration::from_secs(60) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let sid =
            if harness == "claude" { s.rex.claude_session_of(session, block, &s.claude_sessions).await } else { None };
        if sid.is_some() && registered_at.is_none() {
            registered_at = Some(tokio::time::Instant::now());
        }
        match s.rex.status_of(session, block).await {
            Ok(Status::Idle | Status::Done) => return Ok(sid),
            Ok(Status::Unknown) if registered_at.is_some_and(|t| t.elapsed() > Duration::from_secs(3)) => {
                return Ok(sid);
            }
            Ok(_) => {}
            Err(Failure::NotHere(_)) => return Err(format!("{harness} exited before it took input")),
            Err(Failure::Other(m)) => tracing::debug!("waiting for {harness}: {m}"),
        }
    }
    Err(format!("{harness} didn't take input within a minute"))
}

/// The handler is an `Arc<Plugin>` so the events task can share the plugin's state.
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
                    .ok_or_else(|| RpcError::refused("fleetlayer-rex runs under a Fleetlayer node"))?;
                let config: Config = serde_json::from_value(node.config.clone()).unwrap_or_default();
                let home = PathBuf::from(&node.home);
                let settings = Settings {
                    rex: Rex { program: config.program.unwrap_or_else(|| "rex".into()), server: config.server },
                    claude_sessions: config.claude_dir.unwrap_or_else(|| rex::claude_dir(&home)).join("sessions"),
                };
                *plugin.settings.lock().await = Some(Arc::new(settings));
                let role = SessionManagerRole {
                    open: true,
                    close: true,
                    interrupt: true,
                    deliver: true,
                    events: true,
                    terminal: true,
                    keys: false,
                    own_messages: vec![],
                };
                let meta = PluginMeta {
                    protocol: fleetlayer_protocol::PROTOCOL,
                    roles: Roles { session_manager: Some(role), ..Default::default() },
                };
                answer(&InitializeResponse {
                    protocol_version: fleetlayer_protocol::ACP_PROTOCOL,
                    agent_capabilities: Capabilities {
                        meta: Some(ext::meta_with(meta_key::PLUGIN, &meta)),
                        ..Default::default()
                    },
                    agent_info: Some(Implementation {
                        name: "fleetlayer-rex".into(),
                        title: Some("Rex".into()),
                        version: env!("CARGO_PKG_VERSION").into(),
                    }),
                    auth_methods: vec![],
                })
            }
            ext::method::PING => Ok(json!({})),
            ext::method::TERMINAL_COMMAND => {
                let r: ext::TerminalCommandRequest = params(p)?;
                let s = plugin.settings().await?;
                let (session, block) = plugin.locate(&r.session_id).await?;
                answer(&terminal_command(&s.rex, &session, &block, r.mode, r.cols, r.rows))
            }
            acp::method::SESSION_LIST => {
                let agents = plugin.snapshot().await?;
                plugin.ensure_events(peer).await;
                answer(&ListSessionsResponse { sessions: agents.iter().map(session_info).collect(), next_cursor: None })
            }
            acp::method::SESSION_CLOSE => {
                let r: SessionRef = params(p)?;
                let s = plugin.settings().await?;
                let (session, block) = plugin.locate(&r.session_id).await?;
                s.rex.close(&session, &block).await.map_err(failure)?;
                Ok(json!({}))
            }
            ext::method::SESSION_OPEN => {
                let open: OpenSession = params(p)?;
                let s = plugin.settings().await?;
                if !open.cwd.starts_with('/') {
                    return Err(RpcError::refused(format!("{} isn't an absolute directory", open.cwd)));
                }
                let (session, block) =
                    s.rex.new_session(&open.label, &open.cwd, &open.harness, &open.args).await.map_err(failure)?;
                let ready = wait_ready(&s, &session, &block, &open.harness).await;
                let session_id = ready.as_ref().ok().cloned().flatten();
                // The session is up either way: a prompt that doesn't go in is reported, not a failed open. With no
                // prompt, an agent that's slow to take input isn't an error either.
                let prompt_error = match (&open.prompt, ready) {
                    (None, _) => None,
                    (Some(_), Err(e)) => Some(e),
                    (Some(text), Ok(_)) => {
                        s.rex.type_message(&session, &block, text).await.err().map(|e| e.message().to_string())
                    }
                };
                // Known at once, so its status changes arrive as updates before the node lists it again.
                let known = Known {
                    session: session.clone(),
                    session_id: session_id.clone().unwrap_or_else(|| unnamed_session_id(&block)),
                    harness: open.harness.clone(),
                    status: Status::Unknown,
                };
                plugin.blocks.lock().await.insert(block.clone(), known);
                answer(&OpenedSession { address: block, session_id, prompt_error })
            }
            ext::method::SESSION_DELIVER => {
                let d: Deliver = params(p)?;
                let s = plugin.settings().await?;
                let session = plugin.session_of_block(&d.address).await?;
                match s.rex.status_of(&session, &d.address).await {
                    Ok(Status::Blocked) => {
                        return Err(RpcError::new(
                            code::BUSY,
                            "it's waiting on a person (a permission prompt or a question)",
                        ));
                    }
                    Err(Failure::NotHere(m)) => return Err(RpcError::not_here(m)),
                    _ => {}
                }
                s.rex.type_message(&session, &d.address, &d.text).await.map_err(failure)?;
                answer(&Delivered { via: Via::Typed })
            }
            _ => Err(RpcError::method_not_found(method)),
        }
    }

    async fn notification(&self, _peer: &Peer, method: &str, p: Value) {
        if method == acp::method::SESSION_CANCEL {
            let Ok(r) = serde_json::from_value::<SessionRef>(p) else { return };
            let plugin = &self.0;
            let (Ok(s), Ok((session, block))) = (plugin.settings().await, plugin.locate(&r.session_id).await) else {
                return;
            };
            if let Err(e) = s.rex.interrupt(&session, &block).await {
                tracing::warn!("interrupting {}: {e:?}", r.session_id);
            }
        }
    }
}

// ---------------------------------------------------------------- events

/// Follow Rex's events for as long as the plugin runs. When the server isn't running, try again with backoff.
async fn events(plugin: Arc<Plugin>, peer: Peer) {
    let mut backoff = Duration::from_secs(1);
    while !peer.is_closed() {
        let Ok(settings) = plugin.settings().await else { return };
        match settings.rex.events() {
            Ok(mut stream) => {
                // Whatever happened while unsubscribed: have the node read everything again.
                changed(&peer);
                let opened = tokio::time::Instant::now();
                while let Ok(Some(event)) = stream.next().await {
                    handle(&plugin, &settings, &peer, event).await;
                }
                if opened.elapsed() > Duration::from_secs(10) {
                    backoff = Duration::from_secs(1);
                }
                changed(&peer);
            }
            Err(e) => tracing::debug!("rex events: {}", e.message()),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

fn update(peer: &Peer, session_id: &str, meta: SessionMeta) {
    let n = SessionNotification {
        session_id: session_id.into(),
        update: SessionUpdate::Info(SessionInfoUpdate {
            session_update: SessionInfoTag::SessionInfoUpdate,
            title: None,
            updated_at: Some(Timestamp::now()),
            meta: Some(ext::meta_with(meta_key::SESSION, &meta)),
        }),
    };
    peer.notify(acp::method::SESSION_UPDATE, &n);
}

fn ended(peer: &Peer, session_id: &str, how: How) {
    peer.notify(ext::method::SESSION_ENDED, &SessionEnded { session_id: session_id.into(), at: Timestamp::now(), how });
}

fn changed(peer: &Peer) {
    peer.notify(ext::method::SESSIONS_CHANGED, &json!({}));
}

async fn handle(plugin: &Arc<Plugin>, s: &Settings, peer: &Peer, event: Event) {
    match event {
        Event::Status { session, block } => {
            let Some(known) = plugin.blocks.lock().await.get(&block).cloned() else {
                // A terminal not listed yet: an agent may have started reporting in it.
                changed(peer);
                return;
            };
            let Ok(status) = s.rex.status_of(&session, &block).await else { return };
            let mut sid = known.session_id.clone();
            // An unnamed Claude Code session got its ID.
            if sid.starts_with('@')
                && known.harness == "claude"
                && let Some(new) = s.rex.claude_session_of(&session, &block, &s.claude_sessions).await
            {
                peer.notify(ext::method::SESSION_RENAMED, &SessionRenamed { from: sid.clone(), to: new.clone() });
                sid = new;
            }
            if let Some(k) = plugin.blocks.lock().await.get_mut(&block) {
                k.session_id = sid.clone();
                k.status = status;
            }
            let named = Some(!sid.starts_with('@'));
            update(
                peer,
                &sid,
                SessionMeta {
                    status: Some(status),
                    status_changed_at: Some(Timestamp::now()),
                    named,
                    ..Default::default()
                },
            );
        }
        Event::Process { session, block } => {
            let known = plugin.blocks.lock().await.get(&block).cloned();
            match known {
                // The agent may have exited back to a shell: check what runs there now.
                Some(k) => {
                    let still = s.rex.snapshot(&s.claude_sessions).await.ok().is_some_and(|agents| {
                        agents.iter().any(|a| a.block == block && a.session == session && a.harness == k.harness)
                    });
                    if !still {
                        plugin.blocks.lock().await.remove(&block);
                        ended(
                            peer,
                            &k.session_id,
                            How::AgentExited { final_status: Some(k.status), terminal_closed: false },
                        );
                    }
                }
                None => changed(peer),
            }
        }
        Event::BlockClosed { block, .. } => {
            if let Some(k) = plugin.blocks.lock().await.remove(&block) {
                ended(peer, &k.session_id, How::TabClosed);
            }
        }
        Event::SessionDestroyed { session } => {
            let mut blocks = plugin.blocks.lock().await;
            let gone: Vec<String> =
                blocks.iter().filter(|(_, k)| k.session == session).map(|(b, _)| b.clone()).collect();
            for b in gone {
                if let Some(k) = blocks.remove(&b) {
                    ended(peer, &k.session_id, How::TabClosed);
                }
            }
        }
        Event::Changed => changed(peer),
        Event::Other => {}
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("FLEETLAYER_LOG"))
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("terminal") {
        let Some(a) = terminal::parse_args(&args[1..]) else {
            eprintln!(
                "usage: fleetlayer-rex terminal <observe|control> <session> <block> <cols> <rows> <rex> [<server>]"
            );
            std::process::exit(2);
        };
        if let Err(e) = terminal::run(a).await {
            eprintln!("fleetlayer-rex terminal: {e}");
            std::process::exit(1);
        }
        return;
    }
    fleetlayer_plugin::serve_stdio(Arc::new(Shared(Arc::new(Plugin::default())))).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminals_stream_through_the_plugins_own_helper() {
        let rex = Rex { program: "/usr/bin/rex".into(), server: Some("ssh://build".into()) };
        let c = terminal_command(&rex, "session:a", "block:b", ext::TerminalMode::Control, 100, 30);
        assert_eq!(c.args, ["terminal", "control", "session:a", "block:b", "100", "30", "/usr/bin/rex", "ssh://build"]);
        let c =
            terminal_command(&Rex { program: "rex".into(), server: None }, "s", "b", ext::TerminalMode::Observe, 3, 1);
        assert_eq!(c.args, ["terminal", "observe", "s", "b", "20", "5", "rex"], "a usable minimum size, no server");
    }

    #[test]
    fn an_unnamed_agent_is_addressed_by_its_block() {
        let a = Agent {
            session: "session:a".into(),
            session_label: Some("planning".into()),
            block: "block:b".into(),
            block_label: None,
            harness: "codex".into(),
            session_id: None,
            cwd: Some("/w".into()),
            status: Status::Idle,
            tab: Some("planning".into()),
            human_attached: Some(false),
        };
        let info = session_info(&a);
        assert_eq!(info.session_id, "@block:b");
        let meta: SessionMeta = ext::get_meta(info.meta.as_ref(), meta_key::SESSION).unwrap().unwrap();
        assert_eq!(meta.tab.as_deref(), Some("planning"));
        assert_eq!(meta.named, Some(false));
        assert_eq!(meta.opens_in.as_deref(), Some("Rex"));
    }
}
