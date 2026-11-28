//! The session-manager role over tmux (`docs/protocol.md` section 4).

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
    self, Deliver, Delivered, How, NodeMeta, OpenSession, OpenedSession, PluginMeta, PluginStatusReport, Roles,
    SessionEnded, SessionManagerRole, SessionMeta, SessionRenamed, Status, Via, meta_key, unnamed_session_id,
};
use fleetlayer_protocol::rpc::{RpcError, code};
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::terminal;
use crate::tmux::{self, Agent, Failure, Socket, Tmux};

/// How often the plugin reads tmux and the agents' state. tmux has no event stream that doesn't attach a client.
const POLL: Duration = Duration::from_secs(1);

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// The tmux binary (default `tmux`).
    pub program: Option<String>,
    /// A named server (`tmux -L <socket>`).
    pub socket: Option<String>,
    /// A server's socket path (`tmux -S <path>`).
    pub socket_path: Option<String>,
    /// The tmux session new agents open in (default `fleetlayer`), made when it isn't there.
    pub session: Option<String>,
    /// The login shell new agents start through (default `$SHELL`, else `/bin/sh`).
    pub shell: Option<String>,
    /// Claude Code's directory (default `CLAUDE_CONFIG_DIR` or `~/.claude`).
    pub claude_dir: Option<PathBuf>,
}

pub struct Settings {
    pub tmux: Tmux,
    pub session: String,
    pub shell: String,
    pub claude_sessions: PathBuf,
}

impl Settings {
    pub fn from_config(config: Config, home: &std::path::Path, user: &str) -> Settings {
        let socket = match (config.socket_path, config.socket) {
            (Some(p), _) => Some(Socket::Path(p)),
            (None, Some(n)) => Some(Socket::Name(n)),
            _ => None,
        };
        Settings {
            tmux: Tmux { program: config.program.unwrap_or_else(|| "tmux".into()), socket },
            session: config.session.unwrap_or_else(|| "fleetlayer".into()),
            shell: config.shell.or_else(|| login_shell(user)).unwrap_or_else(|| "/bin/sh".into()),
            claude_sessions: config.claude_dir.unwrap_or_else(|| tmux::claude_dir(home)).join("sessions"),
        }
    }
}

/// The person's login shell: `$SHELL`, else their account's (a service manager often leaves `SHELL` unset, and
/// `/bin/sh -l` would skip a zsh user's own profile, and with it their PATH).
fn login_shell(user: &str) -> Option<String> {
    let abs = |s: &str| Some(s.trim().to_string()).filter(|s| s.starts_with('/'));
    if let Some(s) = std::env::var("SHELL").ok().and_then(|s| abs(&s)) {
        return Some(s);
    }
    if cfg!(target_os = "macos") {
        let out = std::process::Command::new("dscl")
            .args([".", "-read", &format!("/Users/{user}"), "UserShell"])
            .output()
            .ok()?;
        return String::from_utf8_lossy(&out.stdout).split_whitespace().nth(1).and_then(abs);
    }
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd_shell(&passwd, user)
}

pub fn passwd_shell(passwd: &str, user: &str) -> Option<String> {
    passwd.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() >= 7 && f[0] == user && f[6].starts_with('/')).then(|| f[6].trim().to_string())
    })
}

/// What the plugin last knew about a pane that runs an agent.
#[derive(Debug, Clone, PartialEq)]
pub struct Known {
    pub session_id: String,
    pub harness: String,
    /// As reported, `done` included.
    pub status: Status,
    /// As read, before `done`.
    pub raw: Status,
    pub status_changed_at: Timestamp,
    pub tab: String,
    pub cwd: Option<String>,
    pub attached: bool,
}

/// What changed between two looks at tmux, as the notifications that say so.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Ended {
        session_id: String,
        how: How,
    },
    Renamed {
        from: String,
        to: String,
    },
    Status {
        session_id: String,
        status: Status,
        at: Timestamp,
        named: bool,
    },
    /// Something the node should read the list again for.
    Listed,
}

fn session_id(a: &Agent) -> String {
    a.session_id.clone().unwrap_or_else(|| unnamed_session_id(&tmux::address(&a.pane.id)))
}

/// The new state of every agent pane, and what changed since `old`. `panes` is every pane that still exists.
pub fn diff(
    old: &HashMap<String, Known>,
    agents: &[Agent],
    panes: &[String],
    now: Timestamp,
) -> (HashMap<String, Known>, Vec<Change>) {
    let mut new = HashMap::new();
    let mut changes = vec![];
    for a in agents {
        let sid = session_id(a);
        let prev = old.get(&a.pane.id);
        // A turn that finished while nobody could see the pane is `done` until someone looks. From blocked too: a
        // prompt answered from elsewhere, or a working spell shorter than one look, ends the turn all the same.
        let status = match prev {
            Some(p)
                if a.status == Status::Idle
                    && !a.pane.visible()
                    && matches!(p.raw, Status::Working | Status::Blocked) =>
            {
                Status::Done
            }
            Some(p) if a.status == Status::Idle && !a.pane.visible() && p.status == Status::Done => Status::Done,
            _ => a.status,
        };
        let known = Known {
            session_id: sid.clone(),
            harness: a.harness.clone(),
            status,
            raw: a.status,
            status_changed_at: match prev {
                Some(p) if p.status == status => p.status_changed_at,
                _ => now,
            },
            tab: a.tab.clone(),
            cwd: a.pane.cwd.clone(),
            attached: a.pane.attached > 0,
        };
        match prev {
            None => changes.push(Change::Listed),
            Some(p) => {
                if p.session_id != sid {
                    if p.session_id.starts_with('@') && p.harness == a.harness {
                        changes.push(Change::Renamed { from: p.session_id.clone(), to: sid.clone() });
                    } else {
                        // Another agent in the same pane.
                        changes.push(Change::Ended {
                            session_id: p.session_id.clone(),
                            how: How::AgentExited { final_status: Some(p.status), terminal_closed: false },
                        });
                        changes.push(Change::Listed);
                    }
                }
                if p.status != status {
                    changes.push(Change::Status {
                        session_id: sid.clone(),
                        status,
                        at: now,
                        named: !sid.starts_with('@'),
                    });
                }
                if p.tab != known.tab || p.cwd != known.cwd || p.attached != known.attached {
                    changes.push(Change::Listed);
                }
            }
        }
        new.insert(a.pane.id.clone(), known);
    }
    for (pane, p) in old {
        if new.contains_key(pane) {
            continue; // The same agent, renamed, or another one reported above.
        }
        let how = if panes.contains(pane) {
            How::AgentExited { final_status: Some(p.status), terminal_closed: false }
        } else {
            How::TabClosed
        };
        changes.push(Change::Ended { session_id: p.session_id.clone(), how });
    }
    if changes.iter().filter(|c| **c == Change::Listed).count() > 1 {
        let mut seen = false;
        changes.retain(|c| *c != Change::Listed || !std::mem::replace(&mut seen, true));
    }
    (new, changes)
}

pub fn session_info(a: &Agent, known: Option<&Known>) -> SessionInfo {
    let status = known.map_or(a.status, |k| k.status);
    let changed = known.map_or_else(Timestamp::now, |k| k.status_changed_at);
    let meta = SessionMeta {
        harness: Some(a.harness.clone()),
        address: Some(tmux::address(&a.pane.id)),
        named: Some(a.session_id.is_some()),
        workspace: Some(a.pane.session.clone()),
        tab: Some(a.tab.clone()),
        status: Some(status),
        status_changed_at: Some(changed),
        running: Some(true),
        human_attached: Some(a.pane.attached > 0),
        opens_in: Some("tmux".into()),
        ..Default::default()
    };
    SessionInfo {
        session_id: session_id(a),
        cwd: a.pane.cwd.clone(),
        title: Some(a.tab.clone()),
        updated_at: Some(changed),
        meta: Some(ext::meta_with(meta_key::SESSION, &meta)),
    }
}

fn failure(e: Failure) -> RpcError {
    match e {
        Failure::NotHere(m) => RpcError::not_here(m),
        Failure::NoServer => RpcError::not_here("no tmux server is running"),
        Failure::Other(m) => RpcError::refused(m),
    }
}

fn not_running(session_id: &str) -> RpcError {
    RpcError::not_here(format!("session {session_id} isn't running in tmux on this machine"))
}

fn busy(m: impl Into<String>) -> RpcError {
    RpcError::new(code::BUSY, m.into())
}

#[derive(Default)]
pub struct Plugin {
    settings: Mutex<Option<Arc<Settings>>>,
    /// Each agent pane (`%12`) → what the plugin last knew.
    known: Mutex<HashMap<String, Known>>,
    events_started: Mutex<bool>,
}

impl Plugin {
    async fn settings(&self) -> Result<Arc<Settings>, RpcError> {
        self.settings.lock().await.clone().ok_or_else(|| RpcError::refused("initialize first"))
    }

    /// Read tmux, keep what it says, and send what changed. From `session/list`, only what the list can't say
    /// (sessions that ended, unnamed ones that got their ID) is sent: the list itself is the rest.
    async fn refresh(&self, peer: &Peer, from_list: bool) -> Result<Vec<(Agent, Known)>, Failure> {
        let s = self.settings().await.map_err(|e| Failure::Other(e.message))?;
        let (agents, panes) = s.tmux.snapshot(&s.claude_sessions).await?;
        let mut known = self.known.lock().await;
        let (new, changes) = diff(&known, &agents, &panes, Timestamp::now());
        *known = new;
        let out = agents.into_iter().filter_map(|a| known.get(&a.pane.id).cloned().map(|k| (a, k))).collect();
        drop(known);
        for c in changes {
            send(peer, c, from_list);
        }
        Ok(out)
    }

    /// The pane (`%12`) a session ID names: `@p12`, or a named session's pane from the last look (taking a fresh
    /// one when it isn't there).
    /// Only panes that run an agent: an `@p<N>` someone guessed never reaches a plain shell.
    async fn pane_of(&self, peer: &Peer, session_id: &str) -> Result<String, RpcError> {
        let find =
            |k: &HashMap<String, Known>| k.iter().find(|(_, v)| v.session_id == session_id).map(|(p, _)| p.clone());
        if let Some(p) = find(&*self.known.lock().await) {
            return Ok(p);
        }
        self.refresh(peer, true).await.map_err(failure)?;
        find(&*self.known.lock().await).ok_or_else(|| not_running(session_id))
    }

    async fn ensure_events(self: &Arc<Self>, peer: &Peer) {
        let mut started = self.events_started.lock().await;
        if !*started {
            *started = true;
            tokio::spawn(events(self.clone(), peer.clone()));
        }
    }

    /// Whether the pane can take typing now: it still runs an agent, nobody is scrolling in it, and the agent isn't
    /// waiting on a person.
    async fn ready_for_typing(&self, s: &Settings, pane: &str, refuse_blocked: bool) -> Result<(), RpcError> {
        let p = s.tmux.pane(pane).await.map_err(failure)?;
        if p.dead {
            return Err(RpcError::not_here(format!("the program in tmux pane {pane} has exited")));
        }
        let procs = tmux::processes().await;
        if tmux::find_agent(p.pid, &procs).is_none() {
            // A shell now, where typed text would run as commands.
            return Err(RpcError::not_here(format!("no agent runs in tmux pane {pane} any more")));
        }
        if p.in_mode {
            return Err(busy("someone is scrolling in its tmux pane (copy mode), so nothing was typed"));
        }
        if refuse_blocked && self.known.lock().await.get(pane).is_some_and(|k| k.status == Status::Blocked) {
            return Err(busy("it's waiting on a person (a permission prompt or a question)"));
        }
        Ok(())
    }
}

fn send(peer: &Peer, c: Change, from_list: bool) {
    match c {
        Change::Ended { session_id, how } => {
            peer.notify(ext::method::SESSION_ENDED, &SessionEnded { session_id, at: Timestamp::now(), how });
        }
        Change::Renamed { from, to } => {
            peer.notify(ext::method::SESSION_RENAMED, &SessionRenamed { from, to });
        }
        Change::Status { .. } | Change::Listed if from_list => {}
        Change::Status { session_id, status, at, named } => {
            let meta = SessionMeta {
                status: Some(status),
                status_changed_at: Some(at),
                named: Some(named),
                ..Default::default()
            };
            let n = SessionNotification {
                session_id,
                update: SessionUpdate::Info(SessionInfoUpdate {
                    session_update: SessionInfoTag::SessionInfoUpdate,
                    title: None,
                    updated_at: Some(at),
                    meta: Some(ext::meta_with(meta_key::SESSION, &meta)),
                }),
            };
            peer.notify(acp::method::SESSION_UPDATE, &n);
        }
        Change::Listed => {
            peer.notify(ext::method::SESSIONS_CHANGED, &json!({}));
        }
    }
}

/// Wait until a new agent takes input: Claude Code's registry says it's idle, or (for an agent with no registry
/// state) its screen reads idle, or a few seconds pass with nothing to say otherwise. Up to a minute. Answers the
/// session's ID when it's known by then.
async fn wait_ready(s: &Settings, pane: &str, harness: &str) -> Result<Option<String>, String> {
    let start = tokio::time::Instant::now();
    let mut seen: Option<tokio::time::Instant> = None;
    while start.elapsed() < Duration::from_secs(60) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let p = match s.tmux.pane(pane).await {
            Ok(p) if !p.dead => p,
            Ok(_) | Err(Failure::NotHere(_) | Failure::NoServer) => {
                return Err(format!("{harness} exited before it took input"));
            }
            Err(Failure::Other(m)) => {
                tracing::debug!("waiting for {harness}: {m}");
                continue;
            }
        };
        let procs = tmux::processes().await;
        let Some(agent) = tmux::find_agent(p.pid, &procs) else { continue };
        let since = *seen.get_or_insert_with(tokio::time::Instant::now);
        let settled = since.elapsed() > Duration::from_secs(3);
        let screen = s.tmux.screen(pane, 30).await.unwrap_or_default();
        if harness == "claude" {
            let regs = tmux::claude_registrations(&s.claude_sessions).await;
            if let Some(r) = regs.iter().find(|r| r.pid == agent.pid) {
                match r.status.as_deref().map(tmux::registry_status) {
                    Some(Status::Idle) => return Ok(Some(r.session_id.clone())),
                    Some(_) => continue,
                    None if settled && tmux::screen_status(harness, &screen) == Status::Idle => {
                        return Ok(Some(r.session_id.clone()));
                    }
                    None => continue,
                }
            }
        } else {
            let st = tmux::screen_status(harness, &screen);
            if st == Status::Idle || (st == Status::Unknown && settled && !screen.trim().is_empty()) {
                return Ok(if harness == "codex" { tmux::codex_session(agent.pid) } else { None });
            }
        }
    }
    Err(format!("{harness} didn't take input within a minute"))
}

/// The handler is an `Arc<Plugin>` so the events task can share the plugin's state.
pub struct Shared(pub Arc<Plugin>);

#[async_trait]
impl Handler for Shared {
    async fn request(&self, peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
        let plugin = &self.0;
        match method {
            acp::method::INITIALIZE => {
                let req: InitializeRequest = params(p)?;
                let node: NodeMeta = ext::get_meta(req.client_capabilities.meta.as_ref(), meta_key::NODE)
                    .map_err(RpcError::invalid_params)?
                    .ok_or_else(|| RpcError::refused("fleetlayer-tmux runs under a Fleetlayer node"))?;
                let config: Config = serde_json::from_value(node.config.clone()).map_err(RpcError::invalid_params)?;
                let settings = Settings::from_config(config, &PathBuf::from(&node.home), &node.user);
                *plugin.settings.lock().await = Some(Arc::new(settings));
                let role = SessionManagerRole {
                    open: true,
                    close: true,
                    interrupt: true,
                    deliver: true,
                    events: true,
                    terminal: true,
                    keys: true,
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
                        name: "fleetlayer-tmux".into(),
                        title: Some("tmux".into()),
                        version: env!("CARGO_PKG_VERSION").into(),
                    }),
                    auth_methods: vec![],
                })
            }
            ext::method::PING => Ok(json!({})),
            acp::method::SESSION_LIST => {
                let agents = plugin.refresh(peer, true).await.map_err(failure)?;
                plugin.ensure_events(peer).await;
                answer(&ListSessionsResponse {
                    sessions: agents.iter().map(|(a, k)| session_info(a, Some(k))).collect(),
                    next_cursor: None,
                })
            }
            ext::method::TERMINAL_COMMAND => {
                let r: ext::TerminalCommandRequest = params(p)?;
                let s = plugin.settings().await?;
                let pane = plugin.pane_of(peer, &r.session_id).await?;
                answer(&terminal::command(&s.tmux, &pane, r.mode))
            }
            acp::method::SESSION_CLOSE => {
                let r: SessionRef = params(p)?;
                let s = plugin.settings().await?;
                let pane = plugin.pane_of(peer, &r.session_id).await?;
                s.tmux.kill_pane(&pane).await.map_err(failure)?;
                Ok(json!({}))
            }
            ext::method::SESSION_OPEN => {
                let open: OpenSession = params(p)?;
                let s = plugin.settings().await?;
                if !open.cwd.starts_with('/') {
                    return Err(RpcError::refused(format!("{} isn't an absolute directory", open.cwd)));
                }
                let command = tmux::window_command(&s.shell, &open.harness, &open.args);
                let pane = s.tmux.open(&s.session, &open.label, &open.cwd, &command).await.map_err(failure)?;
                let ready = wait_ready(&s, &pane, &open.harness).await;
                let session_id = ready.as_ref().ok().cloned().flatten();
                // The session is up either way: a prompt that doesn't go in is reported, not a failed open.
                let prompt_error = match (&open.prompt, ready) {
                    (None, _) => None,
                    (Some(_), Err(e)) => Some(e),
                    (Some(text), Ok(_)) => s.tmux.type_message(&pane, text).await.err().map(|e| e.message()),
                };
                answer(&OpenedSession { address: tmux::address(&pane), session_id, prompt_error })
            }
            ext::method::SESSION_DELIVER => {
                let d: Deliver = params(p)?;
                let s = plugin.settings().await?;
                let pane = tmux::pane_id(&d.address).ok_or_else(|| not_running(&d.session_id))?;
                plugin.ready_for_typing(&s, &pane, true).await?;
                s.tmux.type_message(&pane, &d.text).await.map_err(failure)?;
                answer(&Delivered { via: Via::Typed })
            }
            ext::method::SESSION_KEYS => {
                let k: ext::SessionKeys = params(p)?;
                let s = plugin.settings().await?;
                let pane = plugin.pane_of(peer, &k.session_id).await?;
                // Keys answer prompts, so a blocked agent is exactly when they're wanted.
                plugin.ready_for_typing(&s, &pane, false).await?;
                s.tmux.keys(&pane, &k.steps).await.map_err(failure)?;
                Ok(json!({}))
            }
            ext::method::SESSION_SCREEN => {
                let id = p["sessionId"].as_str().ok_or_else(|| RpcError::invalid_params("sessionId"))?.to_string();
                let lines = p["lines"].as_u64().unwrap_or(60).clamp(1, 400);
                let s = plugin.settings().await?;
                let pane = plugin.pane_of(peer, &id).await?;
                let text = s.tmux.screen(&pane, usize::try_from(lines).unwrap_or(60)).await.map_err(failure)?;
                Ok(json!({"text": text}))
            }
            _ => Err(RpcError::method_not_found(method)),
        }
    }

    async fn notification(&self, peer: &Peer, method: &str, p: Value) {
        if method == acp::method::SESSION_CANCEL {
            let Ok(r) = serde_json::from_value::<SessionRef>(p) else { return };
            let plugin = &self.0;
            let (Ok(s), Ok(pane)) = (plugin.settings().await, plugin.pane_of(peer, &r.session_id).await) else {
                return;
            };
            if let Err(e) = s.tmux.interrupt(&pane).await {
                tracing::warn!("interrupting {}: {}", r.session_id, e.message());
            }
        }
    }
}

/// Read tmux every second for as long as the plugin runs, sending what changed, and say at least once a minute
/// whether it can (`_fleetlayer.dev/plugin/status`): a tmux that can't be run is a problem, no server running isn't.
async fn events(plugin: Arc<Plugin>, peer: Peer) {
    let mut last_report: Option<(Option<String>, tokio::time::Instant)> = None;
    let mut last_ok: Option<Timestamp> = None;
    while !peer.is_closed() {
        let (problem, sessions) = match plugin.refresh(&peer, false).await {
            Ok(agents) => {
                last_ok = Some(Timestamp::now());
                (None, agents.len())
            }
            Err(e) => (Some(format!("can't read tmux: {}", e.message())), 0),
        };
        let due = match &last_report {
            None => true,
            Some((p, at)) => *p != problem || at.elapsed() > Duration::from_secs(50),
        };
        if due {
            let detail = json!({"sessions": sessions, "lastReadAt": last_ok.map(|t| t.to_string())});
            peer.notify(ext::method::PLUGIN_STATUS, &PluginStatusReport { problem: problem.clone(), detail });
            last_report = Some((problem, tokio::time::Instant::now()));
        }
        tokio::time::sleep(POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::Pane;

    fn agent(pane: &str, sid: Option<&str>, status: Status, visible: bool) -> Agent {
        Agent {
            pane: Pane {
                id: pane.into(),
                attached: u32::from(visible),
                window_active: true,
                session: "work".into(),
                ..Default::default()
            },
            pid: 1,
            harness: "claude".into(),
            session_id: sid.map(str::to_string),
            status,
            claude_name: None,
            tab: "work".into(),
        }
    }

    fn t(s: i64) -> Timestamp {
        Timestamp::from_second(1_800_000_000 + s).unwrap()
    }

    #[test]
    fn a_new_agent_is_listed_then_its_changes_are_updates() {
        let (k, c) = diff(&HashMap::new(), &[agent("%1", None, Status::Working, true)], &["%1".into()], t(0));
        assert_eq!(c, [Change::Listed]);
        assert_eq!(k["%1"].session_id, "@p1");
        let (k, c) = diff(&k, &[agent("%1", Some("s1"), Status::Working, true)], &["%1".into()], t(1));
        assert_eq!(c, [Change::Renamed { from: "@p1".into(), to: "s1".into() }]);
        let (_, c) = diff(&k, &[agent("%1", Some("s1"), Status::Idle, true)], &["%1".into()], t(2));
        assert_eq!(c, [Change::Status { session_id: "s1".into(), status: Status::Idle, at: t(2), named: true }]);
    }

    #[test]
    fn a_turn_finished_out_of_sight_is_done_until_someone_looks() {
        let (k, _) = diff(&HashMap::new(), &[agent("%1", Some("s"), Status::Working, false)], &[], t(0));
        let (k, c) = diff(&k, &[agent("%1", Some("s"), Status::Idle, false)], &[], t(1));
        assert_eq!(k["%1"].status, Status::Done);
        assert!(matches!(c[0], Change::Status { status: Status::Done, .. }));
        let (k, c) = diff(&k, &[agent("%1", Some("s"), Status::Idle, false)], &[], t(2));
        assert_eq!((k["%1"].status, c.len()), (Status::Done, 0), "still done, nothing to say");
        assert_eq!(k["%1"].status_changed_at, t(1));
        let (k, _) = diff(&k, &[agent("%1", Some("s"), Status::Idle, true)], &[], t(3));
        assert_eq!(k["%1"].status, Status::Idle, "seen now");
        let (k, _) = diff(&HashMap::new(), &[agent("%2", Some("v"), Status::Working, true)], &[], t(0));
        let (k, _) = diff(&k, &[agent("%2", Some("v"), Status::Idle, true)], &[], t(1));
        assert_eq!(k["%2"].status, Status::Idle, "finished in front of the person");
        let (k, _) = diff(&HashMap::new(), &[agent("%3", Some("w"), Status::Blocked, false)], &[], t(0));
        let (k, _) = diff(&k, &[agent("%3", Some("w"), Status::Idle, false)], &[], t(1));
        assert_eq!(k["%3"].status, Status::Done, "a prompt answered from elsewhere");
    }

    #[test]
    fn an_agent_that_exits_ends_its_session() {
        let (k, _) = diff(
            &HashMap::new(),
            &[agent("%1", Some("a"), Status::Idle, true), agent("%2", Some("b"), Status::Working, true)],
            &[],
            t(0),
        );
        let (k, mut c) = diff(&k, &[], &["%1".into()], t(1));
        c.sort_by_key(|c| format!("{c:?}"));
        assert_eq!(
            c,
            [
                Change::Ended {
                    session_id: "a".into(),
                    how: How::AgentExited { final_status: Some(Status::Idle), terminal_closed: false }
                },
                Change::Ended { session_id: "b".into(), how: How::TabClosed },
            ],
            "back to a shell in a pane that stayed, or the pane closed"
        );
        assert!(k.is_empty());
    }

    #[test]
    fn another_agent_in_the_same_pane_is_a_new_session() {
        let (k, _) = diff(&HashMap::new(), &[agent("%1", Some("a"), Status::Idle, true)], &[], t(0));
        let (_, c) = diff(&k, &[agent("%1", Some("b"), Status::Idle, true)], &["%1".into()], t(1));
        assert_eq!(
            c,
            [
                Change::Ended {
                    session_id: "a".into(),
                    how: How::AgentExited { final_status: Some(Status::Idle), terminal_closed: false }
                },
                Change::Listed
            ]
        );
    }

    #[test]
    fn list_entries_carry_the_pane_and_say_tmux() {
        let a = agent("%7", None, Status::Idle, false);
        let info = session_info(&a, None);
        assert_eq!(info.session_id, "@p7");
        let meta: SessionMeta = ext::get_meta(info.meta.as_ref(), meta_key::SESSION).unwrap().unwrap();
        assert_eq!(meta.address.as_deref(), Some("p7"));
        assert_eq!((meta.named, meta.human_attached), (Some(false), Some(false)));
        assert_eq!(meta.opens_in.as_deref(), Some("tmux"));
        assert_eq!(meta.workspace.as_deref(), Some("work"));
    }

    #[test]
    fn config_picks_the_server_and_defaults() {
        let s = Settings::from_config(
            Config { socket: Some("a".into()), socket_path: Some("/s".into()), ..Default::default() },
            std::path::Path::new("/h"),
            "a",
        );
        assert_eq!(s.tmux.socket, Some(Socket::Path("/s".into())), "a path wins over a name");
        assert_eq!(s.session, "fleetlayer");
        assert_eq!(s.tmux.program, "tmux");
        let passwd =
            "root:x:0:0:root:/root:/bin/bash\nalice:x:1000:1000::/home/alice:/usr/bin/zsh\nbob:x:1:1::/:nologin";
        assert_eq!(passwd_shell(passwd, "alice").as_deref(), Some("/usr/bin/zsh"));
        assert_eq!(passwd_shell(passwd, "bob"), None);
        assert_eq!(passwd_shell(passwd, "carol"), None);
    }
}
