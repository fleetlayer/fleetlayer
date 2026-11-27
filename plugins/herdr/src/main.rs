//! fleetlayer-herdr: the Fleetlayer adapter for herdr. It takes the session-manager role (`docs/protocol.md` section
//! 4): lists herdr's agents as sessions, streams their status from herdr's event socket, opens and closes tabs,
//! interrupts and types messages in. It never takes a session over: the person keeps using herdr as before.
//!
//! Config (its section of the node's config, all optional):
//! - `program`: the herdr binary (default `herdr`).
//! - `machine`: the machine name herdr's `herdr://open` links use (default: the node's machine).

mod herdr;

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
    SessionManagerRole, SessionMeta, SessionRenamed, Via, meta_key, unnamed_session_id,
};
use fleetlayer_protocol::rpc::RpcError;
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::herdr::{Event, Failure, SubscribeError};

#[derive(Debug, Default, Deserialize)]
struct Config {
    program: Option<String>,
    machine: Option<String>,
}

struct Settings {
    program: String,
    machine: String,
}

#[derive(Default)]
struct Plugin {
    settings: Mutex<Option<Arc<Settings>>>,
    /// The session ID the node knows each pane's session by (`@<pane>` while unnamed), from the last snapshot or event.
    panes: Mutex<HashMap<String, String>>,
    events_started: Mutex<bool>,
}

fn session_id(s: &herdr::Session) -> String {
    s.session_id.clone().unwrap_or_else(|| unnamed_session_id(&s.pane))
}

fn session_info(s: &herdr::Session, machine: &str) -> SessionInfo {
    let meta = SessionMeta {
        harness: Some(s.harness.clone()),
        address: Some(s.pane.clone()),
        named: Some(s.session_id.is_some()),
        workspace: s.workspace.clone(),
        tab: s.tab.clone(),
        status: Some(s.status),
        status_changed_at: Some(s.status_changed_at),
        running: Some(true),
        open_url: s.tab.as_deref().map(|tab| herdr::open_url(machine, tab)),
        opens_in: Some("herdr".into()),
        ..Default::default()
    };
    SessionInfo {
        session_id: session_id(s),
        cwd: s.cwd.clone(),
        title: s.tab.clone(),
        updated_at: Some(s.status_changed_at),
        meta: Some(ext::meta_with(meta_key::SESSION, &meta)),
    }
}

impl Plugin {
    async fn settings(&self) -> Result<Arc<Settings>, RpcError> {
        self.settings.lock().await.clone().ok_or_else(|| RpcError::refused("initialize first"))
    }

    async fn snapshot(&self) -> Result<Vec<herdr::Session>, RpcError> {
        let s = self.settings().await?;
        let sessions = herdr::snapshot(&s.program).await.map_err(|e| RpcError::refused(e.to_string()))?;
        let mut panes = self.panes.lock().await;
        panes.clear();
        for x in &sessions {
            panes.insert(x.pane.clone(), session_id(x));
        }
        Ok(sessions)
    }

    /// The pane a session ID names: `@<pane>`, or a named session's pane from the last snapshot (taking a fresh one
    /// when it isn't there).
    async fn pane_of(&self, session_id: &str) -> Result<String, RpcError> {
        if let Some(pane) = session_id.strip_prefix('@') {
            return Ok(pane.to_string());
        }
        let find =
            |panes: &HashMap<String, String>| panes.iter().find(|(_, s)| *s == session_id).map(|(p, _)| p.clone());
        if let Some(p) = find(&*self.panes.lock().await) {
            return Ok(p);
        }
        self.snapshot().await?;
        find(&*self.panes.lock().await).ok_or_else(|| not_running(session_id))
    }

    async fn ensure_events(self: &Arc<Self>, peer: &Peer) {
        let mut started = self.events_started.lock().await;
        if !*started {
            *started = true;
            tokio::spawn(events(self.clone(), peer.clone()));
        }
    }
}

/// herdr's own terminal stream: `herdr terminal session observe|control <pane>` speaks the terminal stream format.
fn terminal_command(program: &str, pane: &str, mode: ext::TerminalMode, cols: u16, rows: u16) -> ext::TerminalCommand {
    let mode = match mode {
        ext::TerminalMode::Observe => "observe",
        ext::TerminalMode::Control => "control",
    };
    ext::TerminalCommand {
        command: program.into(),
        args: vec![
            "terminal".into(),
            "session".into(),
            mode.into(),
            pane.into(),
            "--cols".into(),
            cols.max(20).to_string(),
            "--rows".into(),
            rows.max(5).to_string(),
        ],
        ..Default::default()
    }
}

fn not_running(session_id: &str) -> RpcError {
    RpcError::not_here(format!("session {session_id} isn't running in herdr on this machine"))
}

fn failure(e: Failure) -> RpcError {
    match e {
        Failure::NotHere(m) => RpcError::not_here(m),
        Failure::Busy(m) => RpcError::new(fleetlayer_protocol::rpc::code::BUSY, m),
        Failure::Other(m) => RpcError::refused(m),
    }
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
                    .ok_or_else(|| RpcError::refused("fleetlayer-herdr runs under a Fleetlayer node"))?;
                let config: Config = serde_json::from_value(node.config.clone()).unwrap_or_default();
                let settings = Settings {
                    program: config.program.unwrap_or_else(|| "herdr".into()),
                    machine: config.machine.unwrap_or_else(|| node.machine.clone()),
                };
                *plugin.settings.lock().await = Some(Arc::new(settings));
                let role = SessionManagerRole {
                    open: true,
                    close: true,
                    interrupt: true,
                    deliver: true,
                    events: true,
                    terminal: true,
                    own_messages: herdr::auto_continue_message(&PathBuf::from(&node.home)).into_iter().collect(),
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
                        name: "fleetlayer-herdr".into(),
                        title: Some("herdr".into()),
                        version: env!("CARGO_PKG_VERSION").into(),
                    }),
                    auth_methods: vec![],
                })
            }
            ext::method::PING => Ok(json!({})),
            ext::method::TERMINAL_COMMAND => {
                let r: ext::TerminalCommandRequest = params(p)?;
                let s = plugin.settings().await?;
                let pane = plugin.pane_of(&r.session_id).await?;
                answer(&terminal_command(&s.program, &pane, r.mode, r.cols, r.rows))
            }
            acp::method::SESSION_LIST => {
                let sessions = plugin.snapshot().await?;
                let machine = plugin.settings().await?.machine.clone();
                plugin.ensure_events(peer).await;
                answer(&ListSessionsResponse {
                    sessions: sessions.iter().map(|s| session_info(s, &machine)).collect(),
                    next_cursor: None,
                })
            }
            acp::method::SESSION_CLOSE => {
                let r: SessionRef = params(p)?;
                let s = plugin.settings().await?;
                let pane = plugin.pane_of(&r.session_id).await?;
                let sessions = herdr::snapshot(&s.program).await.map_err(|e| RpcError::refused(e.to_string()))?;
                let tab = sessions
                    .iter()
                    .find(|x| x.pane == pane)
                    .and_then(|x| x.tab_id.clone())
                    .ok_or_else(|| not_running(&r.session_id))?;
                herdr::close_tab(&s.program, &tab).await.map_err(failure)?;
                Ok(json!({}))
            }
            ext::method::SESSION_OPEN => {
                let open: OpenSession = params(p)?;
                let s = plugin.settings().await?;
                if !open.cwd.starts_with('/') {
                    return Err(RpcError::refused(format!("{} isn't an absolute directory", open.cwd)));
                }
                let (pane, session_id) = herdr::create_tab(&s.program, &open).await.map_err(RpcError::refused)?;
                // The session is up either way: a prompt that doesn't go in is reported, not a failed open.
                let mut prompt_error = None;
                if let Some(text) = &open.prompt
                    && let Err(e) = herdr::prompt(&s.program, &pane, text).await
                {
                    prompt_error = Some(failure(e).message);
                }
                answer(&OpenedSession { address: pane, session_id, prompt_error })
            }
            ext::method::SESSION_DELIVER => {
                let d: Deliver = params(p)?;
                let s = plugin.settings().await?;
                herdr::prompt(&s.program, &d.address, &d.text).await.map_err(failure)?;
                answer(&Delivered { via: Via::Typed })
            }
            _ => Err(RpcError::method_not_found(method)),
        }
    }

    async fn notification(&self, _peer: &Peer, method: &str, p: Value) {
        if method == acp::method::SESSION_CANCEL {
            let Ok(r) = serde_json::from_value::<SessionRef>(p) else { return };
            let plugin = &self.0;
            let (Ok(s), Ok(pane)) = (plugin.settings().await, plugin.pane_of(&r.session_id).await) else { return };
            if let Err(e) = herdr::interrupt(&s.program, &pane).await {
                tracing::warn!("interrupting {}: {e:?}", r.session_id);
            }
        }
    }
}

// ---------------------------------------------------------------- events

/// Follow herdr's events for as long as the plugin runs, turning them into `session/update` and the session
/// notifications. When herdr isn't running, try again with backoff; when it refuses to stream (an older herdr), poll
/// `agent list` every few seconds and say when it changed.
async fn events(plugin: Arc<Plugin>, peer: Peer) {
    let mut backoff = Duration::from_secs(1);
    while !peer.is_closed() {
        let Ok(settings) = plugin.settings().await else { return };
        match herdr::Subscription::open(&settings.program).await {
            Ok(mut sub) => {
                backoff = Duration::from_secs(1);
                // Whatever happened while unsubscribed: have the node read everything again.
                peer.notify(ext::method::SESSIONS_CHANGED, &json!({}));
                while let Ok(Some(event)) = sub.next().await {
                    handle(&plugin, &peer, event).await;
                }
                peer.notify(ext::method::SESSIONS_CHANGED, &json!({}));
            }
            Err(SubscribeError::Refused(why)) => {
                tracing::warn!("herdr won't stream events ({why}); polling instead");
                poll(&plugin, &peer).await;
            }
            Err(SubscribeError::Unreachable(why)) => {
                tracing::debug!("herdr isn't reachable: {why}");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Poll until the snapshot fails (herdr went away), saying when it changed.
async fn poll(plugin: &Arc<Plugin>, peer: &Peer) {
    let mut last = None;
    while !peer.is_closed() {
        let Ok(now) = plugin.snapshot().await else { return };
        if last.as_ref() != Some(&now) {
            if last.is_some() {
                peer.notify(ext::method::SESSIONS_CHANGED, &json!({}));
            }
            last = Some(now);
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

fn update(peer: &Peer, session_id: &str, meta: SessionMeta, title: Option<String>) {
    let n = SessionNotification {
        session_id: session_id.into(),
        update: SessionUpdate::Info(SessionInfoUpdate {
            session_update: SessionInfoTag::SessionInfoUpdate,
            title,
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

async fn handle(plugin: &Arc<Plugin>, peer: &Peer, event: Event) {
    match event {
        Event::Status { pane, status } => {
            let known = plugin.panes.lock().await.get(&pane).cloned();
            match known {
                Some(sid) => update(
                    peer,
                    &sid,
                    SessionMeta {
                        status: Some(status),
                        status_changed_at: Some(Timestamp::now()),
                        ..Default::default()
                    },
                    None,
                ),
                None => changed(peer),
            }
        }
        Event::TabClosed { sessions } => {
            let mut panes = plugin.panes.lock().await;
            for sid in sessions {
                panes.retain(|_, s| *s != sid);
                ended(peer, &sid, How::TabClosed);
            }
        }
        Event::AgentExited { pane, session, how } => {
            let mut panes = plugin.panes.lock().await;
            let sid = session.or_else(|| pane.as_ref().and_then(|p| panes.get(p).cloned()));
            if let Some(p) = &pane {
                panes.remove(p);
            }
            match sid {
                Some(sid) => ended(peer, &sid, how),
                None => changed(peer),
            }
        }
        Event::Updated { pane, session, tab, cwd } => {
            let mut panes = plugin.panes.lock().await;
            let known = panes.get(&pane).cloned();
            let sid = match (known, session) {
                // An unnamed session got its harness's ID.
                (Some(old), Some(new)) if old != new => {
                    peer.notify(ext::method::SESSION_RENAMED, &SessionRenamed { from: old, to: new.clone() });
                    panes.insert(pane.clone(), new.clone());
                    new
                }
                (Some(old), _) => old,
                (None, _) => {
                    drop(panes);
                    changed(peer);
                    return;
                }
            };
            drop(panes);
            let meta = SessionMeta { tab: tab.clone(), named: Some(!sid.starts_with('@')), ..Default::default() };
            update(peer, &sid, meta, tab);
            if cwd.is_some() {
                // ACP carries cwd only on list entries, so a moved session is re-read.
                changed(peer);
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
    fleetlayer_plugin::serve_stdio(Arc::new(Shared(Arc::new(Plugin::default())))).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminals_stream_through_herdrs_own_terminal_session_command() {
        let c = terminal_command("/usr/local/bin/herdr", "w1:p4", ext::TerminalMode::Control, 100, 30);
        assert_eq!(c.command, "/usr/local/bin/herdr");
        assert_eq!(c.args, ["terminal", "session", "control", "w1:p4", "--cols", "100", "--rows", "30"]);
        let c = terminal_command("herdr", "w1:p1", ext::TerminalMode::Observe, 3, 1);
        assert_eq!(c.args[2], "observe");
        assert_eq!((c.args[5].as_str(), c.args[7].as_str()), ("20", "5"), "a usable minimum size");
    }
}
