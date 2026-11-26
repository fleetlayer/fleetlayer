//! fleetlayer-summaries: summaries of every agent session on this machine. It follows the node's sessions, and when
//! one goes idle it reads the session's transcript and asks a headless Claude Code for a one-liner, a paragraph and
//! a page (the module in `summaries/`, a port of claude-summarizer). Each summary is written as a Markdown file, the
//! person's own copy to sync, back up or feed their own RAG, and published to the control plane for search and the
//! web UI.
//!
//! Config (its section of the node's config, all optional):
//! - `dir`: where the Markdown files go (default `~/.local/share/fleetlayer/summaries`).
//! - `publish`: what goes to the control plane: `full` (default), `sentence` (only the one-liner) or `none`.
//! - `claude`: the Claude Code binary for summary calls (default `claude`).
//! - `coordinator_socket`: the Claude account rotator's socket, to pick the account with the soonest reset (default
//!   `~/.claude/account-rotator/coordinator.sock` when it exists; otherwise Claude Code's own login).
//! - The engine's settings: `idle_seconds`, `min_gap_seconds`, `max_concurrent`, `input_cap_chars`,
//!   `daily_chunk_days`, `sweep`, `model`, `user_name`, `enabled` (see `fleetlayer_protocol::summary::SummariesConfig`).

mod summaries;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fleetlayer_plugin::{Handler, Peer, answer, params};
use fleetlayer_protocol::acp::{self, Capabilities, Implementation, InitializeRequest, InitializeResponse};
use fleetlayer_protocol::ext::{self, NodeMeta, PluginMeta, Roles, Status, meta_key};
use fleetlayer_protocol::rpc::RpcError;
use fleetlayer_protocol::summary::{
    AgentKind, SessionFacts, SessionSignal, SummariesConfig, SummaryEvent, SummaryRecord,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Publish {
    #[default]
    Full,
    Sentence,
    None,
}

#[derive(Debug, Default, Deserialize)]
struct Config {
    dir: Option<PathBuf>,
    #[serde(default)]
    publish: Publish,
    claude: Option<PathBuf>,
    coordinator_socket: Option<PathBuf>,
    #[serde(flatten)]
    engine: Option<Value>,
}

struct Settings {
    node: String,
    dir: PathBuf,
    publish: Publish,
}

#[derive(Default)]
struct Plugin {
    settings: Mutex<Option<Arc<Settings>>>,
    started: Mutex<bool>,
    pending_start: Mutex<Option<(SummariesConfig, summaries::SummariesEnv)>>,
    /// Where session signals go once the engine runs.
    signals: Mutex<Option<mpsc::Sender<SessionSignal>>>,
    /// The last status seen per session, so each change makes one signal.
    last: Mutex<HashMap<String, Status>>,
    /// The engine's handle, kept for as long as the plugin runs.
    handle: Mutex<Option<summaries::SummariesHandle>>,
}

struct Shared(Arc<Plugin>);

fn expand(home: &Path, p: PathBuf) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => p,
    }
}

#[async_trait]
impl Handler for Shared {
    async fn request(&self, peer: &Peer, method: &str, p: Value) -> Result<Value, RpcError> {
        let plugin = &self.0;
        match method {
            acp::method::INITIALIZE => {
                let req: InitializeRequest = params(p)?;
                let node: NodeMeta = ext::get_meta(req.client_capabilities.meta.as_ref(), meta_key::NODE)
                    .map_err(RpcError::invalid_params)?
                    .ok_or_else(|| RpcError::refused("fleetlayer-summaries runs under a Fleetlayer node"))?;
                let home = PathBuf::from(&node.home);
                // Only this user's own sessions: a mirror of another user's transcripts is theirs to summarise.
                summaries::set_owner_home(&node.home);
                let config: Config = serde_json::from_value(node.config.clone()).map_err(RpcError::invalid_params)?;
                let engine: SummariesConfig = match config.engine.clone() {
                    Some(v) => serde_json::from_value(v).map_err(RpcError::invalid_params)?,
                    None => SummariesConfig::default(),
                };
                let dir = expand(&home, config.dir.unwrap_or_else(|| home.join(".local/share/fleetlayer/summaries")));
                let rotator = home.join(".claude/account-rotator/coordinator.sock");
                let env = summaries::SummariesEnv {
                    state_dir: home.join(".local/state/fleetlayer/summaries"),
                    home: home.clone(),
                    claude_program: config.claude.unwrap_or_else(|| PathBuf::from("claude")),
                    coordinator_socket: config
                        .coordinator_socket
                        .map(|p| expand(&home, p))
                        .or_else(|| rotator.exists().then_some(rotator)),
                    time_zone: jiff::tz::TimeZone::system().iana_name().unwrap_or("UTC").to_string(),
                };
                *plugin.settings.lock().await =
                    Some(Arc::new(Settings { node: node.node.clone(), dir, publish: config.publish }));
                *plugin.pending_start.lock().await = Some((engine, env));
                let meta = PluginMeta {
                    protocol: fleetlayer_protocol::PROTOCOL,
                    roles: Roles { observer: Some(json!({})), ..Default::default() },
                };
                // Start once the node has had our answer: ask for its sessions from a separate task.
                let (plugin2, peer2) = (plugin.clone(), peer.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    start(plugin2, peer2).await;
                });
                answer(&InitializeResponse {
                    protocol_version: fleetlayer_protocol::ACP_PROTOCOL,
                    agent_capabilities: Capabilities {
                        meta: Some(ext::meta_with(meta_key::PLUGIN, &meta)),
                        ..Default::default()
                    },
                    agent_info: Some(Implementation {
                        name: "fleetlayer-summaries".into(),
                        title: Some("Summaries".into()),
                        version: env!("CARGO_PKG_VERSION").into(),
                    }),
                    auth_methods: vec![],
                })
            }
            ext::method::PING => Ok(json!({})),
            ext::method::SUMMARY_CATCHUP => {
                let id = p["sessionId"].as_str().ok_or_else(|| RpcError::invalid_params("sessionId"))?.to_string();
                let guard = plugin.handle.lock().await;
                let handle =
                    guard.as_ref().ok_or_else(|| RpcError::refused("the summaries engine isn't running yet"))?;
                let c = handle
                    .catchup(summaries::CatchupRequest { session_id: id, transcript: None })
                    .await
                    .map_err(|e| RpcError::refused(e.to_string()))?;
                answer(&c)
            }
            ext::method::SUMMARY_IMPORT => {
                let dir = p["dir"].as_str().ok_or_else(|| RpcError::invalid_params("dir"))?;
                let guard = plugin.handle.lock().await;
                let handle =
                    guard.as_ref().ok_or_else(|| RpcError::refused("the summaries engine isn't running yet"))?;
                let n = handle
                    .import_claude_summarizer(PathBuf::from(dir))
                    .await
                    .map_err(|e| RpcError::refused(e.to_string()))?;
                answer(&json!({"records": n}))
            }
            _ => Err(RpcError::method_not_found(method)),
        }
    }

    async fn notification(&self, _peer: &Peer, method: &str, p: Value) {
        let plugin = &self.0;
        if method == ext::method::SESSIONS_UPDATE
            && let Some(tx) = plugin.signals.lock().await.clone()
        {
            let signals = signals_from_update(&p, &mut *plugin.last.lock().await);
            for s in signals {
                let _ = tx.send(s).await;
            }
        }
    }
}

async fn start(plugin: Arc<Plugin>, peer: Peer) {
    {
        let mut started = plugin.started.lock().await;
        if *started {
            return;
        }
        *started = true;
    }
    let Some((engine, env)) = plugin.pending_start.lock().await.take() else { return };
    let Some(settings) = plugin.settings.lock().await.clone() else { return };
    let (sig_tx, sig_rx) = mpsc::channel(1024);
    let (ev_tx, mut ev_rx) = mpsc::channel(1024);
    *plugin.signals.lock().await = Some(sig_tx.clone());
    *plugin.handle.lock().await = Some(summaries::spawn(engine, env, sig_rx, ev_tx));

    let out = (settings.clone(), peer.clone());
    tokio::spawn(async move {
        let (settings, peer) = out;
        while let Some(event) = ev_rx.recv().await {
            if let SummaryEvent::Summary(r) = &event
                && let Err(e) = write_markdown(&settings.dir, &settings.node, r)
            {
                tracing::warn!("writing a summary for {}: {e}", r.session_id);
            }
            if let Some(e) = for_publishing(event, settings.publish) {
                peer.notify(ext::method::SUMMARY_PUBLISH, &json!({"event": e}));
            }
        }
    });

    match peer.request::<_, Value>(ext::method::SESSIONS_SUBSCRIBE, &json!({})).await {
        Ok(v) => {
            if let Some(m) = v["ownMessages"].as_array().and_then(|a| a.first()).and_then(Value::as_str) {
                summaries::set_auto_continue_message(m);
            }
            let update = json!({"change": "reset", "sessions": v["sessions"]});
            let signals = signals_from_update(&update, &mut *plugin.last.lock().await);
            for s in signals {
                let _ = sig_tx.send(s).await;
            }
        }
        Err(e) => tracing::error!("following the node's sessions failed ({e}); is sessions.watch granted?"),
    }
}

fn agent(meta: &Value) -> Option<AgentKind> {
    match meta["harness"].as_str()? {
        "claude" => Some(AgentKind::Claude),
        "codex" => Some(AgentKind::Codex),
        _ => None,
    }
}

/// The signals a node session update means: a status signal when a running session's status changed (or it's new),
/// a closed signal when it ended.
fn signals_from_update(update: &Value, last: &mut HashMap<String, Status>) -> Vec<SessionSignal> {
    let sessions: Vec<&Value> = match update["change"].as_str() {
        Some("reset") => update["sessions"].as_array().map(|a| a.iter().collect()).unwrap_or_default(),
        Some("updated") => vec![&update["session"]],
        _ => vec![],
    };
    let now = jiff::Timestamp::now();
    let mut out = vec![];
    for s in sessions {
        let Some(id) = s["sessionId"].as_str() else { continue };
        if id.starts_with('@') {
            continue; // not named yet: its transcript can't be found
        }
        let meta = &s["meta"];
        let Some(agent) = agent(meta) else { continue };
        if !s["ended"].is_null() {
            if last.remove(id).is_some() {
                out.push(SessionSignal::Closed { session_id: id.into(), at: now });
            }
            continue;
        }
        let status: Status = serde_json::from_value(meta["status"].clone()).unwrap_or_default();
        if last.get(id) == Some(&status) {
            continue;
        }
        last.insert(id.into(), status);
        out.push(SessionSignal::Status {
            session: SessionFacts {
                session_id: id.into(),
                agent,
                tab: meta["tab"].as_str().map(str::to_string),
                cwd: s["cwd"].as_str().map(str::to_string),
                transcript: None,
            },
            status,
            at: now,
        });
    }
    out
}

/// What goes to the control plane under the `publish` setting.
fn for_publishing(event: SummaryEvent, publish: Publish) -> Option<SummaryEvent> {
    match (publish, event) {
        (Publish::None, _) | (_, SummaryEvent::Catchup(_)) => None,
        (Publish::Full, e) => Some(e),
        (Publish::Sentence, SummaryEvent::Summary(mut r)) => {
            r.paragraph.clear();
            r.page.clear();
            Some(SummaryEvent::Summary(r))
        }
        (Publish::Sentence, e) => Some(e),
    }
}

fn period(r: &SummaryRecord) -> (&'static str, &str) {
    use fleetlayer_protocol::summary::SummaryPeriod::*;
    match &r.period {
        Hour(k) => ("hour", k),
        Day(k) => ("day", k),
        Week(k) => ("week", k),
    }
}

/// The record as Markdown, in claude-summarizer's layout (`## Sentence`, `## Paragraph`, `## Page`) with front matter.
pub fn markdown(node: &str, r: &SummaryRecord) -> String {
    let (kind, key) = period(r);
    let q = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let mut fm = vec![
        format!("session: {}", r.session_id),
        format!("node: {}", q(node)),
        format!(
            "agent: {}",
            serde_json::to_value(r.agent).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
        ),
    ];
    if let Some(t) = &r.tab {
        fm.push(format!("tab: {}", q(t)));
    }
    if let Some(d) = &r.dir {
        fm.push(format!("dir: {}", q(d)));
    }
    fm.push(format!("period: {kind} {key}"));
    fm.push(format!("activity_from: {}", r.activity_from));
    fm.push(format!("activity_to: {}", r.activity_to));
    fm.push(format!("written_at: {}", r.written_at));
    format!(
        "---\n{}\n---\n\n## Sentence\n\n{}\n\n## Paragraph\n\n{}\n\n## Page\n\n{}\n",
        fm.join("\n"),
        r.sentence.trim(),
        r.paragraph.trim(),
        r.page.trim()
    )
}

/// `<dir>/<session id>/<period key>.md`, replaced when the same period is summarised again. Written atomically.
fn write_markdown(dir: &Path, node: &str, r: &SummaryRecord) -> std::io::Result<PathBuf> {
    let (kind, key) = period(r);
    let safe = |s: &str| {
        s.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect::<String>()
    };
    let folder = dir.join(safe(&r.session_id));
    std::fs::create_dir_all(&folder)?;
    let name = if kind == "hour" { format!("{}.md", safe(key)) } else { format!("{kind}-{}.md", safe(key)) };
    let path = folder.join(name);
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, markdown(node, r))?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
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

#[cfg(test)]
mod plugin_tests {
    use super::*;
    use fleetlayer_protocol::summary::{RecordOrigin, SummaryPeriod};

    fn record() -> SummaryRecord {
        let t: jiff::Timestamp = "2026-11-26T01:00:00Z".parse().unwrap();
        SummaryRecord {
            session_id: "s1".into(),
            agent: AgentKind::Claude,
            period: SummaryPeriod::Hour("2026-11-26T14".into()),
            activity_from: t,
            activity_to: t,
            written_at: t,
            sentence: "Ported the summaries.".into(),
            paragraph: "A paragraph.".into(),
            page: "A page.".into(),
            tab: Some("fleetlayer".into()),
            dir: Some("/home/a/code/fleetlayer".into()),
            origin: RecordOrigin::Node,
        }
    }

    #[test]
    fn a_summary_becomes_a_markdown_file_in_its_sessions_folder() {
        let dir = tempfile::tempdir().unwrap();
        let p = write_markdown(dir.path(), "agents", &record()).unwrap();
        assert_eq!(p, dir.path().join("s1/2026-11-26T14.md"));
        let text = std::fs::read_to_string(p).unwrap();
        assert!(text.starts_with("---\nsession: s1\nnode: \"agents\"\nagent: claude\ntab: \"fleetlayer\""));
        assert!(text.contains("period: hour 2026-11-26T14"));
        assert!(text.contains(
            "\n## Sentence\n\nPorted the summaries.\n\n## Paragraph\n\nA paragraph.\n\n## Page\n\nA page.\n"
        ));
    }

    #[test]
    fn publishing_follows_the_setting() {
        let e = SummaryEvent::Summary(record());
        assert!(for_publishing(e.clone(), Publish::None).is_none());
        let Some(SummaryEvent::Summary(r)) = for_publishing(e.clone(), Publish::Sentence) else { panic!() };
        assert_eq!((r.sentence.as_str(), r.paragraph.as_str(), r.page.as_str()), ("Ported the summaries.", "", ""));
        assert_eq!(for_publishing(e.clone(), Publish::Full), Some(e));
    }

    #[test]
    fn session_updates_become_signals_once_per_status() {
        let mut last = HashMap::new();
        let s = |status: &str, ended: bool| json!({"sessionId":"s1","cwd":"/w","meta":{"harness":"claude","tab":"t","status":status},"ended": if ended { json!({"at":"x"}) } else { Value::Null }});
        let reset =
            json!({"change":"reset","sessions":[s("working", false), {"sessionId":"@p2","meta":{"harness":"claude"}}]});
        assert_eq!(signals_from_update(&reset, &mut last).len(), 1, "an unnamed session is skipped");
        assert!(signals_from_update(&json!({"change":"updated","session": s("working", false)}), &mut last).is_empty());
        let idle = signals_from_update(&json!({"change":"updated","session": s("idle", false)}), &mut last);
        assert!(matches!(idle.as_slice(), [SessionSignal::Status { status: Status::Idle, .. }]));
        let closed = signals_from_update(&json!({"change":"updated","session": s("idle", true)}), &mut last);
        assert!(matches!(closed.as_slice(), [SessionSignal::Closed { .. }]));
    }
}
