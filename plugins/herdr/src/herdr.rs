//! herdr's CLI and socket, in herdr's own terms: `agent list`, `tab list`, the `events.subscribe` stream, `tab create`,
//! `tab close`, `agent send-keys` and `agent prompt`. Commands run through herdr's CLI (argv, never a shell). The client
//! contract this follows is in herdr's socket-api docs ("Client contract").
//!
//! First written for Quietdeck (`quietdeck-node/src/orchestrator/herdr.rs`) by the same author, and changed here to
//! speak Fleetlayer's types.

use std::path::{Path, PathBuf};

use fleetlayer_protocol::ext::{self, How, OpenSession, Status};
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    result: T,
}

#[derive(Debug, Deserialize)]
pub struct AgentList {
    agents: Vec<Agent>,
}

#[derive(Debug, Deserialize)]
pub struct Agent {
    agent: Option<String>,
    agent_session: Option<AgentSession>,
    agent_status: Option<String>,
    cwd: Option<String>,
    pane_id: String,
    tab_id: Option<String>,
    workspace_id: Option<String>,
    state_changed_at_unix_ms: Option<i64>,
    /// Newer herdr puts the tab's label on the agent record.
    tab_label: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AgentSession {
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TabList {
    tabs: Vec<Tab>,
}

#[derive(Debug, Deserialize)]
struct Tab {
    tab_id: String,
    label: Option<String>,
}

/// One agent session as herdr shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The harness's own session ID; None while it's starting, or when herdr can't read it.
    pub session_id: Option<String>,
    pub harness: String,
    pub pane: String,
    pub tab_id: Option<String>,
    pub tab: Option<String>,
    pub workspace: Option<String>,
    pub cwd: Option<String>,
    pub status: Status,
    pub status_changed_at: Timestamp,
}

/// Why herdr couldn't be run, as a sentence: a missing program says how to point the plugin at it.
pub fn spawn_error(herdr: &str, e: &std::io::Error) -> String {
    if e.kind() != std::io::ErrorKind::NotFound {
        return format!("running {herdr}: {e}");
    }
    let fix = "give the herdr plugin `config: { program: <full path to herdr> }` in node.yaml";
    if herdr.contains('/') {
        format!("there's no herdr at {herdr}: {fix}")
    } else {
        let path = std::env::var("PATH").unwrap_or_default();
        format!("{herdr} isn't on the node's PATH ({path}): {fix}")
    }
}

async fn run_json<T: serde::de::DeserializeOwned>(herdr: &str, args: &[&str]) -> anyhow::Result<T> {
    let out = tokio::process::Command::new(herdr)
        .args(args)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!(spawn_error(herdr, &e)))?;
    if !out.status.success() {
        anyhow::bail!("herdr {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    let env: Envelope<T> =
        serde_json::from_slice(&out.stdout).map_err(|e| anyhow::anyhow!("reading herdr {}: {e}", args.join(" ")))?;
    Ok(env.result)
}

pub fn parse_status(s: Option<&str>) -> Status {
    match s {
        Some("idle") => Status::Idle,
        Some("done") => Status::Done,
        Some("working") => Status::Working,
        Some("blocked") => Status::Blocked,
        _ => Status::Unknown,
    }
}

/// Every agent herdr runs on this machine. Panes that run no agent (a shell) are left out.
pub async fn snapshot(herdr: &str) -> anyhow::Result<Vec<Session>> {
    let agents: AgentList = run_json(herdr, &["agent", "list"]).await?;
    let tabs: TabList = run_json(herdr, &["tab", "list"]).await.unwrap_or(TabList { tabs: vec![] });
    Ok(join(agents, tabs))
}

fn harness(agent: Option<&str>) -> Option<String> {
    match agent {
        None | Some("") | Some("shell") => None,
        Some(a) => Some(a.to_string()),
    }
}

pub fn join(agents: AgentList, tabs: TabList) -> Vec<Session> {
    agents
        .agents
        .into_iter()
        .filter_map(|a| {
            let harness = harness(a.agent.as_deref())?;
            let tab = a.tab_label.or_else(|| {
                let id = a.tab_id.as_deref()?;
                tabs.tabs.iter().find(|t| t.tab_id == id).and_then(|t| t.label.clone())
            });
            Some(Session {
                session_id: a.agent_session.and_then(|s| s.value),
                harness,
                pane: a.pane_id,
                tab_id: a.tab_id,
                tab,
                workspace: a.workspace_id,
                cwd: a.cwd,
                status: parse_status(a.agent_status.as_deref()),
                status_changed_at: a
                    .state_changed_at_unix_ms
                    .and_then(|ms| Timestamp::from_millisecond(ms).ok())
                    .unwrap_or_else(Timestamp::now),
            })
        })
        .collect()
}

/// herdr's link that focuses a tab and brings its terminal forward, on this machine's herdr or a saved one.
pub fn open_url(machine: &str, tab: &str) -> String {
    let query = form_urlencoded::Serializer::new(String::new())
        .append_pair("machine", machine)
        .append_pair("tab", tab)
        .finish();
    // A literal + is already %2B, so every + left stands for a space.
    format!("herdr://open?{}", query.replace('+', "%20"))
}

/// The auto-continue message herdr's config.toml sets, if any (`[session.auto_continue] message`).
pub fn auto_continue_message(home: &Path) -> Option<String> {
    let dir = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
    let text = std::fs::read_to_string(dir.join("herdr/config.toml")).ok()?;
    auto_continue_from(&text)
}

fn auto_continue_from(toml_text: &str) -> Option<String> {
    let table: toml::Table = toml::from_str(toml_text).ok()?;
    table.get("session")?.get("auto_continue")?.get("message")?.as_str().map(str::to_string)
}

// ---------------------------------------------------------------- events

/// Status changes for every pane, the two close events, and the lifecycle events after which `agent list` reads
/// differently (a new agent, its session ID arriving, a rename or a move).
const SUBSCRIPTIONS: &[&str] = &[
    "pane.agent_status_changed",
    "tab.closed",
    "pane.agent_exited",
    "pane.agent_detected",
    "pane.updated",
    "pane.moved",
    "tab.renamed",
    "tab.moved",
];

/// One herdr event, in the plugin's terms.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Status {
        pane: String,
        status: Status,
    },
    /// A tab closed, with the session IDs that were running in it.
    TabClosed {
        sessions: Vec<String>,
    },
    AgentExited {
        pane: Option<String>,
        session: Option<String>,
        how: How,
    },
    Updated {
        pane: String,
        session: Option<String>,
        tab: Option<String>,
        cwd: Option<String>,
    },
    /// Something `agent list` shows differently now.
    Changed,
    Other,
}

fn session_value(v: &Value) -> Option<String> {
    v.get("agent_session")?.get("value")?.as_str().map(str::to_string)
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

// herdr names events with dots when subscribing and with underscores in some payloads; both are accepted.
pub fn parse_event(line: &str) -> Event {
    let Ok(v) = serde_json::from_str::<Value>(line) else { return Event::Other };
    let data = v.get("data").cloned().unwrap_or(Value::Null);
    let name = v.get("event").and_then(Value::as_str).unwrap_or_default().replace('.', "_");
    match name.as_str() {
        "pane_agent_status_changed" => match str_field(&data, "pane_id") {
            Some(pane) => {
                Event::Status { pane, status: parse_status(data.get("agent_status").and_then(Value::as_str)) }
            }
            None => Event::Other,
        },
        "tab_closed" => Event::TabClosed {
            sessions: data
                .get("agents")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(session_value).collect())
                .unwrap_or_default(),
        },
        "pane_agent_exited" => Event::AgentExited {
            pane: str_field(&data, "pane_id"),
            session: session_value(&data),
            how: How::AgentExited {
                final_status: data.get("final_status").and_then(Value::as_str).map(|s| parse_status(Some(s))),
                terminal_closed: data.get("pane_exited").and_then(Value::as_bool).unwrap_or(false),
            },
        },
        "pane_updated" => {
            let pane = data.get("pane").cloned().unwrap_or(Value::Null);
            match str_field(&pane, "pane_id") {
                Some(id) => Event::Updated {
                    pane: id,
                    session: session_value(&pane),
                    tab: str_field(&pane, "tab_label"),
                    cwd: str_field(&pane, "cwd"),
                },
                None => Event::Changed,
            }
        }
        "pane_agent_detected" | "pane_moved" | "tab_renamed" | "tab_moved" => Event::Changed,
        _ => Event::Other,
    }
}

/// herdr's API socket, as `herdr status server` reports it (it knows named sessions and dev builds).
async fn socket_path(herdr: &str) -> anyhow::Result<PathBuf> {
    let out = tokio::process::Command::new(herdr)
        .args(["status", "server"])
        .output()
        .await
        .map_err(|e| anyhow::anyhow!(spawn_error(herdr, &e)))?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|l| l.strip_prefix("socket:").map(|p| PathBuf::from(p.trim())))
        .filter(|_| text.lines().any(|l| l.trim() == "status: running"))
        .ok_or_else(|| anyhow::anyhow!("herdr isn't running"))
}

/// Why a subscription failed.
#[derive(Debug)]
pub enum SubscribeError {
    /// herdr runs but won't stream every pane's events (an older herdr).
    Refused(String),
    /// herdr isn't reachable now.
    Unreachable(String),
}

/// An open subscription.
pub struct Subscription {
    lines: tokio::io::Lines<BufReader<tokio::net::UnixStream>>,
}

impl Subscription {
    pub async fn open(herdr: &str) -> Result<Subscription, SubscribeError> {
        let socket = socket_path(herdr).await.map_err(|e| SubscribeError::Unreachable(e.to_string()))?;
        let unreachable = |e: std::io::Error| SubscribeError::Unreachable(e.to_string());
        let mut stream = tokio::net::UnixStream::connect(&socket).await.map_err(unreachable)?;
        let subs: Vec<Value> = SUBSCRIPTIONS.iter().map(|t| serde_json::json!({"type": t})).collect();
        let req = serde_json::json!({"id": "fleetlayer-herdr", "method": "events.subscribe",
            "params": {"subscriptions": subs}});
        stream.write_all(format!("{req}\n").as_bytes()).await.map_err(unreachable)?;
        let mut lines = BufReader::new(stream).lines();
        let first = lines
            .next_line()
            .await
            .map_err(unreachable)?
            .ok_or_else(|| SubscribeError::Unreachable("herdr hung up".into()))?;
        let v: Value = serde_json::from_str(&first).unwrap_or(Value::Null);
        if v.pointer("/result/type").and_then(Value::as_str) == Some("subscription_started") {
            return Ok(Subscription { lines });
        }
        let msg = v.pointer("/error/message").and_then(Value::as_str).unwrap_or(&first).to_string();
        Err(SubscribeError::Refused(msg))
    }

    /// The next event; None when herdr closed the stream (it stopped, restarted or handed off).
    pub async fn next(&mut self) -> std::io::Result<Option<Event>> {
        Ok(self.lines.next_line().await?.map(|l| parse_event(&l)))
    }
}

// ---------------------------------------------------------------- acting on sessions

/// What an action on a session can fail with.
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    /// The session isn't running here.
    NotHere(String),
    /// Running, but waiting on a person, so it can't take input now.
    Busy(String),
    /// Anything else, as a sentence.
    Other(String),
}

/// `herdr tab create --agent …`: answers once the agent takes input, with the new pane and the session ID if the
/// agent reported it already, or with herdr's own message for a start-up screen or a failed start.
pub async fn create_tab(herdr: &str, open: &OpenSession) -> Result<(String, Option<String>), String> {
    let run = tokio::process::Command::new(herdr).args(tab_create(open)).output();
    let out = tokio::time::timeout(std::time::Duration::from_secs(120), run)
        .await
        .map_err(|_| "herdr didn't answer within two minutes".to_string())?
        .map_err(|e| spawn_error(herdr, &e))?;
    if !out.status.success() {
        return Err(herdr_error(&out.stderr));
    }
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("reading herdr's answer: {e}"))?;
    let tab_id = v.pointer("/result/tab/tab_id").and_then(Value::as_str);
    let pane_id = v.pointer("/result/root_pane/pane_id").and_then(Value::as_str);
    match (tab_id, pane_id) {
        (Some(_), Some(p)) => Ok((p.into(), v.pointer("/result/agent").and_then(session_value))),
        _ => Err("herdr's answer named no tab or pane".into()),
    }
}

/// herdr's error as a sentence: its message, and for a failed start the agent's last line.
pub fn herdr_error(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr).trim().to_string();
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return text };
    let Some(err) = v.get("error") else { return text };
    let message = err.get("message").and_then(Value::as_str).unwrap_or(&text).to_string();
    let last = err
        .get("output")
        .and_then(Value::as_array)
        .and_then(|lines| lines.iter().rev().filter_map(Value::as_str).map(str::trim).find(|l| !l.is_empty()));
    match last {
        Some(l) if err.get("code").and_then(Value::as_str) == Some("agent_start_failed") => format!("{message}: {l}"),
        _ => message,
    }
}

/// `herdr tab create` arguments: herdr runs the agent itself, with the agent's arguments after `--`.
pub fn tab_create(open: &OpenSession) -> Vec<String> {
    let mut a: Vec<String> =
        ["tab", "create", "--label", &open.label, "--cwd", &open.cwd, "--no-focus", "--agent", &open.harness, "--"]
            .map(String::from)
            .into();
    a.extend(open.args.iter().cloned());
    a
}

/// Press Esc in a pane: the agent stops its turn.
pub async fn interrupt(herdr: &str, pane: &str) -> Result<(), Failure> {
    run_plain(herdr, &["agent", "send-keys", pane, "esc"]).await
}

/// Press keys and type text in a pane, one step at a time with a moment between, so the agent's menu redraws before
/// the next key: `herdr agent send-keys` for a key, `herdr pane send-text` for literal text (no Enter).
pub async fn keys(herdr: &str, pane: &str, steps: &[ext::KeyStep]) -> Result<(), Failure> {
    for (i, step) in steps.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        }
        match step {
            ext::KeyStep::Key(k) => run_plain(herdr, &["agent", "send-keys", pane, k]).await?,
            ext::KeyStep::Text(t) => run_plain(herdr, &["pane", "send-text", pane, t]).await?,
        }
    }
    Ok(())
}

/// What a pane shows now, as plain text: the visible screen, its last `lines` lines (herdr can't read history while
/// an agent is blocked at a menu, and the menu is on screen anyway).
pub async fn screen(herdr: &str, pane: &str, lines: u64) -> Result<String, Failure> {
    let n = lines.to_string();
    let out = tokio::process::Command::new(herdr)
        .args(["agent", "read", pane, "--source", "visible", "--lines", &n])
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(std::time::Duration::from_secs(5), out)
        .await
        .map_err(|_| Failure::Other(format!("{herdr} agent read didn't finish within 5 seconds")))?
        .map_err(|e| Failure::Other(spawn_error(herdr, &e)))?;
    if !out.status.success() {
        return Err(classify(herdr_error(&out.stderr)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Close a tab.
pub async fn close_tab(herdr: &str, tab_id: &str) -> Result<(), Failure> {
    run_plain(herdr, &["tab", "close", tab_id]).await
}

async fn run_plain(herdr: &str, args: &[&str]) -> Result<(), Failure> {
    let out = tokio::process::Command::new(herdr)
        .args(args)
        .output()
        .await
        .map_err(|e| Failure::Other(spawn_error(herdr, &e)))?;
    if out.status.success() { Ok(()) } else { Err(classify(herdr_error(&out.stderr))) }
}

fn classify(err: String) -> Failure {
    let lower = err.to_lowercase();
    if lower.contains("not found") || lower.contains("no such pane") || lower.contains("unknown pane") {
        Failure::NotHere(err)
    } else if lower.contains("blocked") || lower.contains("waiting for") {
        Failure::Busy(err)
    } else {
        Failure::Other(err)
    }
}

/// `herdr agent prompt`: types the text into the pane and submits it, once the agent takes input.
pub async fn prompt(herdr: &str, pane: &str, text: &str) -> Result<(), Failure> {
    // Typing takes milliseconds; a herdr that hangs is cut off at 3 seconds and its process killed.
    let run = tokio::process::Command::new(herdr).args(["agent", "prompt", pane, text]).kill_on_drop(true).output();
    let out = match tokio::time::timeout(std::time::Duration::from_secs(3), run).await {
        Ok(r) => r.map_err(|e| Failure::Other(spawn_error(herdr, &e)))?,
        Err(_) => return Err(Failure::Other(format!("{herdr} agent prompt didn't finish within 3 seconds"))),
    };
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let err = if err.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_string() } else { err };
    Err(classify(err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_herdr_says_how_to_point_at_it() {
        let gone = std::io::Error::from(std::io::ErrorKind::NotFound);
        let e = spawn_error("herdr", &gone);
        assert!(e.starts_with("herdr isn't on the node's PATH ("), "{e}");
        assert!(e.ends_with("give the herdr plugin `config: { program: <full path to herdr> }` in node.yaml"), "{e}");
        assert!(spawn_error("/opt/herdr", &gone).starts_with("there's no herdr at /opt/herdr: "));
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(spawn_error("herdr", &denied).starts_with("running herdr: "));
    }

    #[test]
    fn agents_join_their_tab_labels_and_unnamed_agents_are_kept() {
        let agents: Envelope<AgentList> = serde_json::from_value(serde_json::json!({"result": {"agents": [
            {"agent": "claude", "agent_session": {"value": "s1"}, "agent_status": "blocked", "cwd": "/w",
             "pane_id": "w1:p1", "tab_id": "w1:t1", "workspace_id": "w1", "state_changed_at_unix_ms": 1795402660205i64},
            {"agent": "codex", "agent_status": "idle", "pane_id": "w1:p2", "tab_id": "w1:t2"},
            {"agent": "shell", "agent_session": {"value": "x"}, "pane_id": "w1:p3"}
        ]}}))
        .unwrap();
        let tabs: Envelope<TabList> =
            serde_json::from_value(serde_json::json!({"result": {"tabs": [{"tab_id": "w1:t1", "label": "billing"}]}}))
                .unwrap();
        let s = join(agents.result, tabs.result);
        assert_eq!(s.len(), 2, "a shell is left out; an agent with no session ID yet is kept");
        assert_eq!(s[0].tab.as_deref(), Some("billing"));
        assert_eq!(s[0].status, Status::Blocked);
        assert_eq!(s[1].session_id, None);
        assert_eq!(s[1].harness, "codex");
    }

    #[test]
    fn the_open_link_names_the_machine_and_tab() {
        assert_eq!(open_url("mac", "esign cert+1"), "herdr://open?machine=mac&tab=esign%20cert%2B1");
    }

    // Lines as a herdr server sent them (23 November 2026).
    #[test]
    fn herdr_events_parse() {
        let status = r#"{"data":{"agent":"claude","agent_status":"working","pane_id":"w1:p3","tab_id":"w1:t3","tab_label":"qd probe","workspace_id":"w1"},"event":"pane.agent_status_changed"}"#;
        assert_eq!(parse_event(status), Event::Status { pane: "w1:p3".into(), status: Status::Working });

        let closed = r#"{"data":{"agents":[{"agent":"claude","agent_session":{"agent":"claude","kind":"id","source":"herdr:claude","value":"4f20528f"},"name":"qd-probe","pane_id":"w1:p3"}],"label":"qd probe","tab_id":"w1:t3","type":"tab_closed","workspace_id":"w1"},"event":"tab_closed"}"#;
        assert_eq!(parse_event(closed), Event::TabClosed { sessions: vec!["4f20528f".into()] });

        let exited = r#"{"data":{"agent":"claude","final_status":"done","name":"qd-probe","pane_exited":false,"pane_id":"w1:p2","tab_id":"w1:t2","tab_label":"qd probe","type":"pane_agent_exited","workspace_id":"w1"},"event":"pane_agent_exited"}"#;
        assert_eq!(
            parse_event(exited),
            Event::AgentExited {
                pane: Some("w1:p2".into()),
                session: None,
                how: How::AgentExited { final_status: Some(Status::Done), terminal_closed: false }
            }
        );

        let updated = r#"{"data":{"pane":{"agent":"claude","agent_session":{"agent":"claude","kind":"id","source":"herdr:claude","value":"4f20528f"},"agent_status":"idle","cwd":"/Users/x/code/q","pane_id":"w1:p3","tab_id":"w1:t3","tab_label":"qd probe","workspace_id":"w1"},"type":"pane_updated"},"event":"pane_updated"}"#;
        assert_eq!(
            parse_event(updated),
            Event::Updated {
                pane: "w1:p3".into(),
                session: Some("4f20528f".into()),
                tab: Some("qd probe".into()),
                cwd: Some("/Users/x/code/q".into())
            }
        );
        assert_eq!(parse_event(r#"{"event":"tab_renamed","data":{"type":"tab_renamed"}}"#), Event::Changed);
        assert_eq!(parse_event(r#"{"event":"some_new_kind","data":{}}"#), Event::Other);
        assert_eq!(parse_event("not json"), Event::Other);
    }

    /// herdr can't start an agent with a first prompt that has newlines in it, so the prompt never goes on the command
    /// line: it's typed with `herdr agent prompt` once the agent takes input, where newlines stay in one turn.
    #[test]
    fn the_first_prompt_never_goes_on_the_command_line() {
        let open = OpenSession {
            label: "t".into(),
            cwd: "/w".into(),
            harness: "claude".into(),
            args: vec![],
            prompt: Some("Message from planning (laptop) via Fleetlayer:\n\nfirst line\nsecond line".into()),
        };
        let args = tab_create(&open);
        assert!(args.iter().all(|a| !a.contains("Message from") && !a.contains('\n')), "{args:?}");
        assert_eq!(args.last().map(String::as_str), Some("--"));
    }

    #[test]
    fn a_tab_runs_the_agent_with_its_own_arguments_after_the_separator() {
        let open = OpenSession {
            label: "esign cert".into(),
            cwd: "/w".into(),
            harness: "claude".into(),
            args: vec!["--name".into(), "esign cert".into()],
            prompt: None,
        };
        assert_eq!(
            tab_create(&open),
            [
                "tab",
                "create",
                "--label",
                "esign cert",
                "--cwd",
                "/w",
                "--no-focus",
                "--agent",
                "claude",
                "--",
                "--name",
                "esign cert"
            ]
        );
    }

    #[test]
    fn the_auto_continue_message_comes_from_herdrs_config() {
        let text =
            "[ui]\nconfirm_close = true\n\n[session.auto_continue]\nenabled = true\nmessage = \"Go on ({reason})\"\n";
        assert_eq!(auto_continue_from(text).as_deref(), Some("Go on ({reason})"));
        assert_eq!(auto_continue_from("[session.auto_continue]\nenabled = true\n"), None);
        assert_eq!(auto_continue_from("not = = toml"), None);
    }

    #[test]
    fn herdr_errors_read_as_sentences_and_sort_into_kinds() {
        let failed = br#"{"error":{"code":"agent_start_failed","message":"agent process exited before becoming interactive","output":["claude --resume 0000","No conversation found with session ID: 0000",""],"tab_closed":true},"id":"cli:tab:create"}"#;
        assert_eq!(
            herdr_error(failed),
            "agent process exited before becoming interactive: No conversation found with session ID: 0000"
        );
        assert_eq!(herdr_error(b"plain words"), "plain words");
        assert!(matches!(classify("pane w1:p9 not found".into()), Failure::NotHere(_)));
        assert!(matches!(classify("the agent is blocked on a prompt".into()), Failure::Busy(_)));
        assert!(matches!(classify("something else".into()), Failure::Other(_)));
    }
}
